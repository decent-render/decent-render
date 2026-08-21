use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, Context};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::oneshot;

use crate::protocol::{
    JobAssignMessage, JobCompleteMessage, JobFailedMessage, JobMetrics, JobProgressMessage,
    WorkerMessage,
};
use crate::purge::WorkDir;

const SILENCE_TIMEOUT: Duration = Duration::from_secs(120);
const CANCEL_GRACE: Duration = Duration::from_secs(10);

#[derive(Debug)]
pub struct InFlightJob {
    pub job_id: String,
    pub cancel: oneshot::Sender<()>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum RunnerEvent {
    Progress {
        progress: f64,
    },
    /// Liveness only — the runner is alive but has no progress to report (a
    /// heavy composition can exceed the 5% reporting delta by more than
    /// [`SILENCE_TIMEOUT`]). Receiving it resets the silence timer; nothing is
    /// forwarded to dispatch.
    Heartbeat,
    Done {
        #[serde(rename = "outputSizeInBytes")]
        output_size_in_bytes: u64,
        #[serde(rename = "wallTimeMs")]
        wall_time_ms: u64,
        metrics: Option<JobMetrics>,
    },
    Error {
        message: String,
    },
}

fn home_dir() -> anyhow::Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("HOME is not set"))
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Fetch a content-addressed tarball into `~/.decent-worker/<kind>/<sha>`.
///
/// Shared by every artifact the supervisor caches (render payloads, browsers)
/// because the security-relevant steps are the same and must not drift apart:
/// download, verify the sha256 BEFORE anything is unpacked, extract into a
/// hidden sibling, then rename into place — so a torn download can never be
/// mistaken for a complete cache entry.
///
/// `marker` is the relative path that must exist for an extracted dir to count
/// as populated; it is what makes the cache check honest rather than a bare
/// directory-exists test.
async fn ensure_artifact(
    kind: &str,
    sha256: &str,
    get_url: &str,
    marker: &str,
) -> anyhow::Result<PathBuf> {
    let dir = home_dir()?.join(".decent-worker").join(kind).join(sha256);
    if dir.join(marker).exists() {
        tracing::info!(kind, sha = %sha256, path = %dir.display(), "artifact cached");
        return Ok(dir);
    }

    let parent = dir
        .parent()
        .ok_or_else(|| anyhow!("{kind} dir has no parent"))?
        .to_path_buf();
    tokio::fs::create_dir_all(&parent).await?;
    let tmp = parent.join(format!(".{sha256}-download"));
    if tmp.exists() {
        tokio::fs::remove_dir_all(&tmp).await.ok();
    }
    tokio::fs::create_dir_all(&tmp).await?;

    tracing::info!(kind, sha = %sha256, "downloading artifact");
    let bytes = reqwest::get(get_url)
        .await
        .with_context(|| format!("{kind} download request failed"))?
        .error_for_status()
        .with_context(|| format!("{kind} download returned non-2xx"))?
        .bytes()
        .await
        .with_context(|| format!("{kind} body read failed"))?;
    let actual = sha256_hex(&bytes);
    if actual != sha256 {
        tokio::fs::remove_dir_all(&tmp).await.ok();
        return Err(anyhow!(
            "{kind} sha mismatch: expected {sha256}, got {actual}"
        ));
    }
    let tar_path = tmp.join("artifact.tar.gz");
    tokio::fs::write(&tar_path, &bytes).await?;

    let status = Command::new("tar")
        .arg("-xzf")
        .arg(&tar_path)
        .arg("-C")
        .arg(&tmp)
        .status()
        .await
        .with_context(|| format!("failed to spawn tar for {kind} extract"))?;
    if !status.success() {
        tokio::fs::remove_dir_all(&tmp).await.ok();
        return Err(anyhow!("{kind} tar extract failed with {status}"));
    }
    tokio::fs::remove_file(&tar_path).await.ok();
    if !tmp.join(marker).exists() {
        tokio::fs::remove_dir_all(&tmp).await.ok();
        return Err(anyhow!("{kind} {sha256} is missing {marker} after extract"));
    }
    if dir.exists() {
        tokio::fs::remove_dir_all(&dir).await.ok();
    }
    tokio::fs::rename(&tmp, &dir)
        .await
        .or_else(|_| std::fs::rename(&tmp, &dir).map_err(anyhow::Error::from))?;
    tracing::info!(kind, sha = %sha256, path = %dir.display(), "artifact extracted");
    Ok(dir)
}

