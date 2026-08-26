//! Streaming artifact download with bounds and cancellation (packet 37).
//!
//! Security ordering (unchanged from the previous buffered shape, now
//! enforced over a stream): bytes go to a TEMP path only, the sha256 is
//! computed over the SAME bytes as they are written and compared BEFORE
//! anything is extracted or renamed into the cache. Nothing is extracted,
//! executed, or published to the cache on a mismatch — the temp dir is
//! removed and the caller errors.
//!
//! Why a module: `ensure_artifact` previously buffered the whole artifact
//! in RAM (~340 MB peak for the browser tarball) with `reqwest::get` and
//! NO timeout of any kind — a stalled body held the job slot (and thus
//! SIGTERM, via the drain) forever. The constants below bound every phase
//! of the transfer; the sha is fed incrementally so peak RSS is one chunk.

use anyhow::{anyhow, Context};
use sha2::{Digest, Sha256};
use std::path::Path;
use tokio::io::AsyncWriteExt;

/// Connect timeout for artifact fetches.
///
/// DNS + TCP + TLS to an R2 edge. A server that cannot complete the
/// handshake in 10 s is not going to serve a 340 MB body usefully, and
/// the reconnect loop can retry a different edge.
pub(crate) const ARTIFACT_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// HARD ceiling on a single artifact download, start to finish.
///
/// The largest real artifact today is the browser tarball (~340 MB). On a
/// slow domestic uplink (5 Mbps sustained — worse than most, better than
/// satellite) that transfers in ~10 minutes; 30 minutes leaves 3x margin
/// without allowing a stalled transfer to hold the job slot for hours.
pub(crate) const ARTIFACT_TOTAL_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(30 * 60);

/// Per-read timeout: how long the next body chunk may take to arrive.
///
/// This is the trickle guard the total timeout only bounds lazily: any
/// 60-second gap in the body aborts the read immediately. A healthy
/// transfer on a bad link pauses in sub-second bursts; a minute of
/// silence means the transfer is effectively dead.
pub(crate) const ARTIFACT_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Hard byte ceiling for one artifact download.
///
/// Content-Length can lie (it is peer-supplied), so the ceiling is
/// enforced as the stream lands, not up front. Real artifacts: payloads
/// are tens of MB, the browser ~340 MB, and a future browser with full
/// Chrome + headless shell might reach ~1 GB. 2 GiB bounds every honest
/// artifact today with generations of margin, while capping a hostile or
/// buggy server's ability to fill the operator's disk via the temp path.
pub(crate) const ARTIFACT_MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Ceiling on bytes extracted from an artifact tarball.
///
/// tar.gz is a decompression-bomb vector. The extracted tree of the
/// browser artifact is ~700 MB (~2x its compressed size); 4 GiB covers
/// every honest artifact with wide margin while bounding the bomb to one
/// removable temp dir instead of a full disk.
pub(crate) const ARTIFACT_MAX_EXTRACTED_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// Build the artifact-download client with the timeouts above baked in.
///
/// One client per supervisor process (connection reuse); per-request
/// `.timeout()` layers the total bound on top.
pub(crate) fn artifact_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(ARTIFACT_CONNECT_TIMEOUT)
        .read_timeout(ARTIFACT_READ_TIMEOUT)
        .build()
        .expect("reqwest client with plain timeouts cannot fail to build")
}

