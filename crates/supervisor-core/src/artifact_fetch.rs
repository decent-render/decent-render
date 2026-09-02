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
use std::time::Duration;
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
    artifact_client_with(ARTIFACT_CONNECT_TIMEOUT, ARTIFACT_READ_TIMEOUT)
}

/// [`artifact_client`] with INJECTED timeouts: the behaviour tests prove the
/// connect/read bounds with tiny values (the production constants would take
/// minutes to fire). Same builder, same options — only the durations differ.
pub(crate) fn artifact_client_with(connect: Duration, read: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(connect)
        .read_timeout(read)
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
    download_to_file_hashed_capped(client, get_url, dest, kind, ARTIFACT_MAX_BYTES).await
}

/// [`download_to_file_hashed`] with the byte ceiling as a parameter, so the
/// boundary (exactly-at-cap accepted, one chunk over rejected, lying
/// Content-Length refused up front) can be proven with a few MiB instead of
/// streaming 2 GiB through the suite (packet 66 follow-up).
async fn download_to_file_hashed_capped(
    client: &reqwest::Client,
    get_url: &str,
    dest: &Path,
    kind: &str,
    max_bytes: u64,
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
        if len > max_bytes {
            return Err(anyhow!(
                "{kind} content-length {len} exceeds the {} byte artifact ceiling",
                max_bytes
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
        if written > max_bytes {
            // Remove the oversized temp file before failing — the caller
            // cannot be trusted to know how far we got.
            let _ = tokio::fs::remove_file(dest).await;
            return Err(anyhow!(
                "{kind} download exceeded the {} byte artifact ceiling",
                max_bytes
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

    // ── PACKET 66 (C-13 / N-13 rank B): the artifact caps are exact ──────
    //
    // A hostile or buggy artifact server is exactly the threat the caps
    // exist for, so the boundary is pinned from BOTH sides: exactly-at-cap
    // must pass, one byte over must fail — for the content-length
    // pre-flight AND the streamed counter. The fake server is a raw
    // TcpListener (hyper-free): it streams zeros with no content-length so
    // the STREAMED counter is what fires.

    /// A small ceiling for the streaming boundary tests: the gate logic is
    /// identical at any cap, and 4 MiB proves it without pushing 2 GiB
    /// through the suite on every run.
    const TEST_CAP: u64 = 4 * 1024 * 1024;

    /// One-shot fake artifact server: streams `total` bytes of zeros (in
    /// `chunk`-byte pieces) with NO content-length, unless `content_length`
    /// overrides the header (to test a lying pre-flight). Connection close
    /// delimits the body.
    async fn serve_zero_stream(
        total: u64,
        chunk: usize,
        content_length: Option<u64>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buf = vec![0u8; 8192];
            let _ = socket.read(&mut buf).await; // drain the request head
            let cl = match content_length {
                Some(n) => format!("content-length: {n}\r\n"),
                None => String::new(),
            };
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/octet-stream\r\n{cl}connection: close\r\n\r\n"
            );
            let _ = socket.write_all(head.as_bytes()).await;
            let zeros = vec![0u8; chunk];
            let mut left = total;
            while left > 0 {
                let n = (left.min(chunk as u64)) as usize;
                if socket.write_all(&zeros[..n]).await.is_err() {
                    break;
                }
                left -= n as u64;
            }
            let _ = socket.shutdown().await;
        });
        (format!("http://{addr}/artifact"), handle)
    }

    fn scratch_dest(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "p66-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    // ── PACKET 74 (N-13 old rank B): the timeouts proven BY BEHAVIOUR ────
    //
    // The packet-37 constants are only paper until something observes a
    // server that violates them. These tests use tiny injected timeouts
    // (never the production constants) against pathological servers:
    //
    // - a server that accepts the connection and NEVER writes (read timeout
    //   must abort the transfer),
    // - a black-hole address that never completes the TCP handshake
    //   (connect timeout must abort it).
    //
    // Each mutant — dropping `.read_timeout` / `.connect_timeout` from the
    // builder — turns the fast inner error into the OUTER wall-clock
    // timeout, which the elapsed-time assertion catches.

    /// Server accepts and goes silent: the per-read timeout must abort the
    /// transfer (well inside the outer wall-clock bound) with a timeout
    /// error, not a hang.
    #[tokio::test]
    async fn read_timeout_fails_a_server_that_accepts_and_never_writes() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // Hold the accepted socket open forever: read nothing, write
        // nothing, never drop it inside this task.
        tokio::spawn(async move {
            if let Ok((socket, _)) = listener.accept().await {
                let _keep_open = socket;
                futures_util::future::pending::<()>().await;
            }
        });

        let client = artifact_client_with(Duration::from_secs(5), Duration::from_millis(200));
        let dest = scratch_dest("read-timeout");
        let started = std::time::Instant::now();
        let outcome = tokio::time::timeout(Duration::from_secs(5), async {
            download_to_file_hashed_capped(
                &client,
                &format!("http://{addr}/x"),
                &dest,
                "payloads",
                1024,
            )
            .await
        })
        .await;
        let elapsed = started.elapsed();

        // The INNER result must be the error — an outer Err means the whole
        // call hung past the wall-clock bound (the mutant signature).
        let inner = outcome.expect("the call itself must not hit the outer timeout");
        let error = inner.expect_err("a silent server must fail the download");
        assert!(
            elapsed < Duration::from_secs(2),
            "read timeout fired late: {elapsed:?}"
        );
        let rendered = format!("{error:#}");
        assert!(
            rendered.contains("timed out") || rendered.contains("timeout"),
            "the error should be a timeout, got: {rendered}"
        );
        let _ = tokio::fs::remove_file(&dest).await;
    }

    /// A black-hole address (RFC 1918, no route): the CONNECT timeout must
    /// abort the dial (well inside the outer wall-clock bound) instead of
    /// hanging on SYN retransmits.
    #[tokio::test]
    async fn connect_timeout_fails_a_black_hole_address() {
        let client = artifact_client_with(Duration::from_millis(300), Duration::from_secs(5));
        let dest = scratch_dest("connect-timeout");
        let started = std::time::Instant::now();
        let outcome = tokio::time::timeout(Duration::from_secs(10), async {
            download_to_file_hashed_capped(
                &client,
                "http://10.255.255.1:9/x",
                &dest,
                "payloads",
                1024,
            )
            .await
        })
        .await;
        let elapsed = started.elapsed();

        let inner = outcome.expect("the call itself must not hit the outer timeout");
        assert!(
            inner.is_err(),
            "a black-hole address must fail the dial: {inner:?}"
        );
        assert!(
            elapsed < Duration::from_secs(3),
            "connect timeout fired late: {elapsed:?}"
        );
        let _ = tokio::fs::remove_file(&dest).await;
    }

    /// Exactly-at-cap content-length passes the pre-flight: the gate is
    /// strictly-above, so the transfer proceeds (and fails on the truncated
    /// body — "body read failed" — never on the ceiling wording).
    #[tokio::test]
    async fn content_length_exactly_at_cap_passes_the_preflight() {
        let (url, server) = serve_zero_stream(0, 1024, Some(TEST_CAP)).await;
        let dest = scratch_dest("cl-exact");
        let error =
            download_to_file_hashed_capped(&artifact_client(), &url, &dest, "payloads", TEST_CAP)
                .await
                .expect_err("a truncated body must fail");
        assert!(
            error.to_string().contains("body read failed"),
            "exactly-at-cap must pass the pre-flight and fail on truncation, got: {error}"
        );
        assert!(
            !error.to_string().contains("exceeds"),
            "exactly-at-cap must not trip the ceiling pre-flight, got: {error}"
        );
        let _ = tokio::fs::remove_file(&dest).await;
        server.abort();
    }

    /// One byte over the cap in the content-length is rejected BY THE
    /// PRE-FLIGHT, before any body bytes are read.
    #[tokio::test]
    async fn content_length_over_cap_is_rejected_by_the_preflight() {
        let (url, server) = serve_zero_stream(0, 1024, Some(TEST_CAP + 1)).await;
        let dest = scratch_dest("cl-over");
        let error =
            download_to_file_hashed_capped(&artifact_client(), &url, &dest, "payloads", TEST_CAP)
                .await
                .expect_err("over-cap content-length must fail");
        assert!(
            error.to_string().contains("content-length"),
            "the pre-flight must reject it, got: {error}"
        );
        assert!(error.to_string().contains("exceeds"), "got: {error}");
        server.abort();
    }

    /// A streamed body of EXACTLY the cap is accepted — the streamed counter
    /// is strictly-above too. Kills the `>` → `>=` mutant on the counter.
    #[tokio::test]
    async fn streamed_body_exactly_at_cap_is_accepted() {
        let chunk = 1024 * 1024;
        let (url, server) = serve_zero_stream(TEST_CAP, chunk, None).await;
        let dest = scratch_dest("stream-exact");
        let sha =
            download_to_file_hashed_capped(&artifact_client(), &url, &dest, "payloads", TEST_CAP)
                .await
                .expect("exactly-at-cap must stream successfully");
        // sha pins that ALL TEST_CAP bytes were hashed (sha256 of that many zeros).
        let mut expected = Sha256::new();
        let zeros = vec![0u8; chunk];
        for _ in 0..(TEST_CAP / chunk as u64) {
            expected.update(&zeros);
        }
        assert_eq!(sha, hex_lower(&expected.finalize()));
        assert!(dest.exists());
        tokio::fs::remove_file(&dest).await.ok();
        server.abort();
    }

    /// A streamed body one chunk OVER the cap is rejected and the oversized
    /// temp file is REMOVED (the code promises the caller cannot be trusted
    /// to know how far it got). Kills the `+=` → `*=` mutant: a zeroed
    /// counter never fires and the body would complete successfully.
    #[tokio::test]
    async fn streamed_body_one_chunk_over_the_cap_is_rejected_and_temp_file_removed() {
        let chunk = 1024 * 1024;
        let (url, server) = serve_zero_stream(TEST_CAP + chunk as u64, chunk, None).await;
        let dest = scratch_dest("stream-over");
        let error =
            download_to_file_hashed_capped(&artifact_client(), &url, &dest, "payloads", TEST_CAP)
                .await
                .expect_err("an over-cap streamed body must fail");
        assert!(error.to_string().contains("exceeded the"), "got: {error}");
        assert!(
            !dest.exists(),
            "the oversized temp file must be removed after the streamed cap fires"
        );
        server.abort();
    }

    /// The extraction ceiling boundary is EXACT: a tree measuring exactly
    /// the ceiling passes; one more block above it fires the gate.
    #[test]
    fn extracted_size_gate_exactly_at_the_ceiling_passes_and_above_fails() {
        let s = Scratch::new("gate-exact");
        let dest = s.path().join("dest");
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join("a.bin"), vec![0u8; 512]).unwrap();
        let measured = crate::cache::dir_size(&dest);
        assert!(measured > 0, "sanity: the tree must measure non-zero");
        verify_extracted_size_under(&dest, "payloads", measured)
            .expect("exactly at the ceiling passes");

        // One more block-worth of tree: now strictly above the old ceiling.
        std::fs::write(dest.join("b.bin"), vec![0u8; 512]).unwrap();
        let grown = crate::cache::dir_size(&dest);
        assert!(grown > measured, "growing the tree must grow the measure");
        let err = verify_extracted_size_under(&dest, "payloads", measured)
            .expect_err("the grown tree is over the old ceiling");
        assert!(err.to_string().contains("over the"), "got: {err}");
        // And it fits its own (grown) measure — the failure above is the
        // boundary, not the tree.
        verify_extracted_size_under(&dest, "payloads", grown)
            .expect("the grown tree fits the grown ceiling");
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

/// N-13 rank C: pin the download/extraction limits to their documented
/// values so a `*`→`+` slip (2 GiB → 5 bytes: every artifact refused; 30 min
/// → 90 s: every big payload times out) cannot pass the suite.
#[cfg(test)]
mod const_pins {
    use super::*;

    #[test]
    fn download_caps_are_two_and_four_gib() {
        assert_eq!(ARTIFACT_MAX_BYTES, 2_147_483_648);
        assert_eq!(ARTIFACT_MAX_EXTRACTED_BYTES, 4_294_967_296);
    }

    #[test]
    fn timeouts_are_ten_seconds_connect_sixty_read_thirty_minutes_total() {
        assert_eq!(ARTIFACT_CONNECT_TIMEOUT, std::time::Duration::from_secs(10));
        assert_eq!(ARTIFACT_READ_TIMEOUT, std::time::Duration::from_secs(60));
        assert_eq!(
            ARTIFACT_TOTAL_TIMEOUT,
            std::time::Duration::from_secs(1_800)
        );
    }
}
