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
    pub cancel: Option<oneshot::Sender<()>>,
    /// The run_job task itself. The connection loop MUST await this on every
    /// exit path (PACKET 5): cancel triggers teardown, but teardown runs to
    /// completion INSIDE the task — TERM → CANCEL_GRACE → KILL → pidfile
    /// sweep → purge. Dropping the JoinHandle (or returning while it is
    /// mid-grace) aborts the future at its await point and strands a live
    /// render tree on the operator machine. Observed in packet 4's wedge run:
    /// wedged runner + ffmpeg + 8 Chrome, alive until killed by hand.
    pub handle: tokio::task::JoinHandle<()>,
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

/// Test-only redirection of the worker state root. Never set in production.
static WORKER_ROOT_OVERRIDE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Root of this node's cached state — payloads, browsers — normally
/// `~/.decent-worker`.
///
/// Exists as a seam because the tests used to seed fake payloads and browsers
/// directly into the REAL directory. That is live operator state, not scratch:
/// it holds the sha-named artifacts an actual node renders with. The tests
/// name their fixtures `test-*` to avoid collisions, but cleaning up then
/// means `rm -rf` globs pointed at the real cache, and one slightly wrong glob
/// deletes a real payload. Redirecting the root removes the hazard entirely
/// rather than managing it.
fn worker_root() -> anyhow::Result<PathBuf> {
    if let Some(root) = WORKER_ROOT_OVERRIDE.get() {
        return Ok(root.clone());
    }
    Ok(home_dir()?.join(".decent-worker"))
}

/// Point the worker state root at `root` for the rest of this process.
///
/// Idempotent and first-write-wins (`OnceLock::set`), which is what makes it
/// safe to call from every test in a multi-threaded test binary without
/// ordering assumptions — unlike `set_var("HOME", ...)`, which races and would
/// leak into anything else reading the environment.
#[cfg(test)]
pub(crate) fn set_worker_root_for_tests(root: PathBuf) {
    let _ = WORKER_ROOT_OVERRIDE.set(root);
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
    let dir = worker_root()?.join(kind).join(sha256);
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

/// Write the per-job browser exec wrapper and return its path.
///
/// WHY THIS EXISTS (Defect A, packet 3): Remotion's BrowserRunner spawns
/// Chrome with `detached: true` on non-Windows, which makes Chrome a session
/// and group LEADER of its own — pgid == its pid, ppid eventually 1. The
/// runner's process-group kill (ITEM 1) is structurally unable to reach it,
/// and no amount of smarter scoping changes that: the kernel link back to
/// the runner is gone by the time we want to kill it.
///
/// The supervisor is the party that hands the browser to Remotion
/// (`DECENT_BROWSER_EXECUTABLE`), so THIS is the interposition point that
/// sees the exec boundary the runner never controls. The wrapper is a tiny
/// POSIX shell script placed in the supervisor's per-job workdir:
///
///   #!/bin/sh
///   echo $$ >> <pidfile>
///   exec "<real-executable>" "$@"
///
/// Remotion spawns this script as the "browser executable"; the script's pid
/// is written BEFORE `exec` replaces it with the real Chrome. Because
/// `detached: true` makes that pid a group leader, the pid IS the pgid of
/// Chrome's whole tree — so `killpg(pid, SIGKILL)` later is exactly the
/// containment Remotion itself uses when it closes a browser
/// (BrowserRunner.js kills `-proc.pid`).
///
/// Appending (`>>`) rather than truncating: Remotion opens a browser per
/// `selectComposition`/`renderMedia` call, so a job legitimately produces
/// multiple browser processes; all of them must die.
///
/// The wrapper lives in the supervisor's workdir so the purge deletes it
/// with everything else — no new cleanup path, no pidfile leak.
#[cfg(unix)]
fn write_browser_wrapper(
    workdir: &Path,
    real_executable: &Path,
) -> anyhow::Result<(PathBuf, PathBuf)> {
    use std::os::unix::fs::PermissionsExt;
    let wrapper = workdir.join(".decent-browser-wrapper");
    let pidfile = workdir.join(".decent-browser-pids");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\necho $$ >> {pidfile}\nexec \"{real}\" \"$@\"\n",
            pidfile = pidfile.display(),
            real = real_executable.display(),
        ),
    )
    .with_context(|| format!("write browser wrapper {}", wrapper.display()))?;
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755))
        .with_context(|| format!("chmod browser wrapper {}", wrapper.display()))?;
    Ok((wrapper, pidfile))
}

