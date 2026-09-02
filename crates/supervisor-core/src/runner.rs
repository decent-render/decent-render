use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use crate::artifact_fetch;
use anyhow::{anyhow, Context};
use serde::Deserialize;
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

/// Hard ceiling on how long one job may run, however healthy it looks.
///
/// [`SILENCE_TIMEOUT`] only catches a runner that goes QUIET. A job that keeps
/// emitting progress resets that timer forever, so a pathological composition
/// — an accidental 10-hour duration, a render that crawls at a frame a minute —
/// occupies the node indefinitely and no existing timeout ever fires. Dispatch
/// will not rescue it either: from its side the job is progressing normally.
///
/// One hour is well above any legitimate render the farm has seen (the E2E
/// 30-frame render takes ~2s; the heaviest real compositions are minutes) and
/// well below "nobody noticed for a day".
const MAX_JOB_WALL_TIME: Duration = Duration::from_secs(60 * 60);

// Caveat (packet 18): this deadline uses tokio's monotonic timer, which
// does NOT advance across system sleep — a render suspended by lid-close
// or forced sleep undercounts its wall time. The per-job idle-sleep
// assertion (keepawake.rs) shrinks that to lid-close/forced sleep only.

/// `DECENT_MAX_JOB_WALL_TIME_MS` overrides the ceiling (floor 50ms) so tests
/// can exercise the path in milliseconds instead of waiting out an hour. Same
/// shape as runner-core's `DECENT_RUNNER_HEARTBEAT_MS`.
pub(crate) fn max_job_wall_time() -> Duration {
    parse_job_wall_time_override(std::env::var("DECENT_MAX_JOB_WALL_TIME_MS").ok().as_deref())
}

/// Pure parse of the wall-time override — parameterized on the raw value
/// (rather than reading the env inside) for the same reason
/// `WORKER_ROOT_OVERRIDE` exists: the test binary is parallel, and mutating
/// process-global env from one test while a connection test spawns a job
/// that reads it is a flake factory. Tests drive this function directly;
/// production goes through [`max_job_wall_time`].
pub(crate) fn parse_job_wall_time_override(raw: Option<&str>) -> Duration {
    match raw.and_then(|raw| raw.parse::<u64>().ok()) {
        Some(ms) if ms >= 50 => Duration::from_millis(ms),
        _ => MAX_JOB_WALL_TIME,
    }
}

/// Hard ceiling on how much disk one job's workdir may consume.
///
/// A render that fills the disk takes down the WHOLE node — every other job,
/// the supervisor itself, and anything else on the machine — which is a
/// different class of damage from a job that merely fails. The workdir holds
/// the render output, the browser profile, and any frame intermediates; real
/// jobs land in the tens of MB (the E2E fixture output is ~48KB, a heavy
/// 1080p composition a few hundred MB at most). 20 GiB is two orders of
/// magnitude above any legitimate job this farm has seen, and far below the
/// free space a node needs to stay healthy.
const MAX_WORKDIR_BYTES: u64 = 20 * 1024 * 1024 * 1024;

/// How often the workdir is size-sampled during a render. See
/// [`WORKDIR_SAMPLE_INTERVAL`] on the sampler branch for the cost argument.
const WORKDIR_SAMPLE_INTERVAL: Duration = Duration::from_secs(2);

/// `DECENT_MAX_WORKDIR_BYTES` overrides the cap (floor 1 MiB) so tests can
/// exercise the path with a few KB instead of 20 GiB, and operators can tune
/// for a small disk. Same shape as [`max_job_wall_time`].
pub(crate) fn max_workdir_bytes() -> u64 {
    parse_workdir_bytes_override(std::env::var("DECENT_MAX_WORKDIR_BYTES").ok().as_deref())
}

/// Pure parse of the workdir cap override. Parameterized for the same
/// parallel-test reason as [`parse_job_wall_time_override`].
pub(crate) fn parse_workdir_bytes_override(raw: Option<&str>) -> u64 {
    match raw.and_then(|raw| raw.parse::<u64>().ok()) {
        Some(bytes) if bytes >= 1024 * 1024 => bytes,
        _ => MAX_WORKDIR_BYTES,
    }
}