/// Stream `get_url` to `dest`, hashing as we write. Returns the sha256 of
/// the bytes actually written. Enforces [`ARTIFACT_MAX_BYTES`] against
/// both a lying Content-Length and the real stream length. Cancellation:
/// every await in here is inside the caller's task, so dropping the
/// future (cancel signal / shutdown) stops the transfer immediately.
pub(crate) async fn download_to_file_hashed(
    client: &reqwest::Client,
    get_url: &str,
    dest: &Path,
    kind: &str,
) -> anyhow::Result<String> {
    let response = client
        .get(get_url)
        .timeout(ARTIFACT_TOTAL_TIMEOUT)
        .send()
        .await
        .with_context(|| format!("{kind} download request failed"))?
        .error_for_status()
        .with_context(|| format!("{kind} download returned non-2xx"))?;

    if let Some(len) = response.content_length() {
        if len > ARTIFACT_MAX_BYTES {
            return Err(anyhow!(
                "{kind} content-length {len} exceeds the {} byte artifact ceiling",
                ARTIFACT_MAX_BYTES
            ));
        }
    }

    let mut file = tokio::fs::File::create(dest)
        .await
        .with_context(|| format!("create temp artifact file {}", dest.display()))?;
    let mut hasher = Sha256::new();
    let mut stream = response.bytes_stream();
    let mut written: u64 = 0;
    loop {
        let next = tokio::time::timeout(ARTIFACT_READ_TIMEOUT, stream.next())
            .await
            .map_err(|_| {
                anyhow!("{kind} download stalled: no body data for {ARTIFACT_READ_TIMEOUT:?}")
            })?;
        let chunk = match next {
            Some(chunk) => chunk.with_context(|| format!("{kind} body read failed"))?,
            None => break, // body complete
        };
        written += chunk.len() as u64;
        if written > ARTIFACT_MAX_BYTES {
            // Remove the oversized temp file before failing — the caller
            // cannot be trusted to know how far we got.
            let _ = tokio::fs::remove_file(dest).await;
            return Err(anyhow!(
                "{kind} download exceeded the {} byte artifact ceiling",
                ARTIFACT_MAX_BYTES
            ));
        }
        hasher.update(&chunk);
        file.write_all(&chunk)
            .await
            .with_context(|| format!("write {kind} body to {}", dest.display()))?;
    }
    file.flush().await.ok();
    drop(file);
    Ok(hex_lower(&hasher.finalize()))
}