pub async fn ensure_payload(assign: &JobAssignMessage) -> anyhow::Result<PathBuf> {
    ensure_artifact(
        "payloads",
        &assign.payload_sha256,
        &assign.payload_get_url,
        "decent-render-runner",
    )
    .await
}

/// Resolve the browser for this job, fetching it if this node has not cached
/// that sha before.
///
/// `Ok(None)` means the assignment named no browser — the payload is expected
/// to carry its own under `chrome/` and the runner resolves it from there.
/// A named-but-unusable browser is an error, never a silent fallback: falling
/// back would make Remotion download ~1GB into the per-job workdir and lose it
/// to the purge on every job, which is far worse than failing the job loudly.
pub async fn ensure_browser(assign: &JobAssignMessage) -> anyhow::Result<Option<PathBuf>> {
    let (Some(sha), Some(url)) = (
        assign.browser_sha256.as_deref(),
        assign.browser_get_url.as_deref(),
    ) else {
        return Ok(None);
    };
    let dir = ensure_artifact("browsers", sha, url, "executable").await?;
    browser_executable_in(&dir).map(Some)
}

/// Read a browser artifact's `executable` manifest and resolve it to a path.
///
/// The manifest names the browser relative to the artifact root, because the
/// publisher knows exactly what it downloaded and the supervisor should not be
/// guessing per-platform nested layouts (`.../mac-arm64/chrome-mac-arm64/Google
/// Chrome for Testing.app/Contents/MacOS/...`) that differ per OS.
fn browser_executable_in(dir: &Path) -> anyhow::Result<PathBuf> {
    let manifest = dir.join("executable");
    let contents = std::fs::read_to_string(&manifest)
        .with_context(|| format!("read browser manifest {}", manifest.display()))?;
    let relative = contents.trim();
    if relative.is_empty() {
        return Err(anyhow!("browser manifest {} is empty", manifest.display()));
    }
    // This path gets executed, so it must stay inside the artifact: an absolute
    // path or a `..` hop would let a tarball point the supervisor at any binary
    // on the node. The sha is verified, so this only bites when the publisher is
    // wrong or compromised — precisely the case worth surviving.
    let candidate = Path::new(relative);
    if candidate.is_absolute()
        || candidate
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(anyhow!(
            "browser manifest {} escapes the artifact: {relative}",
            manifest.display()
        ));
    }
    let executable = dir.join(candidate);
    if !executable.exists() {
        return Err(anyhow!(
            "browser manifest points at {}, which does not exist",
            executable.display()
        ));
    }
    Ok(executable)
}

pub async fn run_job(
    assign: JobAssignMessage,
    mut cancel_rx: oneshot::Receiver<()>,
    tx: tokio::sync::mpsc::UnboundedSender<WorkerMessage>,
) {
    let job_id = assign.job_id.clone();
    let tenant = assign.tenant.clone();
    let output_key = assign.output_key.clone();
    let attempt = assign.attempt;
    match run_job_inner(assign, &mut cancel_rx, tx.clone()).await {
        Ok(metrics) => {
            let _ = tx.send(WorkerMessage::JobComplete(JobCompleteMessage {
                tenant,
                job_id,
                attempt,
                output_key,
                metrics,
            }));
        }
        Err(err) => {
            let _ = tx.send(WorkerMessage::JobFailed(JobFailedMessage {
                tenant,
                job_id,
                attempt,
                reason: err.to_string(),
            }));
        }
    }
}