#[derive(Debug)]
pub struct InFlightJob {
    pub job_id: String,
    /// The assignment lease this in-flight job belongs to (C-4). Terminal
    /// frames are guarded by (job_id, attempt): dispatch requeues a failed
    /// or refunded job as attempt+1 of the same job id, so matching by job
    /// id alone would let a late attempt-N frame clear an in-flight
    /// attempt-N+1 render.
    pub attempt: Option<u32>,
    pub cancel: Option<oneshot::Sender<()>>,
    /// The cache keys (kind:sha) this job is using — its payload, browser
    /// and bundle shas. Consumed by the post-termination cache sweep so a
    /// sweep that starts after this job ends can still protect a
    /// CONCURRENT download... and, more precisely, so the sweep that runs
    /// at THIS job's termination knows which entries were just used (they
    /// were touched moments ago, but explicit protection is cheaper to
    /// reason about than marker recency).
    pub cache_keys: Vec<String>,
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
        /// Same bound as protocol v2's jobProgress: a fraction in [0, 1].
        /// (C-3: the shared fixture caught the drift — the lenient `f64`
        /// accepted out-of-range values dispatch would refuse downstream.)
        #[serde(deserialize_with = "crate::protocol::unit_interval")]
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
pub fn worker_root() -> anyhow::Result<PathBuf> {
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

/// One artifact-download client per process (connection reuse), with the
/// packet-37 connect/read timeouts baked in (see artifact_fetch.rs).
static ARTIFACT_CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
fn artifact_client() -> reqwest::Client {
    ARTIFACT_CLIENT
        .get_or_init(artifact_fetch::artifact_client)
        .clone()
}

/// PACKET 37: production hashing moved into artifact_fetch (streaming).
/// Retained for tests (fixture sha computation).
#[cfg(test)]
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
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
        // LRU last-use (cache.rs): atime is unreliable (relatime/noatime),
        // so the hit itself records recency. Best-effort — a failed touch
        // must not fail a cache hit; the fallback is the dir mtime.
        crate::cache::touch_entry_marker(&dir);
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
    // PACKET 37: stream to disk + hash-as-we-write. Previously the whole
    // artifact was buffered in RAM (~340 MB peak for the browser) via a
    // bare reqwest::get with NO timeout of any kind — a stalled body held
    // the job slot (and SIGTERM via the drain) forever. The client bounds
    // connect/read; the per-request total timeout bounds the transfer;
    // the sha is computed over the same streamed bytes it writes, so peak
    // RSS is one chunk.
    let tar_path = tmp.join("artifact.tar.gz");
    let actual =
        artifact_fetch::download_to_file_hashed(&artifact_client(), get_url, &tar_path, kind)
            .await?;
    // SECURITY ORDERING (unchanged): the sha is verified over the bytes on
    // disk BEFORE anything is extracted, executed, or renamed into the
    // cache. A mismatch removes the temp dir and fails the job.
    if actual != sha256 {
        if let Err(e) = tokio::fs::remove_dir_all(&tmp).await {
            tracing::warn!(path = %tmp.display(), error = %e, "removing torn download failed");
        }
        return Err(anyhow!(
            "{kind} sha mismatch: expected {sha256}, got {actual}"
        ));
    }

    artifact_fetch::extract_tar_capped(&tar_path, &tmp, kind).await?;
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
///   echo "$$ $(ps -o lstart= -p $$)" >> <pidfile>
///   exec "<real-executable>" "$@"
///
/// Remotion spawns this script as the "browser executable"; the script's pid
/// is written BEFORE `exec` replaces it with the real Chrome, together with
/// the process START TIME (D-8): `exec` keeps both pid and start time, so the
/// pair identifies THIS Chrome and not whatever the OS later hands the same
/// pid to. Without `ps` the line degrades to the bare pid (see the sweep). Because
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
            "#!/bin/sh\necho \"$$ $(ps -o lstart= -p $$ 2>/dev/null)\" >> {pidfile}\nexec \"{real}\" \"$@\"\n",
            pidfile = pidfile.display(),
            real = real_executable.display(),
        ),
    )
    .with_context(|| format!("write browser wrapper {}", wrapper.display()))?;
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755))
        .with_context(|| format!("chmod browser wrapper {}", wrapper.display()))?;
    Ok((wrapper, pidfile))
}

/// One pidfile line: `<pid>` (legacy) or `<pid> <process start time>` as
/// written by the exec wrapper (D-8). Whitespace in the start time is
/// normalised so `ps` padding cannot defeat the comparison.
#[cfg(unix)]
fn parse_pid_line(line: &str) -> Option<(u32, Option<String>)> {
    let mut tokens = line.split_whitespace();
    let pid = tokens.next()?.parse::<u32>().ok()?;
    let identity = tokens.collect::<Vec<_>>().join(" ");
    Some((
        pid,
        if identity.is_empty() {
            None
        } else {
            Some(identity)
        },
    ))
}

/// The live process's start time as `ps` prints it (`lstart`), whitespace
/// normalised — the identity the wrapper recorded at exec. None when the
/// pid is gone or `ps` is unavailable; both mean "do not trust this pid".
#[cfg(unix)]
fn process_start_time(pid: u32) -> Option<String> {
    let out = std::process::Command::new("ps")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Kill every browser pid recorded by the job's exec wrapper.
///
/// Each recorded pid is a group leader BY CONSTRUCTION (detached spawn), so
/// killing its group takes Chrome's whole tree in one signal — the same
/// semantic as Remotion's own `process.kill(-proc.pid, 'SIGKILL')`.
///
/// Safety mirrors `terminate_child`'s runtime guard: never pid 0, never the
/// supervisor's own pid, and (unlike the group kill) also never a pid that
/// no longer belongs to us — verified by START TIME (D-8), not just liveness. A dead pid is simply skipped — ESRCH means the
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
        let Some((pid, recorded)) = parse_pid_line(line) else {
            continue; // tolerate junk lines rather than aborting the sweep
        };
        if pid == 0 || pid == me {
            // Paranoia identical to terminate_child: a corrupted pidfile
            // must never be able to kill the supervisor or its group.
            tracing::warn!(pid, "browser pidfile contains unsafe pid — skipping");
            continue;
        }
        match recorded {
            Some(recorded) => {
                // D-8 (audit R-9): the pid may have been RECYCLED since the
                // wrapper recorded it. A group SIGKILL on a recycled pid that
                // now leads someone else's group is catastrophic, so signal
                // only when the live process still has the recorded start
                // time. A missing/unreadable start time means "gone or
                // unknowable" — err toward NOT killing.
                match process_start_time(pid) {
                    Some(live) if live == recorded => {}
                    Some(live) => {
                        tracing::warn!(
                            pid,
                            %recorded,
                            %live,
                            "browser pid was recycled by the OS — NOT signalling"
                        );
                        continue;
                    }
                    None => continue, // already gone (ESRCH-equivalent) — success
                }
            }
            None => {
                // Legacy / `ps`-less line: no identity to verify. Keep the
                // pre-D-8 behaviour but say so — this is the unverified path.
                if !pid_exists(pid) {
                    continue;
                }
                tracing::warn!(
                    pid,
                    "browser pid has no recorded identity — signalling unverified"
                );
            }
        }
        let rc = unsafe { libc::killpg(pid as libc::pid_t, libc::SIGKILL) };
        tracing::info!(pid, rc, "browser group SIGKILL (recorded at exec)");
        killed.push(pid);
    }
    killed
}