/// Kill every browser pid recorded by the job's exec wrapper.
///
/// Each recorded pid is a group leader BY CONSTRUCTION (detached spawn), so
/// killing its group takes Chrome's whole tree in one signal — the same
/// semantic as Remotion's own `process.kill(-proc.pid, 'SIGKILL')`.
///
/// Safety mirrors `terminate_child`'s runtime guard: never pid 0, never the
/// supervisor's own pid, and (unlike the group kill) also never a pid that
/// no longer belongs to us. A dead pid is simply skipped — ESRCH means the
/// browser is already gone, which is success. The pidfile is deleted by the
/// workdir purge; stale entries can only exist while the workdir does.
///
/// Returns the pids actually signalled, for the log line.
#[cfg(unix)]
fn kill_recorded_browsers(pidfile: &Path) -> Vec<u32> {
    let contents = match std::fs::read_to_string(pidfile) {
        Ok(c) => c,
        Err(_) => return Vec::new(), // no browser was ever spawned — fine
    };
    let me = std::process::id();
    let mut killed = Vec::new();
    for line in contents.lines() {
        let Ok(pid) = line.trim().parse::<u32>() else {
            continue; // tolerate junk lines rather than aborting the sweep
        };
        if pid == 0 || pid == me {
            // Paranoia identical to terminate_child: a corrupted pidfile
            // must never be able to kill the supervisor or its group.
            tracing::warn!(pid, "browser pidfile contains unsafe pid — skipping");
            continue;
        }
        if pid_exists(pid) {
            let rc = unsafe { libc::killpg(pid as libc::pid_t, libc::SIGKILL) };
            tracing::info!(pid, rc, "browser group SIGKILL (recorded at exec)");
            killed.push(pid);
        }
    }
    killed
}

/// pid > 0 is alive (kill(pid, 0) == 0) — ESRCH means dead, anything else
/// (e.g. EPERM) is treated as alive: err toward NOT killing a foreign pid.
#[cfg(unix)]
fn pid_exists(pid: u32) -> bool {
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
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
    // Browser containment (Defect A): when the supervisor resolved a browser,
    // hand Remotion the exec wrapper instead of the raw binary. The wrapper
    // records the spawned pid — which IS Chrome's group leader pid — so every
    // exit path can kill the browser tree even though Chrome has left the
    // runner's process group entirely. See `write_browser_wrapper`.
    #[cfg(unix)]
    let browser_pidfile = browser
        .as_ref()
        .map(|executable| write_browser_wrapper(workdir.path(), executable))
        .transpose()
        .context("install browser exec wrapper")?
        .map(|(_wrapper, pidfile)| pidfile);
    let mut command = Command::new(&runner);
    // The wire carries a sha and a URL; the local filesystem path is this
    // node's business alone, so it is handed over out-of-band rather than
    // being spliced into the jobAssign frame the runner parses.
    #[cfg(unix)]
    if browser.is_some() {
        command.env(
            "DECENT_BROWSER_EXECUTABLE",
            workdir.path().join(".decent-browser-wrapper"),
        );
    }
    #[cfg(not(unix))]
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
                terminate_child(&mut child, browser_pidfile.as_deref()).await;
                drop(workdir);
                tracing::info!(job_id = %assign.job_id, purged = !purged_path.exists(), "runner canceled and workdir purged");
                return Err(anyhow!("Render canceled by dispatch"));
            }
            line = tokio::time::timeout(SILENCE_TIMEOUT, lines.next_line()) => {
                let line = match line {
                    Err(_) => {
                        terminate_child(&mut child, browser_pidfile.as_deref()).await;
                        drop(workdir);
                        tracing::warn!(job_id = %assign.job_id, purged = !purged_path.exists(), "runner silent and workdir purged");
                        return Err(anyhow!("runner silent"));
                    }
                    Ok(Err(e)) => {
                        // ITEM 4: the stdout pipe failed, not the runner —
                        // the child may still be running and writing into
                        // the workdir. Kill it BEFORE `Drop` purges the
                        // directory out from under it.
                        terminate_child(&mut child, browser_pidfile.as_deref()).await;
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
                        terminate_child(&mut child, browser_pidfile.as_deref()).await;
                        return Err(anyhow!(message));
                    }
                }
            }
        }
    }

    let status = child.wait().await.context("runner wait failed")?;
    // Defect A, success path: Remotion normally closes its own browser, but
    // "normally" is doing load-bearing work — a runner that exits cleanly
    // while Chrome somehow lingers (a leaked browser from an aborted
    // selectComposition, a Remotion bug) would orphan it with nobody left to
    // kill it. The pidfile sweep is idempotent: dead pids are skipped.
    #[cfg(unix)]
    if let Some(pidfile) = browser_pidfile.as_deref() {
        let killed = kill_recorded_browsers(pidfile);
        if !killed.is_empty() {
            tracing::info!(job_id = %assign.job_id, pids = ?killed, "recorded browsers killed after runner exit");
        }
    }
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