fn hex_lower(digest: &[u8]) -> String {
    let mut out = String::with_capacity(digest.len() * 2);
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Extract a (sha-verified) tarball into `dest_dir`, feeding tar from
/// stdin so the process is fully under our control, then gate the
/// EXTRACTED size. The decompression bomb bound is two-layered: the
/// tarball itself is bounded by ARTIFACT_MAX_BYTES at download time, and
/// the extracted tree by ARTIFACT_MAX_EXTRACTED_BYTES here — anything
/// over the ceiling fails the job and the caller removes the temp dir.
#[cfg(unix)]
pub(crate) async fn extract_tar_capped(
    tarball: &Path,
    dest_dir: &Path,
    kind: &str,
) -> anyhow::Result<()> {
    extract_tar_capped_under(tarball, dest_dir, kind, ARTIFACT_MAX_EXTRACTED_BYTES).await
}

/// Same extraction with an injectable size ceiling — production callers
/// pass ARTIFACT_MAX_EXTRACTED_BYTES; tests inject a tiny threshold to
/// prove the gate fires without materialising 4 GiB.
pub(crate) async fn extract_tar_capped_under(
    tarball: &Path,
    dest_dir: &Path,
    kind: &str,
    ceiling: u64,
) -> anyhow::Result<()> {
    let mut child = tokio::process::Command::new("tar")
        .arg("-xzf")
        .arg("-") // tarball on stdin: we control the feed
        .arg("-C")
        .arg(dest_dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .with_context(|| format!("failed to spawn tar for {kind} extract"))?;

    // Stream the verified tarball from disk into tar's stdin. If tar dies
    // (bad gzip, disk full), the write errors and the feeder stops — no
    // unbounded buffering.
    {
        let mut stdin = child.stdin.take().expect("tar stdin piped");
        let mut file = tokio::fs::File::open(tarball)
            .await
            .with_context(|| format!("open verified tarball {}", tarball.display()))?;
        let mut buf = vec![0u8; 1024 * 1024];
        loop {
            use tokio::io::AsyncReadExt as _;
            let n = file
                .read(&mut buf)
                .await
                .with_context(|| format!("read verified tarball {}", tarball.display()))?;
            if n == 0 {
                break;
            }
            if stdin.write_all(&buf[..n]).await.is_err() {
                break; // tar exited early; its status decides below
            }
        }
        let _ = stdin.shutdown().await;
    }

    let status = child
        .wait()
        .await
        .with_context(|| format!("wait tar for {kind} extract"))?;
    if !status.success() {
        return Err(anyhow!("{kind} tar extract failed with {status}"));
    }
    verify_extracted_size_under(dest_dir, kind, ceiling)
}

/// Post-extract size gate (see [`ARTIFACT_MAX_EXTRACTED_BYTES`]).
/// Same gate with an injectable ceiling (tests use a tiny threshold to
/// prove the gate fires without materialising 4 GiB).
pub(crate) fn verify_extracted_size_under(
    dir: &Path,
    kind: &str,
    ceiling: u64,
) -> anyhow::Result<()> {
    let bytes = crate::cache::dir_size(dir);
    if bytes > ceiling {
        return Err(anyhow!(
            "{kind} extracted {bytes} bytes, over the {} byte ceiling",
            ceiling
        ));
    }
    Ok(())
}

use futures_util::StreamExt;
#[cfg(test)]
mod tests {
    use super::*;

    /// Scratch dir cleaned on drop.
    struct Scratch(std::path::PathBuf);
    impl Scratch {
        fn new(tag: &str) -> Self {
            let p = std::env::temp_dir().join(format!(
                "p39-tar-{}-{}-{}",
                tag,
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).ok();
        }
    }

    /// Hand-rolled ustar entries: exact member-name control, no library
    /// normalizing names away. gzip via the system `gzip` (matches what
    /// the production tar will read).
    type Entry<'a> = (&'a str, Option<&'a [u8]>, Option<&'a str>);
    fn ustar(entries: &[Entry<'_>]) -> Vec<u8> {
        let mut tar: Vec<u8> = Vec::new();
        for (name, body, link) in entries {
            let mut h = vec![0u8; 512];
            h[..name.len().min(100)].copy_from_slice(&name.as_bytes()[..name.len().min(100)]);
            h[100..108].copy_from_slice(b"0000644\0");
            h[108..116].copy_from_slice(b"0000000\0");
            h[116..124].copy_from_slice(b"0000000\0");
            let data = body.unwrap_or(b"escaped-p39");
            let size = if link.is_some() { 0 } else { data.len() };
            h[124..136].copy_from_slice(format!("{:011o}\0", size).as_bytes());
            h[148..156].copy_from_slice(b"        ");
            let typeflag = if link.is_some() { b'2' } else { b'0' };
            h[156] = typeflag;
            if let Some(l) = link {
                let l = l.as_bytes();
                h[157..157 + l.len().min(100)].copy_from_slice(&l[..l.len().min(100)]);
            }
            h[257..263].copy_from_slice(b"ustar\0");
            h[263..265].copy_from_slice(b"00");
            let sum: u32 = h.iter().map(|b| *b as u32).sum();
            let chk = format!("{:06o}\0 ", sum);
            h[148..156].copy_from_slice(chk.as_bytes());
            tar.extend_from_slice(&h);
            if link.is_none() {
                tar.extend_from_slice(data);
                let pad = (512 - data.len() % 512) % 512;
                tar.extend(std::iter::repeat_n(0u8, pad));
            }
        }
        tar.extend(std::iter::repeat_n(0u8, 1024));
        // gzip via system tool (present everywhere tar is).
        // Pipe the tar through gzip's stdin — no temp-file round trip
        // (an FS race here flaked the suite under parallel tokio tests).
        // `-n` keeps the gzip header deterministic (no name/timestamp).
        use std::io::Write as _;
        let mut child = std::process::Command::new("gzip")
            .arg("-n")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("gzip available");
        child
            .stdin
            .take()
            .expect("gzip stdin")
            .write_all(&tar)
            .expect("feed gzip");
        let out = child.wait_with_output().expect("gzip finishes");
        assert!(
            out.status.success(),
            "gzip failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        out.stdout
    }

    /// PACKET 39 (2b): pinned MEASURED behaviour. bsdtar 3.5.3 (macOS) and
    /// GNU tar 1.34 (CI's ubuntu:22.04) both REFUSE `..` members outright.
    /// If a future tar/flag change weakens that, this test goes RED on the
    /// affected platform.
    #[tokio::test]
    async fn tar_refuses_dotdot_members_nothing_written_outside() {
        let s = Scratch::new("dotdot");
        let archive_bytes = ustar(&[("../escape-p39.txt", Some(b"escaped"), None)]);
        let tarball = s.path().join("evil.tar.gz");
        std::fs::write(&tarball, &archive_bytes).unwrap();
        let dest = s.path().join("dest");
        std::fs::create_dir_all(&dest).unwrap();

        let result = extract_tar_capped(&tarball, &dest, "payloads").await;
        assert!(result.is_err(), "tar must refuse .. members: {result:?}");
        // Nothing escaped the destination.
        assert!(!s.path().join("escape-p39.txt").exists());
        assert!(!s.path().parent().unwrap().join("escape-p39.txt").exists());
    }

    /// PACKET 39 (2b): absolute-path members are NEUTRALIZED (leading '/'
    /// stripped) on both tars — written INSIDE the destination, never at
    /// the absolute location.
    #[tokio::test]
    async fn tar_neutralizes_absolute_members() {
        let s = Scratch::new("absolute");
        let archive_bytes = ustar(&[("/tmp/p39-rust-absolute-escape.txt", Some(b"abs"), None)]);
        let tarball = s.path().join("abs.tar.gz");
        std::fs::write(&tarball, &archive_bytes).unwrap();
        let dest = s.path().join("dest");
        std::fs::create_dir_all(&dest).unwrap();

        extract_tar_capped(&tarball, &dest, "payloads")
            .await
            .expect("absolute member is neutralized, not fatal");
        // Written under dest (with the stripped-slash path)...
        assert!(dest.join("tmp/p39-rust-absolute-escape.txt").exists());
        // ...and NOT at the absolute location.
        assert!(!std::path::Path::new("/tmp/p39-rust-absolute-escape.txt").exists());
    }

    /// PACKET 39 (2b): symlink write-through is REFUSED by both tars.
    #[tokio::test]
    async fn tar_refuses_symlink_write_through() {
        let s = Scratch::new("through");
        let outside = s.path().join("victim.txt");
        std::fs::write(&outside, b"ORIGINAL").unwrap();
        let archive_bytes = ustar(&[
            (
                "evil",
                None,
                Some("../../../../../../../../../../tmp/does-not-matter"),
            ),
            ("evil/inner.txt", Some(b"THROUGH"), None),
        ]);
        let tarball = s.path().join("through.tar.gz");
        std::fs::write(&tarball, &archive_bytes).unwrap();
        let dest = s.path().join("dest");
        std::fs::create_dir_all(&dest).unwrap();

        let result = extract_tar_capped(&tarball, &dest, "payloads").await;
        assert!(
            result.is_err(),
            "tar must refuse writing through a symlink: {result:?}"
        );
        // The extraction failed BEFORE the verify gate; clean the dest the
        // way ensure_artifact's caller would.
        std::fs::remove_dir_all(&dest).ok();
        // The outside file was untouched (it was never the link target, but
        // pin the invariant that nothing outside dest changed).
        assert_eq!(std::fs::read(&outside).unwrap(), b"ORIGINAL");
    }

    /// PACKET 39 (2b): the extracted-size gate — a tree over the ceiling
    /// fails the extract even when tar itself succeeded.
    #[test]
    fn extracted_size_gate_fires_over_the_ceiling() {
        let s = Scratch::new("gate");
        let dest = s.path().join("dest");
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join("small.bin"), b"0123456789").unwrap(); // 10 bytes

        // 10 bytes over a 4-byte ceiling: the gate must fire.
        let err = verify_extracted_size_under(&dest, "payloads", 4)
            .expect_err("the size gate must fire over the ceiling");
        assert!(err.to_string().contains("over the"), "got: {err}");

        // And under a generous ceiling the same tree passes. (dir_size counts
        // allocated BLOCKS on unix, so a tiny tree still measures a few KiB.)
        verify_extracted_size_under(&dest, "payloads", 64 * 1024)
            .expect("under the ceiling passes");
    }

    /// PACKET 39 (2b), INTEGRATION: a benign archive over a tiny injected
    /// ceiling FAILS through the real extract path. This is the mutation
    /// target: the unit test above drives verify_extracted_size_under
    /// directly and cannot catch the gate CALL being removed from
    /// extract_tar_capped_under (proven — that mutation survived the unit
    /// test and was only caught here).
    #[tokio::test]
    async fn extract_path_enforces_the_size_ceiling() {
        let s = Scratch::new("gate-int");
        let archive_bytes = ustar(&[("index.html", Some(b"<html>ok</html>"), None)]);
        let tarball = s.path().join("good.tar.gz");
        std::fs::write(&tarball, &archive_bytes).unwrap();
        let dest = s.path().join("dest");
        std::fs::create_dir_all(&dest).unwrap();

        let err = extract_tar_capped_under(&tarball, &dest, "payloads", 1)
            .await
            .expect_err("the extract path must enforce the ceiling");
        assert!(err.to_string().contains("over the"), "got: {err}");
    }

    /// PACKET 39 (2b): the happy path — a benign archive extracts and the
    /// size gate passes.
    #[tokio::test]
    async fn benign_archive_extracts_and_passes_the_gate() {
        let s = Scratch::new("benign");
        let archive_bytes = ustar(&[("index.html", Some(b"<html>ok</html>"), None)]);
        let tarball = s.path().join("good.tar.gz");
        std::fs::write(&tarball, &archive_bytes).unwrap();
        let dest = s.path().join("dest");
        std::fs::create_dir_all(&dest).unwrap();

        extract_tar_capped(&tarball, &dest, "payloads")
            .await
            .expect("benign archive extracts");
        assert!(dest.join("index.html").exists());
    }
}