/// pid > 0 is alive (kill(pid, 0) == 0) — ESRCH means dead, anything else
/// (e.g. EPERM) is treated as alive: err toward NOT killing a foreign pid.
#[cfg(unix)]
fn pid_exists(pid: u32) -> bool {
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

/// Apparent-size sum of a directory tree, tolerant of files that
/// vanish mid-walk (a render deleting intermediates while we sample) and of
/// symlinked directories (never followed: a workdir symlink escaping the
/// workdir would make the "cap" measure some other tree). Walks with an
/// explicit heap stack (D-9, same shape as cache/sweep) — workdir trees can
/// be nested arbitrarily deep and recursion would overflow the stack.
fn workdir_bytes(path: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue, // vanished or unreadable: sample what exists
        };
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else {
                continue; // vanished mid-walk
            };
            if meta.is_dir() {
                stack.push(entry.path());
            } else if meta.is_file() {
                total += meta.len();
            }
        }
    }
    total
}

/// Run one assigned job to completion.
///
/// `wall_clock_limit` and `workdir_cap_bytes` are PARAMETERS rather than
/// globals read inside the loop so tests can drive short/small limits without
/// shortening every other job in the process. The test binary runs tests in
/// parallel, so a process-global override (env var or OnceLock) set by one
/// test would also cap the 10s cancel-grace tests running beside it and make
/// them flake. Production passes [`max_job_wall_time`] and
/// [`max_workdir_bytes`].
/// The cache keys an assignment pins: its payload sha, browser sha (if
/// any), and bundle sha. Used for in-flight protection in the cache sweep.
pub fn cache_keys_for(assign: &JobAssignMessage) -> Vec<String> {
    let mut keys = vec![format!("payloads:{}", assign.payload_sha256)];
    if let Some(sha) = assign.browser_sha256.as_deref() {
        keys.push(format!("browsers:{sha}"));
    }
    keys.push(format!("bundles:{}", assign.bundle_sha256));
    keys
}

/// Variables the runner child may inherit. Everything else is DROPPED.
///
/// The supervisor's environment is the operator's shell, and it routinely
/// carries fleet credentials (`WORKER_TOKEN=… decent start`) and cloud
/// secrets (AWS_*/GITHUB_TOKEN/NPM_TOKEN/…) that no render — and no Chrome
/// under a render — has any business reading. Allow by exact name or by
/// prefix; deny everything else (an allowlist, deliberately NOT a denylist:
/// an unknown secret variable must not get a permanent entry on some list).
pub(crate) fn child_env(parent: impl Iterator<Item = (String, String)>) -> Vec<(String, String)> {
    const EXACT: &[&str] = &[
        "PATH",
        "HOME",
        "TMPDIR",
        "TMP",
        "TEMP",
        "LANG",
        "LANGUAGE",
        "USER",
        "LOGNAME",
        "SHELL",
        "TZ",
        "DISPLAY",
        "WAYLAND_DISPLAY",
        "LD_LIBRARY_PATH",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
        "NODE_EXTRA_CA_CERTS",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "NO_PROXY",
        "http_proxy",
        "https_proxy",
        "no_proxy",
    ];
    const PREFIXES: &[&str] = &[
        "LC_",
        "XDG_",
        "FONTCONFIG_",
        "DECENT_",
        "REMOTION_",
        "CHROME_",
        "PUPPETEER_",
    ];
    parent
        .filter(|(k, _)| EXACT.contains(&k.as_str()) || PREFIXES.iter().any(|p| k.starts_with(p)))
        .collect()
}

/// Clear whatever the supervisor inherited and install the allowlisted
/// environment (see [`child_env`]). Caller-set variables (e.g.
/// DECENT_BROWSER_EXECUTABLE) are applied AFTER this so they still win.
fn apply_child_env(command: &mut Command, parent: impl Iterator<Item = (String, String)>) {
    command.env_clear();
    for (k, v) in child_env(parent) {
        command.env(k, v);
    }
}