/// Signal the runner's process group and wait out the grace period, then
/// kill any browser the exec wrapper recorded (Defect A backstop).
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
///   BUT (packet 3, Defect A): Remotion spawns Chrome `detached`, so Chrome
///   LEAVES this group and becomes its own leader — the group kill below
///   cannot reach it. That is what `browser_pidfile` is for: the exec
///   wrapper recorded Chrome's own leader pid, and after the runner tree is
///   down we killpg THAT pid — the exact semantic Remotion itself uses to
///   close a browser (`kill(-proc.pid)`).
///
/// Two-stage behaviour is preserved: TERM → `CANCEL_GRACE` (10s) → KILL,
/// followed by the recorded-browser sweep.
async fn terminate_child(child: &mut Child, browser_pidfile: Option<&Path>) {
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
    // Defect A backstop: Chrome left the runner's group at spawn (detached),
    // so the group signals above could not reach it. The exec wrapper
    // recorded its leader pid; kill that group now. Runs on EVERY terminate
    // path (cancel, silence, runner error event, stdout failure) — and
    // after SIGKILL escalation, when the runner had no chance to clean up.
    #[cfg(unix)]
    if let Some(pidfile) = browser_pidfile {
        let killed = kill_recorded_browsers(pidfile);
        if !killed.is_empty() {
            tracing::info!(pids = ?killed, "recorded browsers killed");
        }
    }
    #[cfg(not(unix))]
    let _ = browser_pidfile;
}

#[cfg(test)]
mod browser_kill_tests {
    use super::kill_recorded_browsers;

    /// The pidfile is attacker-influenced only in the pathological case, but
    /// the sweep must still never be able to kill the supervisor or its
    /// group: pid 0 and the supervisor's own pid are skipped, everything
    /// else signallable is killed.
    #[cfg(unix)]
    #[test]
    fn sweep_skips_unsafe_pids_and_kills_safe_dead_entries_quietly() {
        let dir = std::env::temp_dir().join(format!("decent-pidfile-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pidfile = dir.join("pids");
        let me = std::process::id();
        std::fs::write(
            &pidfile,
            format!(
                "0
{me}
junk
999999999
"
            ),
        )
        .unwrap();
        // None of these may panic, none may kill us: 0 and me are skipped,
        // junk is unparseable, 999999999 is dead (skipped by liveness).
        let killed = kill_recorded_browsers(&pidfile);
        assert!(
            killed.is_empty(),
            "nothing should have been signalled: {killed:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
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
