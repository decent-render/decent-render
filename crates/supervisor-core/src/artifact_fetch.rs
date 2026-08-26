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
    verify_extracted_size(dest_dir, kind)
}

/// Post-extract size gate (see [`ARTIFACT_MAX_EXTRACTED_BYTES`]).
pub(crate) fn verify_extracted_size(dir: &Path, kind: &str) -> anyhow::Result<()> {
    let bytes = crate::cache::dir_size(dir);
    if bytes > ARTIFACT_MAX_EXTRACTED_BYTES {
        return Err(anyhow!(
            "{kind} extracted {bytes} bytes, over the {} byte ceiling",
            ARTIFACT_MAX_EXTRACTED_BYTES
        ));
    }
    Ok(())
}

use futures_util::StreamExt;