pub async fn run_job(
    assign: JobAssignMessage,
    mut cancel_rx: oneshot::Receiver<()>,
    tx: tokio::sync::mpsc::UnboundedSender<WorkerMessage>,
    wall_clock_limit: Duration,
    workdir_cap_bytes: u64,
) {
    let job_id = assign.job_id.clone();
    let tenant = assign.tenant.clone();
    let output_key = assign.output_key.clone();
    let attempt = assign.attempt;
    // D-11: hold the keep-awake guard HERE (not inside run_job_inner) and
    // release it on the blocking pool after the body — the Drop teardown is
    // a blocking TERM → wait → KILL poll. Still awaited BEFORE the terminal
    // frame goes out, so the assertion provably covers the whole render.
    let keep_awake = crate::keepawake::JobKeepAwake::acquire(&job_id);
    let result = run_job_inner(
        assign,
        &mut cancel_rx,
        tx.clone(),
        wall_clock_limit,
        workdir_cap_bytes,
    )
    .await;
    keep_awake.release_blocking().await;
    match result {
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
    wall_clock_limit: Duration,
    workdir_cap_bytes: u64,
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
    // Sleep assertion (packet 18): held for EXACTLY this job's lifetime —
    // acquired once the job is committed (in run_job, above) and released
    // there via release_blocking after this body returns: success, failure,
    // cancel, wall-clock, disk cap, silence, stdout death — and this
    // function being unwound by ? anywhere. Never held while idle:
    // run_job_inner returning is what releases it.
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
    // PACKET 67: the child inherits an ALLOWLISTED environment, never the
    // supervisor's shell — `WORKER_TOKEN=… decent start` hands the fleet
    // credential to every render otherwise. `env_clear` first, then the
    // allowlist; DECENT_BROWSER_EXECUTABLE below is applied after this and
    // therefore still wins.
    // `vars_os`, not `vars`: `std::env::vars()` PANICS on a non-UTF-8 value,
    // and an operator's shell can carry one. Such a variable is simply not
    // inherited — nothing on the allowlist is legitimately non-UTF-8.
    apply_child_env(
        &mut command,
        std::env::vars_os()
            .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?))),
    );
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
    // PACKET 37 (audit 3b): the frame is serialized BEFORE the spawn —
    // pure serialization of our own typed struct, so doing it early removes
    // two `?`s from the spawn→select window where an early return would
    // orphan the child (kill_on_drop is not set) while WorkDir::drop purges
    // the directory under it.
    let mut input_frame = serde_json::to_value(&assign).context("serialize jobAssign frame")?;
    if let serde_json::Value::Object(ref mut obj) = input_frame {
        obj.insert("type".into(), serde_json::Value::String("jobAssign".into()));
    }
    let input = serde_json::to_vec(&input_frame).context("encode jobAssign frame")?;
    let mut child = command
        .current_dir(workdir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("spawn runner {}", runner.display()))?;
    let mut stdin = match child.stdin.take() {
        Some(s) => s,
        None => {
            // Kill BEFORE returning: same deliberate ordering as the
            // stdout-error branch below (audit 3b's reference pattern).
            terminate_child(&mut child, browser_pidfile.as_deref()).await;
            return Err(anyhow!("runner stdin missing"));
        }
    };
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

    let stdout = match child.stdout.take() {
        Some(s) => s,
        None => {
            terminate_child(&mut child, browser_pidfile.as_deref()).await;
            return Err(anyhow!("runner stdout missing"));
        }
    };
    let mut lines = BufReader::new(stdout).lines();
    let mut done_metrics: Option<JobMetrics> = None;

    // Armed once, for the whole job: a per-iteration timer would restart on
    // every progress line and cap nothing.
    let deadline = tokio::time::sleep(wall_clock_limit);
    tokio::pin!(deadline);

    // Workdir disk sampler. Every 2s a recursive apparent-size walk of the
    // workdir runs on the blocking pool. Cost is bounded by FILE COUNT, not
    // bytes: a workdir holds the bundle copy, browser profile and render
    // output — thousands of entries at worst, each a dentry-cached stat, so
    // a sample is single-digit milliseconds and 2s cadence keeps it far below
    // 1% of one core. A tighter interval would buy at most one interval of
    // overrun (a runaway writes hundreds of MB/s regardless); a looser one
    // widens the damage window before the node's disk fills. 2s is the knee.
    let mut disk_sampler = tokio::time::interval(WORKDIR_SAMPLE_INTERVAL);
    disk_sampler.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = &mut deadline => {
                terminate_child(&mut child, browser_pidfile.as_deref()).await;
                drop(workdir);
                tracing::warn!(
                    job_id = %assign.job_id,
                    limit_s = wall_clock_limit.as_secs_f64(),
                    purged = !purged_path.exists(),
                    "render exceeded the wall-clock limit; killed and workdir purged"
                );
                return Err(anyhow!(
                    "render exceeded the {:.0}s wall-clock limit",
                    wall_clock_limit.as_secs_f64()
                ));
            }
            _ = disk_sampler.tick() => {
                // spawn_blocking: the walk is synchronous IO and must not
                // stall the runtime (or the cancel branch beside it).
                let sample_root = purged_path.clone();
                let bytes = match tokio::task::spawn_blocking(move || {
                    workdir_bytes(&sample_root)
                })
                .await
                {
                    Ok(bytes) => bytes,
                    Err(e) => {
                        // PACKET 37 (audit 3b): kill before returning —
                        // same rule as every other ? in this window.
                        terminate_child(&mut child, browser_pidfile.as_deref()).await;
                        return Err(anyhow!(e).context("disk sampler join failed"));
                    }
                };
                if bytes > workdir_cap_bytes {
                    terminate_child(&mut child, browser_pidfile.as_deref()).await;
                    drop(workdir);
                    tracing::warn!(
                        job_id = %assign.job_id,
                        bytes,
                        cap_bytes = workdir_cap_bytes,
                        purged = !purged_path.exists(),
                        "workdir exceeded the disk cap; killed and workdir purged"
                    );
                    return Err(anyhow!(
                        "workdir exceeded the {} byte disk cap ({} bytes on disk)",
                        workdir_cap_bytes,
                        bytes
                    ));
                }
            }
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

    let status = match child.wait().await {
        Ok(status) => status,
        Err(e) => {
            // PACKET 37 (audit 3b): a failed wait leaves the child's state
            // unknown — terminate defensively before Drop purges.
            terminate_child(&mut child, browser_pidfile.as_deref()).await;
            return Err(anyhow!(e).context("runner wait failed"));
        }
    };
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