async fn run_job_inner(
    assign: JobAssignMessage,
    cancel_rx: &mut oneshot::Receiver<()>,
    tx: tokio::sync::mpsc::UnboundedSender<WorkerMessage>,
) -> anyhow::Result<JobMetrics> {
    let payload_dir = ensure_payload(&assign).await?;
    let runner = payload_dir.join("decent-render-runner");
    if !runner.exists() {
        return Err(anyhow!(
            "payload missing decent-render-runner at {}",
            runner.display()
        ));
    }
    let browser = ensure_browser(&assign).await?;
    let workdir = WorkDir::new(&format!("job-{}", assign.job_id)).context("create workdir")?;
    let purged_path = workdir.path().to_path_buf();
    let mut command = Command::new(&runner);
    // The wire carries a sha and a URL; the local filesystem path is this
    // node's business alone, so it is handed over out-of-band rather than
    // being spliced into the jobAssign frame the runner parses.
    if let Some(ref executable) = browser {
        command.env("DECENT_BROWSER_EXECUTABLE", executable);
    }
    // Containment: the runner gets its OWN process group (see
    // `terminate_process_group` for the sign convention) so a cancel can
    // reach Chrome, which the runner spawns as a grandchild. Spawning
    // without a group left terminate able to signal only the runner pid,
    // orphaning Chrome on every cancel/silence-kill/shutdown.
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command
        .current_dir(workdir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("spawn runner {}", runner.display()))?;

    let mut stdin = child.stdin.take().context("runner stdin missing")?;
    let mut input_frame = serde_json::to_value(&assign)?;
    if let serde_json::Value::Object(ref mut obj) = input_frame {
        obj.insert("type".into(), serde_json::Value::String("jobAssign".into()));
    }
    let input = serde_json::to_vec(&input_frame)?;
    // A failed write is NOT the job's failure reason.
    //
    // If the runner exits before reading its stdin — which is exactly what the
    // error path does — this write races with that exit and loses on Linux with
    // EPIPE (macOS buffers the frame and the write succeeds, which is why this
    // only ever failed in CI). Propagating that error replaced the runner's real
    // message with "Broken pipe (os error 32)", destroying the only useful
    // diagnostic the operator would ever see.
    //
    // So: record it and keep going. The runner's own `error` event, or its exit
    // status, is the authoritative reason. This is only reported if nothing else
    // explains the failure.
    let stdin_error = match stdin.write_all(&input).await {
        Ok(()) => stdin.shutdown().await.err(),
        Err(e) => Some(e),
    };
    if let Some(ref e) = stdin_error {
        tracing::warn!(job_id = %assign.job_id, error = %e, "writing jobAssign to runner stdin failed; deferring to the runner's own report");
    }
    drop(stdin);

    let stdout = child.stdout.take().context("runner stdout missing")?;
    let mut lines = BufReader::new(stdout).lines();
    let mut done_metrics: Option<JobMetrics> = None;

    loop {
        tokio::select! {
            _ = &mut *cancel_rx => {
                terminate_child(&mut child).await;
                drop(workdir);
                tracing::info!(job_id = %assign.job_id, purged = !purged_path.exists(), "runner canceled and workdir purged");
                return Err(anyhow!("Render canceled by dispatch"));
            }
            line = tokio::time::timeout(SILENCE_TIMEOUT, lines.next_line()) => {
                let line = match line {
                    Err(_) => {
                        terminate_child(&mut child).await;
                        drop(workdir);
                        tracing::warn!(job_id = %assign.job_id, purged = !purged_path.exists(), "runner silent and workdir purged");
                        return Err(anyhow!("runner silent"));
                    }
                    Ok(Err(e)) => {
                        // ITEM 4: the stdout pipe failed, not the runner —
                        // the child may still be running and writing into
                        // the workdir. Kill it BEFORE `Drop` purges the
                        // directory out from under it.
                        terminate_child(&mut child).await;
                        return Err(anyhow!(e).context("runner stdout read failed"));
                    }
                    Ok(Ok(None)) => break,
                    Ok(Ok(Some(line))) => line,
                };
                let event: RunnerEvent = match serde_json::from_str(&line) {
                    Ok(event) => event,
                    Err(_) => {
                        tracing::warn!(job_id = %assign.job_id, line = %line, "ignoring non-NDJSON runner stdout line");
                        continue;
                    }
                };
                match event {
                    RunnerEvent::Heartbeat => {
                        tracing::trace!(job_id = %assign.job_id, "runner heartbeat");
                    }
                    RunnerEvent::Progress { progress } => {
                        let _ = tx.send(WorkerMessage::JobProgress(JobProgressMessage {
                            tenant: assign.tenant.clone(),
                            job_id: assign.job_id.clone(),
                            attempt: assign.attempt,
                            progress,
                        }));
                    }
                    RunnerEvent::Done { output_size_in_bytes, wall_time_ms, metrics } => {
                        tracing::info!(job_id = %assign.job_id, output_size_in_bytes, wall_time_ms, "runner done");
                        let mut m = metrics.unwrap_or(JobMetrics {
                            wall_ms: wall_time_ms,
                            frames: assign.duration_frames,
                            output_size_in_bytes: None,
                        });
                        // The `done` envelope always carries the output size;
                        // stamp it onto the metrics so dispatch persists it.
                        m.output_size_in_bytes = Some(output_size_in_bytes);
                        done_metrics = Some(m);
                    }
                    RunnerEvent::Error { message } => {
                        // ITEM 4: an `error` event does not imply exit — the
                        // runner may keep running (or keep Chrome running)
                        // after emitting it. Kill it BEFORE `Drop` purges the
                        // workdir out from under a live tree.
                        terminate_child(&mut child).await;
                        return Err(anyhow!(message));
                    }
                }
            }
        }
    }

    let status = child.wait().await.context("runner wait failed")?;
    drop(workdir);
    tracing::info!(job_id = %assign.job_id, purged = !purged_path.exists(), "workdir purged after runner exit");
    if !status.success() {
        return Err(anyhow!("runner exited with {status}"));
    }
    done_metrics.ok_or_else(|| match stdin_error {
        // Only now is the failed stdin write the best explanation available: the
        // runner exited cleanly, said nothing, and never got its assignment.
        Some(e) => anyhow!("runner never received its assignment: {e}"),
        None => anyhow!("runner exited without done event"),
    })
}