/// The belt-and-braces signal guard (see `terminate_child`): pid 0 is never
/// a valid target (`killpg(0, …)` means "the caller's own group" — the
/// supervisor and the operator's shell) and the supervisor's own pid must
/// never be signalled even if a refactor hands terminate a pid it did not
/// spawn. Pure so it is unit-testable (packet 66: the `&&` here survived a
/// full mutants run as an inline expression).
fn is_safe_signal_target(pid: u32, me: u32) -> bool {
    pid != 0 && pid != me
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
            let safe = is_safe_signal_target(pid, std::process::id());
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
                let safe = is_safe_signal_target(pid, std::process::id());
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

    /// D-8 (audit R-9): a recorded pid can be RECYCLED by the OS between the
    /// browser's exit and the sweep. Since every recorded pid is signalled as
    /// a process GROUP, a recycled pid that happens to lead another group —
    /// a user's terminal, a daemon — would take that whole group down with
    /// SIGKILL. The pidfile therefore carries the process START TIME next to
    /// the pid, and the sweep signals only when the live process still has
    /// that start time.
    #[cfg(unix)]
    fn spawn_group_leader_sleeper() -> std::process::Child {
        use std::os::unix::process::CommandExt;
        std::process::Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .expect("spawn sleep")
    }

    #[cfg(unix)]
    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("decent-d8-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[cfg(unix)]
    #[test]
    fn pid_lines_parse_pid_and_optional_identity() {
        assert_eq!(super::parse_pid_line("123"), Some((123, None)));
        assert_eq!(
            super::parse_pid_line("123 Wed Sep  2 01:03:24 2026"),
            Some((123, Some("Wed Sep 2 01:03:24 2026".to_string())))
        );
        assert_eq!(super::parse_pid_line("  123   "), Some((123, None)));
        assert_eq!(super::parse_pid_line("junk"), None);
        assert_eq!(super::parse_pid_line(""), None);
        assert_eq!(super::parse_pid_line("12x 3"), None);
    }

    #[cfg(unix)]
    #[test]
    fn a_recycled_pid_with_a_different_start_time_is_never_signalled() {
        let dir = scratch("recycled");
        let mut child = spawn_group_leader_sleeper();
        let pid = child.id();
        // Recorded identity from "another life" of this pid.
        std::fs::write(dir.join("pids"), format!("{pid} Sat Jan 1 00:00:00 2000\n")).unwrap();
        let killed = super::kill_recorded_browsers(&dir.join("pids"));
        assert!(
            killed.is_empty(),
            "a recycled pid was signalled: {killed:?}"
        );
        assert!(
            child.try_wait().unwrap().is_none(),
            "the unrelated group leader must still be alive"
        );
        child.kill().ok();
        child.wait().ok();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn a_pid_whose_start_time_matches_is_group_killed() {
        let dir = scratch("match");
        let mut child = spawn_group_leader_sleeper();
        let pid = child.id();
        let start = super::process_start_time(pid).expect("ps reports a start time for a live pid");
        std::fs::write(dir.join("pids"), format!("{pid} {start}\n")).unwrap();
        let killed = super::kill_recorded_browsers(&dir.join("pids"));
        assert_eq!(killed, vec![pid]);
        let status = child.wait().unwrap();
        assert!(
            !status.success(),
            "sleeper must have died from SIGKILL: {status:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn a_legacy_pid_only_line_still_kills_but_is_the_unverified_path() {
        let dir = scratch("legacy");
        let mut child = spawn_group_leader_sleeper();
        let pid = child.id();
        std::fs::write(dir.join("pids"), format!("{pid}\n")).unwrap();
        let killed = super::kill_recorded_browsers(&dir.join("pids"));
        assert_eq!(killed, vec![pid]);
        child.wait().ok();
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The wrapper itself must record the identity, not just the pid —
    /// otherwise the sweep can only ever take the unverified path.
    #[cfg(unix)]
    #[test]
    fn the_exec_wrapper_records_pid_and_start_time() {
        let dir = scratch("wrapper");
        let (wrapper, pidfile) =
            super::write_browser_wrapper(&dir, std::path::Path::new("/usr/bin/true")).unwrap();
        let status = std::process::Command::new(&wrapper)
            .status()
            .expect("run wrapper");
        assert!(status.success());
        let contents = std::fs::read_to_string(&pidfile).unwrap();
        let (pid, identity) =
            super::parse_pid_line(contents.lines().next().unwrap_or("")).expect("pid line");
        assert!(pid > 0);
        assert!(
            identity.is_some(),
            "wrapper must record the start time next to the pid: {contents:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── PACKET 67: the runner child's allowlisted environment ─────────────

    #[test]
    fn child_env_drops_everything_not_allowlisted() {
        let parent = [
            ("PATH", "/usr/bin:/bin"),
            ("HOME", "/home/op"),
            ("WORKER_TOKEN", "fleet-credential"),
            ("AWS_SECRET_ACCESS_KEY", "aws-secret"),
            ("GITHUB_TOKEN", "gh-token"),
            ("LC_ALL", "en_US.UTF-8"),
            ("XDG_RUNTIME_DIR", "/run/user/1000"),
            ("DECENT_FOO", "x"),
        ];
        let child = child_env(parent.iter().map(|(k, v)| (k.to_string(), v.to_string())));
        let mut names: Vec<&str> = child.iter().map(|(k, _)| k.as_str()).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            vec!["DECENT_FOO", "HOME", "LC_ALL", "PATH", "XDG_RUNTIME_DIR"],
            "exactly the allowlisted names survive: {child:?}"
        );
    }

    /// The REAL env-building code path (the same `apply_child_env` the spawn
    /// calls) must leave the runner script an environment WITHOUT the
    /// operator's secrets and WITH the basics. The parent iterator is passed
    /// explicitly (no `std::env::set_var` — this binary runs tests in
    /// parallel and no other runner.rs test reads env).
    #[cfg(unix)]
    #[tokio::test]
    async fn spawned_runner_does_not_see_the_supervisors_secrets() {
        let dir = std::env::temp_dir().join(format!(
            "p67-env-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("runner-script.sh");
        std::fs::write(
            &script,
            "#!/bin/sh
env
",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let dir_str = dir.display().to_string();
        let parent: [(&str, &str); 3] = [
            ("PATH", "/usr/bin:/bin"),
            ("WORKER_TOKEN", "leak"),
            ("HOME", dir_str.as_str()),
        ];
        let mut command = Command::new(&script);
        apply_child_env(
            &mut command,
            parent
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string())),
        );
        let out = command.output().await.expect("spawn the runner script");
        assert!(out.status.success(), "script failed: {out:?}");
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        assert!(
            !stdout.contains("WORKER_TOKEN"),
            "the fleet credential leaked into the runner env: {stdout}"
        );
        assert!(!stdout.contains("leak"), "secret value leaked: {stdout}");
        assert!(stdout.contains("PATH="), "the basics must still be there");
        // And a variable the parent iterator did NOT carry must be GONE —
        // this is what catches a deleted `env_clear()`: without it the child
        // inherits the whole test-process environment. Pick the first env
        // key that fails the allowlist (there is always one) and require its
        // absence from the child.
        let allowlisted: std::collections::HashSet<String> = child_env(std::env::vars())
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        let intruder = std::env::vars()
            .map(|(k, _)| k)
            .find(|k| !allowlisted.contains(k) && k.as_str() != "WORKER_TOKEN")
            .expect("the test process has a non-allowlisted env var to probe with");
        assert!(
            !intruder.is_empty(),
            "no non-allowlisted env var found to probe with"
        );
        assert!(
            !stdout.contains(&intruder),
            "the child inherited non-allowlisted {intruder} — env_clear is missing"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// PACKET 66 (N-13 #11-#12): the safe-signal predicate is pinned — pid 0
    /// ("the caller's own group" under killpg) and the supervisor's own pid
    /// are never signalled; any other pid is. Kills the `&&` → `||` mutants:
    /// the OR reads "safe" for pid 0 and for our own pid.
    #[test]
    fn safe_signal_target_rejects_zero_and_self_accepts_others() {
        let me = std::process::id();
        assert!(
            !is_safe_signal_target(0, me),
            "pid 0 is never a valid target"
        );
        assert!(
            !is_safe_signal_target(me, me),
            "our own pid is never a valid target"
        );
        assert!(
            is_safe_signal_target(4_194_304, me),
            "an unrelated pid is a valid target"
        );
    }

    /// C-3 / audit U-12: the runner↔supervisor stdout contract is pinned by
    /// the SAME fixture file the TS side (runner-core's conformance test)
    /// asserts against — packages/protocol/fixtures/runner-stdout-v1.json.
    /// Every accept case must deserialise into `RunnerEvent`; every reject
    /// case must FAIL. A case that parses here but not there (or vice
    /// versa) is exactly the drift the fixtures exist to catch — the first
    /// run of this test caught one: the old lenient `progress: f64` happily
    /// accepted progress 1.5, which dispatch's protocol-v2 schema refuses.
    #[test]
    fn runner_stdout_fixtures_round_trip() {
        use std::fs;
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../packages/protocol/fixtures/runner-stdout-v1.json"
        );
        let raw = fs::read_to_string(path).expect("runner-stdout-v1.json must exist");
        let parsed: serde_json::Value =
            serde_json::from_str(&raw).expect("fixtures are valid JSON");
        let accept = parsed["accept"].as_array().expect("accept array");
        let reject = parsed["reject"].as_array().expect("reject array");
        assert!(!accept.is_empty(), "accept fixture set must be non-empty");
        assert!(!reject.is_empty(), "reject fixture set must be non-empty");

        for case in accept {
            let name = case["name"].as_str().expect("accept case name");
            let wire = &case["wire"];
            let event: RunnerEvent = serde_json::from_value(wire.clone())
                .unwrap_or_else(|e| panic!("accept case {name} must parse into RunnerEvent: {e}"));
            // The tag survives: the parsed variant matches the fixture's type.
            let expected = wire["type"].as_str().expect("wire type");
            let actual = match &event {
                RunnerEvent::Progress { .. } => "progress",
                RunnerEvent::Heartbeat => "heartbeat",
                RunnerEvent::Done { .. } => "done",
                RunnerEvent::Error { .. } => "error",
            };
            assert_eq!(
                actual, expected,
                "accept case {name} parsed to the wrong variant"
            );
        }

        // Collect every accepted entry so one run names ALL the loose bounds.
        let mut accepted = Vec::new();
        for case in reject {
            let name = case["name"].as_str().expect("reject case name");
            let wire = &case["wire"];
            if serde_json::from_value::<RunnerEvent>(wire.clone()).is_ok() {
                accepted.push(name.to_string());
            }
        }
        assert!(
            accepted.is_empty(),
            "negative runner-stdout fixtures were ACCEPTED:\n  {}",
            accepted.join("\n  ")
        );
    }

    /// `DECENT_MAX_WORKDIR_BYTES` parsing. Drives the pure parse directly —
    /// NOT by mutating the env: the test binary is parallel and connection
    /// tests spawn real jobs whose caps are read from the process env, so a
    /// set_var in one test is a flake in another. The parse is factored out
    /// for exactly this reason (same pattern as WORKER_ROOT_OVERRIDE).
    ///
    /// The brief said the wall-time sibling "has parsing tests" to copy — it
    /// has none (verified by grep at packet-12 time), so both functions are
    /// pinned here.
    #[test]
    fn workdir_bytes_override_parsing() {
        // unset → default
        assert_eq!(parse_workdir_bytes_override(None), MAX_WORKDIR_BYTES);
        // garbage → default
        assert_eq!(
            parse_workdir_bytes_override(Some("not-a-number")),
            MAX_WORKDIR_BYTES
        );
        // below the 1 MiB floor → floor, never the raw value
        assert_eq!(
            parse_workdir_bytes_override(Some("1024")),
            MAX_WORKDIR_BYTES
        );
        assert_eq!(
            parse_workdir_bytes_override(Some(&(1024 * 1024 - 1).to_string())),
            MAX_WORKDIR_BYTES
        );
        // exactly the floor → honored (a floor the boundary itself fails
        // would surprise every operator who sets exactly 1 MiB)
        assert_eq!(
            parse_workdir_bytes_override(Some(&(1024 * 1024).to_string())),
            1024 * 1024
        );
        // valid above-floor → value
        assert_eq!(parse_workdir_bytes_override(Some("5368709120")), 5368709120);
    }

    /// Same pins for `DECENT_MAX_JOB_WALL_TIME_MS` (floor 50ms).
    #[test]
    fn job_wall_time_override_parsing() {
        assert_eq!(parse_job_wall_time_override(None), MAX_JOB_WALL_TIME);
        assert_eq!(
            parse_job_wall_time_override(Some("soon")),
            MAX_JOB_WALL_TIME
        );
        assert_eq!(parse_job_wall_time_override(Some("49")), MAX_JOB_WALL_TIME);
        assert_eq!(
            parse_job_wall_time_override(Some("50")),
            Duration::from_millis(50)
        );
        assert_eq!(
            parse_job_wall_time_override(Some("1500")),
            Duration::from_millis(1500)
        );
    }

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

    // ── PACKET 37: download verification path ──────────────────────────────

    /// Minimal one-shot HTTP file server on 127.0.0.1:0, serving `body`
    /// at every path. Returns the base URL. Runs on the tokio runtime the
    /// test already has; shut down by dropping the JoinHandle (the
    /// listener closes when the task is dropped).
    async fn serve_bytes(body: Vec<u8>) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let body = body.clone();
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = vec![0u8; 8192];
                    // drain the request head
                    let _ = socket.read(&mut buf).await;
                    let header = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/octet-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = socket.write_all(header.as_bytes()).await;
                    let _ = socket.write_all(&body).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        (format!("http://{addr}/artifact.tar.gz"), handle)
    }

    /// A valid tar.gz containing a single `decent-render-runner` marker file.
    fn make_payload_tarball(runner_contents: &[u8]) -> Vec<u8> {
        // tar with a 512-byte header + padded content + two zero blocks,
        // then gzip -9 via flate2? No extra dep: shell out to tar+gzip on
        // the test's scratch dir (unix; matches production's system tar).
        let dir = std::env::temp_dir().join(format!(
            "p37-tar-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("decent-render-runner"), runner_contents).unwrap();
        let out = std::process::Command::new("tar")
            .arg("-czf")
            .arg("-")
            .arg("-C")
            .arg(&dir)
            .arg("decent-render-runner")
            .output()
            .expect("tar available");
        std::fs::remove_dir_all(&dir).ok();
        assert!(out.status.success());
        out.stdout
    }

    fn unique_worker_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "p37-worker-root-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    /// THE deletion-mutant survivor (audit 2d): every existing test seeded
    /// the cache so the sha-verify block never ran. This test downloads a
    /// REAL tarball whose bytes do not hash to the claimed sha and asserts
    /// the failure + that nothing was cached or left behind. Removing the
    /// `if actual != sha256` block in ensure_artifact makes it FAIL.
    #[tokio::test]
    async fn sha_mismatch_downloads_are_rejected_and_not_cached() {
        // NOTE: WORKER_ROOT_OVERRIDE is a process-global OnceLock (first write
        // wins) and the test binary is parallel — both packet-37 download
        // tests must agree on ONE root. They use the same tag; whichever
        // runs first sets it, the other reuses it. Distinguish entries by
        // the claimed sha (unique per test).
        let root = unique_worker_root("dl");
        set_worker_root_for_tests(root.clone());
        let tarball = make_payload_tarball(b"#!/bin/sh\necho runner\n");
        let claimed_sha = sha256_hex(b"not-the-tarball");
        let (url, server) = serve_bytes(tarball.clone()).await;

        let err = ensure_artifact("payloads", &claimed_sha, &url, "decent-render-runner")
            .await
            .expect_err("sha mismatch must fail the artifact");
        assert!(err.to_string().contains("sha mismatch"), "got: {err}");
        // Nothing cached under the claimed sha...
        assert!(!root.join("payloads").join(&claimed_sha).exists());
        // ...and THIS test's torn download temp dir (.<claimed_sha>-download)
        // is removed, not left behind. Scoped to our sha: the sibling
        // sha-match test may be mid-download in the shared OnceLock root
        // (tests run in parallel; its temp dir is legitimately transient).
        let torn = root
            .join("payloads")
            .join(format!(".{claimed_sha}-download"));
        assert!(
            !torn.exists(),
            "torn download temp dir left behind: {}",
            torn.display()
        );
        server.abort();
        // root intentionally NOT removed: it is a shared OnceLock root
        // across the packet-37 download tests; /tmp reclamation covers it.
    }

    /// The counterpart happy path: correct sha → cached, marker present.
    #[tokio::test]
    async fn sha_match_downloads_are_cached() {
        let root = unique_worker_root("dl");
        set_worker_root_for_tests(root.clone());
        let tarball = make_payload_tarball(b"#!/bin/sh\necho runner\n");
        let sha = sha256_hex(&tarball);
        let (url, server) = serve_bytes(tarball).await;

        let dir = ensure_artifact("payloads", &sha, &url, "decent-render-runner")
            .await
            .expect("valid artifact");
        assert!(dir.join("decent-render-runner").exists());
        // (cache-internal markers are not asserted — they belong to cache.rs)
        server.abort();
        // root intentionally NOT removed: it is a shared OnceLock root
        // across the packet-37 download tests; /tmp reclamation covers it.
    }

    // D-9 sibling: `workdir_bytes`' walk must not recurse either. Same
    // tiny-stack trick as the sweep.rs test — the OS caps the chain depth
    // (PATH_MAX) below what could overflow a default test stack, so a
    // recursive walker is only caught by a deliberately small stack (the
    // abort IS the red); the explicit heap stack needs constant thread
    // stack, however deep the chain.
    #[test]
    fn workdir_bytes_sums_deep_chains_without_recursing() {
        let tmp = tempfile::tempdir().unwrap();
        let mut p = tmp.path().to_path_buf();
        let mut depth = 0u32;
        loop {
            let next = p.join("d");
            if std::fs::create_dir(&next).is_err() {
                break; // OS refused (PATH_MAX): deepest chain available
            }
            p = next;
            depth += 1;
        }
        // Same PATH_MAX headroom for the leaf file name.
        let mut leaf = p.join("leaf.bin");
        let payload = vec![0x42u8; 12345];
        while std::fs::write(&leaf, &payload).is_err() {
            p = p.parent().unwrap().to_path_buf();
            depth -= 1;
            assert!(depth > 0, "OS allowed no usable chain depth");
            leaf = p.join("leaf.bin");
        }
        std::fs::write(&leaf, &payload).unwrap();
        eprintln!("chain depth the OS allowed: {depth}");

        let root = tmp.path().to_path_buf();
        let total = std::thread::Builder::new()
            .stack_size(32 * 1024)
            .spawn(move || workdir_bytes(&root))
            .unwrap()
            .join()
            .expect("walker must not overflow its stack");
        // The only file in the tree; an exact sum proves the walk reached
        // the bottom (dirs contribute nothing in the file branch).
        assert_eq!(total, payload.len() as u64);
    }
}