/// Signal the runner's process group and wait out the grace period.
///
/// ITEM 1 sign convention — read before touching:
///
/// * At spawn, `Command::process_group(0)` puts the runner in a NEW process
///   group whose pgid EQUALS the runner's own pid. That pgid is what we
///   signal here — never literal 0, never the negative of anything, never a
///   pgid we did not derive from the child we spawned.
///
/// * We call `libc::killpg(pgid, sig)` with a STRICTLY POSITIVE pgid that
///   came from `child.id()`. `killpg(0, …)` would mean "the caller's own
///   group" (the supervisor + the operator's shell) — catastrophic. The
///   runtime guard below rejects pgid 0 and our own pid before signalling,
///   so a future edit that loses the sign/type cannot kill the supervisor.
///
/// * Why a group at all: Chrome is a grandchild (spawned by Remotion inside
///   the runner). Signalling only the runner pid — the old behaviour — left
///   Chrome running on every cancel, silence-kill, and shutdown.
///
/// Two-stage behaviour is preserved: TERM → `CANCEL_GRACE` (10s) → KILL.
async fn terminate_child(child: &mut Child) {
    #[cfg(unix)]
    {
        if let Some(pid) = child.id() {
            // Belt-and-braces: pid 0 is never a valid target for us, and the
            // supervisor's own pid must never be signalled, even if a future
            // refactor hands terminate a pid it did not spawn.
            let safe = pid != 0 && pid != std::process::id();
            if safe {
                let rc = unsafe { libc::killpg(pid as libc::pid_t, libc::SIGTERM) };
                tracing::info!(pid, rc, "TERM sent to process group");
            } else {
                tracing::warn!(pid, "refusing to signal unsafe process group");
            }
        }
    }
    #[cfg(not(unix))]
    let _ = child.start_kill();

    match tokio::time::timeout(CANCEL_GRACE, child.wait()).await {
        Ok(_) => {}
        Err(_) => {
            #[cfg(unix)]
            if let Some(pid) = child.id() {
                let safe = pid != 0 && pid != std::process::id();
                if safe {
                    let _ = unsafe { libc::killpg(pid as libc::pid_t, libc::SIGKILL) };
                }
            }
            #[cfg(not(unix))]
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch artifact dir, cleaned up on drop.
    struct Artifact(PathBuf);

    impl Artifact {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("decent-browser-test-{name}"));
            std::fs::remove_dir_all(&dir).ok();
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        fn manifest(&self, contents: &str) -> &Self {
            std::fs::write(self.0.join("executable"), contents).unwrap();
            self
        }
        fn file(&self, relative: &str) -> &Self {
            let path = self.0.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"#!/bin/sh\n").unwrap();
            self
        }
    }

    impl Drop for Artifact {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).ok();
        }
    }

    #[test]
    fn resolves_a_nested_browser_from_the_manifest() {
        let a = Artifact::new("nested");
        a.file("mac-arm64/Chrome.app/Contents/MacOS/chrome")
            .manifest("mac-arm64/Chrome.app/Contents/MacOS/chrome\n");
        let resolved = browser_executable_in(&a.0).unwrap();
        assert_eq!(
            resolved,
            a.0.join("mac-arm64/Chrome.app/Contents/MacOS/chrome")
        );
    }

    #[test]
    fn rejects_a_manifest_that_escapes_the_artifact() {
        let a = Artifact::new("escape");
        a.manifest("../../../../bin/sh\n");
        let err = browser_executable_in(&a.0).unwrap_err().to_string();
        assert!(err.contains("escapes the artifact"), "got: {err}");
    }

    #[test]
    fn rejects_an_absolute_manifest() {
        let a = Artifact::new("absolute");
        a.manifest("/bin/sh\n");
        let err = browser_executable_in(&a.0).unwrap_err().to_string();
        assert!(err.contains("escapes the artifact"), "got: {err}");
    }

    #[test]
    fn rejects_an_empty_manifest() {
        let a = Artifact::new("empty");
        a.manifest("   \n");
        let err = browser_executable_in(&a.0).unwrap_err().to_string();
        assert!(err.contains("is empty"), "got: {err}");
    }

    #[test]
    fn rejects_a_manifest_pointing_at_a_missing_file() {
        let a = Artifact::new("missing");
        a.manifest("chrome/does-not-exist\n");
        let err = browser_executable_in(&a.0).unwrap_err().to_string();
        assert!(err.contains("does not exist"), "got: {err}");
    }

    /// An assignment with no browser fields must resolve to `None` rather than
    /// erroring — payloads published before the split bundle their own browser
    /// under `chrome/` and the runner falls back to that manifest.
    #[tokio::test]
    async fn no_browser_in_assignment_resolves_to_none() {
        let assign: JobAssignMessage = serde_json::from_str(
            r#"{"type":"jobAssign","tenant":"driffs","jobId":"j","kind":"standard","durationFrames":1,"fps":30,"codec":"h264","bundleSha256":"s","bundleGetUrl":"u","payloadSha256":"p","payloadGetUrl":"u","inputPropsGetUrl":"u","assetGetUrls":[],"outputPutUrl":"u","outputKey":"k","purgeAfter":true}"#,
        )
        .unwrap();
        assert!(ensure_browser(&assign).await.unwrap().is_none());
    }

    /// Half a browser reference is a publisher bug; treat it as "none" rather
    /// than fetching from an unverifiable URL or verifying a sha we cannot get.
    #[tokio::test]
    async fn a_half_specified_browser_is_ignored() {
        let assign: JobAssignMessage = serde_json::from_str(
            r#"{"type":"jobAssign","tenant":"driffs","jobId":"j","kind":"standard","durationFrames":1,"fps":30,"codec":"h264","bundleSha256":"s","bundleGetUrl":"u","payloadSha256":"p","payloadGetUrl":"u","browserSha256":"abc","inputPropsGetUrl":"u","assetGetUrls":[],"outputPutUrl":"u","outputKey":"k","purgeAfter":true}"#,
        )
        .unwrap();
        assert!(ensure_browser(&assign).await.unwrap().is_none());
    }
}
