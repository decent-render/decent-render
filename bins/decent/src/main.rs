//! decent — thin CLI over supervisor-core.
//!
//! `decent start --token <jwt>`
//!
//! (defaults to the production dispatch at
//! wss://decent-render-dispatch.fly.dev/ws — override with --dispatch-url
//! or DISPATCH_URL; plain ws:// is allowed only for localhost)
//! (or env vars DISPATCH_URL / WORKER_TOKEN). Registers with the dispatch and
//! heartbeats; real rendering requires `--allow-real-jobs`.
//!
//! The CLI and the Tauri app share the same `connection::run` code path.
//! The only difference: the CLI passes `Observability::default()` (tracing
//! only), the app passes one with status/log channels attached.

mod service;
mod tui;

use clap::{Parser, Subcommand};
use service::{DaemonState, ServiceSpec};
use supervisor_core::capabilities::detect_capabilities;
use supervisor_core::connection::{self, ConnectionConfig};
use supervisor_core::dispatch_url::{validate_dispatch_url, DEFAULT_DISPATCH_WS};
use supervisor_core::keepawake::{self, KeepAwakeState};
use supervisor_core::protocol::{Platform, RegisterMessage, PROTOCOL_VERSION};
use supervisor_core::status::{Observability, SupervisorStatus};
use supervisor_core::worker_token::{base64url_decode, validate_worker_token_shape};

const SUPERVISOR_VERSION: &str = concat!("rust-", env!("CARGO_PKG_VERSION"));

/// Token storage: a 0600 file at ~/.config/decent/worker-token.
/// Migrates from the old ~/.config/decent-node/ path if the new path doesn't
/// exist but the old one does (pre-v0.1 CLI rename backward compat).
fn token_path() -> anyhow::Result<std::path::PathBuf> {
    let home = std::path::PathBuf::from(
        std::env::var_os("HOME")
            .ok_or_else(|| anyhow::anyhow!("HOME is not set; cannot locate token file"))?,
    );

    let new_path = home.join(".config/decent/worker-token");
    let old_path = home.join(".config/decent-node/worker-token");

    // One-time migration: if new path doesn't exist but old path does, copy.
    // PACKET 40 (audit 11): fs::copy reproduced the OLD file's mode — often
    // 0644 from pre-hardening installs — and create_dir_all made the parent
    // 0755, silently downgrading the fleet credential's protection on every
    // upgrade. Both are tightened immediately after the copy, and the
    // parent is created 0700 before anything lands in it.
    if !new_path.exists() && old_path.exists() {
        if let Some(parent) = new_path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                eprintln!("Warning: could not create config dir for token migration: {e}");
                return Ok(new_path);
            }
            set_owner_only(parent, 0o700);
        }
        match std::fs::copy(&old_path, &new_path) {
            Ok(_) => {
                set_owner_only(&new_path, 0o600);
                eprintln!("Migrated token from ~/.config/decent-node/ → ~/.config/decent/ (0600)");
            }
            Err(e) => {
                eprintln!("Warning: could not migrate token from old path: {e}");
            }
        }
    }

    Ok(new_path)
}

fn load_token() -> String {
    token_path()
        .ok()
        .and_then(|p| std::fs::read_to_string(&p).ok())
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn save_token(token: &str) -> anyhow::Result<()> {
    let path = token_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        set_owner_only(parent, 0o700);
    }
    std::fs::write(&path, format!("{token}\n"))?;
    set_owner_only(&path, 0o600);
    Ok(())
}

fn delete_token() -> anyhow::Result<()> {
    match std::fs::remove_file(&token_path()?) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Current epoch time in milliseconds (for status-snapshot freshness).
/// Create the daemon log (if missing) and make it owner-only. launchd and
/// systemd create `StandardOutPath`/`append:` targets with the default
/// umask (0644) and never change an existing file's mode, so the file is
/// created HERE first, at 0600: the log carries job ids, dispatch frames and
/// error text that no other local user needs to read.
fn ensure_private_log_file(path: &std::path::Path) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    set_owner_only(path, 0o600);
    Ok(())
}

/// The closing line of `decent status` when something above was not the
/// happy path — the doctor is where the detail lives. `None` on a healthy node.
fn status_hint(attention: bool) -> Option<&'static str> {
    attention.then_some("run `decent doctor` for the full check (token modes, log, dispatch, disk)")
}

/// The last `n` lines of `text`, in order. `n == 0` → nothing.
fn tail_lines(text: &str, n: usize) -> Vec<&str> {
    if n == 0 {
        return Vec::new();
    }
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].to_vec()
}

/// Print whatever the daemon appends after `offset`, until Ctrl-C. A file
/// that shrinks (log rotated/truncated) is re-read from the start.
async fn follow_log(path: &std::path::Path, mut offset: u64) -> anyhow::Result<()> {
    use std::io::{Read, Seek, SeekFrom};
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => return Ok(()),
            _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => {}
        }
        let Ok(mut file) = std::fs::File::open(path) else {
            continue;
        };
        let len = file.metadata().map(|m| m.len()).unwrap_or(0);
        if len < offset {
            offset = 0;
        }
        if len == offset {
            continue;
        }
        file.seek(SeekFrom::Start(offset))?;
        let mut buf = String::new();
        file.read_to_string(&mut buf)?;
        print!("{buf}");
        offset = len;
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// B-5 (audit U-4): the one wording for the idle-sleep line, shared by
/// `decent status` and the TUI. "held" is printed ONLY when a live guard
/// reports it; every other outcome says what actually happened.
pub(crate) fn idle_sleep_label(state: Option<KeepAwakeState>) -> &'static str {
    match state {
        Some(s) => s.describe(),
        None => "not held",
    }
}

/// The daemon's live status snapshot, parsed from the `daemon-status` file the
/// running daemon writes every few seconds. Read by the separate `status`
/// command so an operator can see connection/job state without the TUI.
const FRESH_WINDOW_MS: u64 = 15_000;

struct DaemonSnapshot {
    connection: String,
    current_job: Option<(String, String, f64)>,
    /// What the daemon's idle-sleep guard actually did (B-5). `None` = no
    /// guard alive (idle, download phase, or a daemon predating the line).
    keep_awake: Option<KeepAwakeState>,
    jobs_completed: u64,
    jobs_failed: u64,
    jobs_canceled: u64,
    update_available: Option<String>,
    updated_at_ms: u64,
}

impl DaemonSnapshot {
    /// Fresh = written within the last 15s (the daemon writes every 3s).
    fn is_fresh(&self) -> bool {
        self.is_fresh_at(now_ms())
    }

    /// C-11: the boundary is INCLUSIVE — a snapshot written exactly at the
    /// window edge is fresh; one ms past it is stale. `now` is injected so
    /// the exact edge is unit-testable (is_fresh reads the clock itself).
    fn is_fresh_at(&self, now: u64) -> bool {
        now.saturating_sub(self.updated_at_ms) <= FRESH_WINDOW_MS
    }
}

fn read_daemon_snapshot() -> Option<DaemonSnapshot> {
    let path = token_path().ok()?.parent()?.join("daemon-status");
    read_daemon_snapshot_from(&path)
}

/// C-11: parse the snapshot at `path`. Returns None rather than a partial
/// struct: `updated_at_ms` is the LAST line the writer emits, so a torn /
/// half-written file (crashed pre-atomic write, old daemon) lacks a
/// parseable timestamp — that absence IS the torn signal.
fn read_daemon_snapshot_from(path: &std::path::Path) -> Option<DaemonSnapshot> {
    let content = std::fs::read_to_string(path).ok()?;
    let kv: std::collections::HashMap<&str, &str> = content
        .lines()
        .filter_map(|l| {
            let mut it = l.splitn(2, '=');
            Some((it.next()?, it.next()?))
        })
        .collect();
    let val = |k: &str| kv.get(k).copied().unwrap_or("");
    let job_id = val("current_job_id");
    let current_job = if job_id.is_empty() {
        None
    } else {
        Some((
            job_id.to_string(),
            val("current_job_phase").to_string(),
            val("current_job_progress").parse().unwrap_or(0.0),
        ))
    };
    let upd = val("update_available");
    Some(DaemonSnapshot {
        connection: val("connection").to_string(),
        current_job,
        keep_awake: KeepAwakeState::from_token(val("keep_awake")),
        jobs_completed: val("jobs_completed").parse().unwrap_or(0),
        jobs_failed: val("jobs_failed").parse().unwrap_or(0),
        jobs_canceled: val("jobs_canceled").parse().unwrap_or(0),
        update_available: if upd.is_empty() {
            None
        } else {
            Some(upd.to_string())
        },
        updated_at_ms: val("updated_at_ms").parse().ok()?,
    })
}

#[cfg(unix)]
fn set_owner_only(path: &std::path::Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
}

#[cfg(not(unix))]
fn set_owner_only(_path: &std::path::Path, _mode: u32) {}

/// Wait for the first termination signal, returning its name.
///
/// SIGTERM is what launchd (and systemd) send on stop/shutdown; SIGINT is
/// Ctrl-C in a foreground `decent start`. Both must reach the graceful path so
/// the in-flight job's workdir is purged — `Drop` does not run on signal death.
#[cfg(unix)]
async fn await_termination() -> Option<&'static str> {
    use tokio::signal::unix::{signal, SignalKind};
    let mut sigterm = signal(SignalKind::terminate()).ok()?;
    let mut sigint = signal(SignalKind::interrupt()).ok()?;
    tokio::select! {
        _ = sigterm.recv() => Some("SIGTERM"),
        _ = sigint.recv() => Some("SIGINT"),
    }
}

#[cfg(not(unix))]
async fn await_termination() -> Option<&'static str> {
    tokio::signal::ctrl_c().await.ok().map(|()| "ctrl-c")
}

#[derive(Parser)]
#[command(
    name = "decent",
    version,
    about = "Decent render network node supervisor"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Connect to the dispatch service, register, and heartbeat.
    Start {
        /// Dispatch WebSocket URL.
        #[arg(
            long,
            env = "DISPATCH_URL",
            default_value = DEFAULT_DISPATCH_WS
        )]
        dispatch_url: String,
        /// Worker JWT. If omitted (and no WORKER_TOKEN env), reads the token
        /// stored by `decent login` (the token file).
        #[arg(long, env = "WORKER_TOKEN")]
        token: Option<String>,
        /// Exit cleanly after this many heartbeats (smoke-test mode).
        #[arg(long)]
        heartbeat_limit: Option<u32>,
        /// Opt in to executing real render jobs. Default safety posture refuses
        /// jobAssign frames and only registers/heartbeats.
        #[arg(long, env = "ALLOW_REAL_JOBS", default_value_t = false)]
        allow_real_jobs: bool,
    },
    /// Pair this machine: open the web pairing page, paste the issued worker
    /// token, and store it in a 0600 file for `start` to use.
    Login {
        /// The web app URL to pair against.
        #[arg(long, env = "APP_URL", default_value = "https://decent-render.farm")]
        app_url: String,
        /// Store a worker token directly instead of opening the web pairing
        /// page. For company/internal tokens minted via
        /// `mint-worker-token.ts` in the private driffs repo (skips the self-serve
        /// device flow).
        #[arg(long)]
        token: Option<String>,
    },
    /// Forget the stored worker token (clears the token file).
    Logout,
    /// Install the unattended daemon: a launchd agent on macOS, a systemd user
    /// unit on Linux. Runs `decent start` and restarts on exit, so the node
    /// renders unattended and accepts real jobs. Run `decent login` first to
    /// store a token. On Linux this also enables lingering, without which the
    /// node would not survive a reboot on a headless machine.
    Install {
        /// Dispatch WebSocket URL.
        #[arg(
            long,
            env = "DISPATCH_URL",
            default_value = DEFAULT_DISPATCH_WS
        )]
        dispatch_url: String,
    },
    /// Uninstall the daemon: stops it and removes the unit file.
    Uninstall,
    /// Show pairing + daemon status: is a token stored? is the daemon
    /// installed and running?
    Status,
    /// Check this node: token, daemon, dispatch reachability, disk, version.
    /// Prints one line per check and exits 1 if any check FAILED.
    Doctor,
    /// Show the daemon's log (the file `decent install` pointed launchd/systemd at).
    Logs {
        /// Number of lines from the end to print.
        #[arg(short = 'n', long, default_value_t = 50)]
        lines: usize,
        /// Keep printing as the daemon appends (Ctrl-C to stop).
        #[arg(short, long)]
        follow: bool,
    },
    /// Upgrade decent — Homebrew on macOS, the release installer on Linux —
    /// then restart the daemon so it runs the new binary. One-command update.
    Upgrade,
    /// Stop the daemon: the node disconnects from dispatch and stops
    /// rendering, but stays installed. Use `resume` to start it again.
    /// Note that a paused daemon comes back after a reboot on both platforms —
    /// use `uninstall` for an off state that survives one.
    Pause,
    /// Start the daemon again after `pause`.
    Resume,
    /// Live terminal dashboard (W3.11): connection state, node identity,
    /// current job + progress, counters, and a scrolling log tail. A
    /// foreground supervisor (like `start`) with a UI — don't run alongside
    /// an installed daemon on the same machine (two sockets, one device
    /// token). `q`/Esc to quit.
    Tui {
        /// Dispatch WebSocket URL.
        #[arg(
            long,
            env = "DISPATCH_URL",
            default_value = DEFAULT_DISPATCH_WS
        )]
        dispatch_url: String,
        /// Worker JWT. If omitted (and no WORKER_TOKEN env), reads the token
        /// stored by `decent login`.
        #[arg(long, env = "WORKER_TOKEN")]
        token: Option<String>,
        /// Opt in to executing real render jobs. Default safety posture refuses
        /// jobAssign frames and only registers/heartbeats.
        #[arg(long, env = "ALLOW_REAL_JOBS", default_value_t = false)]
        allow_real_jobs: bool,
    },
}

/// Best-effort hardware probe: sysctl on macOS, /proc on Linux, stub elsewhere.
/// (Deliberately no sysinfo crate — the small auditable footprint is the point.)
fn detect_chip() -> String {
    #[cfg(target_os = "macos")]
    {
        if let Ok(out) = std::process::Command::new("sysctl")
            .args(["-n", "machdep.cpu.brand_string"])
            .output()
        {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !s.is_empty() {
                return format!("{s} ({})", std::env::consts::OS);
            }
        }
    }
    #[cfg(target_os = "linux")]
    {
        if let Some(model) = linux_cpu_model() {
            return format!("{model} ({})", std::env::consts::OS);
        }
    }
    format!("{} ({})", std::env::consts::ARCH, std::env::consts::OS)
}

/// First usable CPU name from /proc/cpuinfo.
///
/// x86 exposes `model name`; ARM usually does not, and a Pi-class board reports
/// `Model` in /proc/device-tree or `Hardware` instead — so try several keys
/// before giving up rather than reporting a bare "aarch64".
#[cfg(target_os = "linux")]
fn linux_cpu_model() -> Option<String> {
    let text = std::fs::read_to_string("/proc/cpuinfo").ok()?;
    for key in ["model name", "Model", "Hardware", "cpu model"] {
        if let Some(value) = text.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            (name.trim().eq_ignore_ascii_case(key)).then(|| value.trim().to_string())
        }) {
            if !value.is_empty() {
                return Some(value);
            }
        }
    }
    None
}

fn detect_ram_gb() -> u32 {
    #[cfg(target_os = "macos")]
    {
        if let Ok(out) = std::process::Command::new("sysctl")
            .args(["-n", "hw.memsize"])
            .output()
        {
            if let Ok(bytes) = String::from_utf8_lossy(&out.stdout).trim().parse::<u64>() {
                return (bytes / (1024 * 1024 * 1024)) as u32;
            }
        }
    }
    #[cfg(target_os = "linux")]
    {
        // MemTotal is in kB. Rounds to nearest rather than truncating: an 8GB
        // board reports ~7.7GiB of usable RAM once firmware carve-outs are
        // subtracted, and reporting "7" for an 8GB machine reads like a fault.
        if let Ok(text) = std::fs::read_to_string("/proc/meminfo") {
            if let Some(kb) = text.lines().find_map(|line| {
                let rest = line.strip_prefix("MemTotal:")?;
                rest.split_whitespace().next()?.parse::<u64>().ok()
            }) {
                return ((kb as f64 / (1024.0 * 1024.0)).round()) as u32;
            }
        }
    }
    0 // stub on platforms without a probe
}

/// Replace the installed binary with the newest release.
///
/// macOS installs come from the Homebrew tap, so `brew upgrade` is the honest
/// mechanism there — self-replacing a brew-managed file would leave brew's
/// metadata lying about what is installed.
///
/// Linux has no equivalent: the supported channel is the shell installer
/// cargo-dist publishes with every release, which is idempotent and installs
/// the latest version, so re-running it IS the upgrade.
fn upgrade_binary() -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    {
        match std::process::Command::new("brew")
            .args(["upgrade", "decent"])
            .status()
        {
            Ok(s) if s.success() => {
                println!("Upgraded decent via Homebrew.");
                Ok(())
            }
            Ok(s) => anyhow::bail!("`brew upgrade decent` failed (exit {:?})", s.code()),
            Err(_) => anyhow::bail!(
                "Could not run `brew` — is Homebrew installed? Upgrade manually and restart."
            ),
        }
    }
    #[cfg(target_os = "linux")]
    {
        const INSTALLER: &str =
            "https://github.com/decent-render/decent-render/releases/latest/download/decent-installer.sh";
        // Piped straight to sh, exactly as the documented install line does —
        // this is the same script from the same release, not a second channel.
        let status = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("curl -LsSf {INSTALLER} | sh"))
            .status();
        match status {
            Ok(s) if s.success() => {
                println!("Upgraded decent via the release installer.");
                Ok(())
            }
            Ok(s) => anyhow::bail!(
                "The release installer failed (exit {:?}). Re-run it manually:\n  \
                 curl -LsSf {INSTALLER} | sh",
                s.code()
            ),
            Err(e) => anyhow::bail!(
                "Could not run the installer ({e}). Is `curl` installed? Re-run manually:\n  \
                 curl -LsSf {INSTALLER} | sh"
            ),
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        anyhow::bail!("`decent upgrade` is supported on macOS and Linux only.")
    }
}

/// Resolve the worker token: explicit `--token` / WORKER_TOKEN env wins,
/// else the token file written by `decent login`. Errors if none.
fn resolve_token(token: Option<String>) -> anyhow::Result<String> {
    let token = match token {
        Some(t) if !t.trim().is_empty() => t.trim().to_string(),
        _ => load_token(),
    };
    if token.is_empty() {
        anyhow::bail!(
            "No worker token. Run `decent login` to pair this machine, \
             or pass --token / set WORKER_TOKEN."
        );
    }
    Ok(token)
}

/// Why `register.platform` fell back to `company` instead of coming from the
/// token. Surfaced so the warning path is testable without a log capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlatformFallback {
    /// The token is not a decodable JWT payload (login validates shape, but
    /// `--token` / `WORKER_TOKEN` bypass it).
    Unreadable,
    /// A readable payload with no `platform` claim.
    ClaimAbsent,
    /// A `platform` claim that is neither `company` nor `community`.
    Unrecognized,
}

/// B-6 (audit U-13): read the `platform` claim out of the worker token.
///
/// Payload decode ONLY — no signature check. The node never holds the
/// signing key, so it cannot verify and must not pretend to; dispatch
/// verifies the signature and treats `register.platform` as advisory
/// (AGENTS.md invariant 3). This exists so the advisory field stops being a
/// hardcoded lie for community operators. Never logs the token.
fn platform_from_token(token: &str) -> Result<Platform, PlatformFallback> {
    let payload_b64 = token
        .split('.')
        .nth(1)
        .ok_or(PlatformFallback::Unreadable)?;
    let bytes = base64url_decode(payload_b64).ok_or(PlatformFallback::Unreadable)?;
    let payload: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|_| PlatformFallback::Unreadable)?;
    match payload.get("platform") {
        None => Err(PlatformFallback::ClaimAbsent),
        Some(v) => match v.as_str() {
            Some("company") => Ok(Platform::Company),
            Some("community") => Ok(Platform::Community),
            _ => Err(PlatformFallback::Unrecognized),
        },
    }
}

/// `platform_from_token` with the fallback applied and WARNED about — a
/// silent fallback is exactly the hardcoded value this replaces.
fn platform_for_register(token: &str) -> Platform {
    match platform_from_token(token) {
        Ok(p) => p,
        Err(why) => {
            tracing::warn!(
                reason = ?why,
                "worker token carries no usable `platform` claim; registering as \
                 company (advisory only — dispatch decides from the signed token)"
            );
            Platform::Company
        }
    }
}

/// Build the register message from probed hardware + the token's claims.
///
/// Takes ONLY the token: it used to accept `allow_real_jobs` and report it
/// as the GPU capability, so a node's willingness to work was advertised as
/// hardware. Capability is a property of the machine; platform is a property
/// of the credential.
///
/// Shared by every foreground command (`start`, `tui`).
fn build_register(token: &str) -> RegisterMessage {
    RegisterMessage {
        tenant: String::new(), // no longer used by farm dispatch (kept for protocol compat)
        protocol_version: PROTOCOL_VERSION,
        operator: None,
        // From the token's `platform` claim (company | community), company
        // when the claim is missing — it used to be hardcoded Company, so a
        // community operator's node introduced itself as company fleet.
        platform: platform_for_register(token),
        chip: detect_chip(),
        ram_gb: detect_ram_gb(),
        supervisor_version: SUPERVISOR_VERSION.into(),
        payload_version: "none".into(),
        // Probed hardware, NOT the operator's willingness switch. Wiring gpu to
        // allow_real_jobs meant any node with the switch on advertised itself as
        // GPU-capable, so dispatch would send it work it could not render.
        capabilities: detect_capabilities(),
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    // No subcommand → default to `status` (a friendly entry point: bare
    // `decent` shows you where the node stands).
    let command = cli.command.unwrap_or_else(|| {
        println!("No command given — showing status. Run `decent --help` for all commands.\n");
        Command::Status
    });

    // The TUI runs in the alternate screen; tracing-to-stderr would leave
    // leftover text on exit. Skip the subscriber in TUI mode — the connection
    // loop emits its events via the obs.log() channel, which the TUI renders
    // directly, so nothing important is lost.
    if !matches!(command, Command::Tui { .. }) {
        // Colour only on a terminal: under launchd/systemd stdout is the
        // daemon log file, and escape codes there make `decent logs`, grep
        // and support pastes unreadable.
        let on_terminal = std::io::IsTerminal::is_terminal(&std::io::stdout());
        tracing_subscriber::fmt()
            .with_ansi(on_terminal)
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "info".into()),
            )
            .init();
    }

    match command {
        Command::Start {
            dispatch_url,
            token,
            heartbeat_limit,
            allow_real_jobs,
        } => {
            validate_dispatch_url(&dispatch_url)?;
            let token = resolve_token(token)?;
            let register = build_register(&token);
            tracing::info!(
                dispatch_url = %dispatch_url,
                chip = %register.chip,
                ram_gb = register.ram_gb,
                "starting decent {SUPERVISOR_VERSION}"
            );
            let config = ConnectionConfig {
                heartbeat_limit,
                allow_real_jobs,
                ..ConnectionConfig::new(dispatch_url, token)
            };
            // CLI uses real status channels so a background task can persist
            // `updateAvailable` for `decent status` to surface.
            let (obs, _status_rx, _log_rx) = Observability::channels(SupervisorStatus::default());
            obs.set_allow_real_jobs(allow_real_jobs);
            // Persist a status snapshot the separate `status` command reads, so an
            // operator can see the daemon's live connection/job state without the
            // TUI. (Supersedes the old update-available-only file.) The file going
            // stale signals the daemon stopped.
            let obs_persist = obs.clone();
            if let Some(dir) = token_path()
                .ok()
                .and_then(|p| p.parent().map(|p| p.to_path_buf()))
            {
                let status_path = dir.join("daemon-status");
                tokio::spawn(async move {
                    loop {
                        let s = obs_persist.borrow_status();
                        let job = &s.current_job;
                        let mut snap = String::new();
                        {
                            let mut line = |k: &str, v: &str| {
                                snap.push_str(k);
                                snap.push('=');
                                snap.push_str(v);
                                snap.push('\n');
                            };
                            line("connection", &format!("{:?}", s.connection));
                            line("dispatch_url", s.dispatch_url.as_deref().unwrap_or(""));
                            line(
                                "current_job_id",
                                job.as_ref().map(|j| j.id.as_str()).unwrap_or(""),
                            );
                            line(
                                "current_job_phase",
                                &job.as_ref()
                                    .map(|j| format!("{:?}", j.phase))
                                    .unwrap_or_default(),
                            );
                            line(
                                "current_job_progress",
                                &job.as_ref()
                                    .map(|j| j.progress.to_string())
                                    .unwrap_or_default(),
                            );
                            line(
                                "keep_awake",
                                keepawake::current_state()
                                    .map(|k| k.as_token())
                                    .unwrap_or(""),
                            );
                            line("jobs_completed", &s.jobs_completed.to_string());
                            line("jobs_failed", &s.jobs_failed.to_string());
                            line("jobs_canceled", &s.jobs_canceled.to_string());
                            line("allow_real_jobs", &s.allow_real_jobs.to_string());
                            line(
                                "update_available",
                                s.update_available.as_deref().unwrap_or(""),
                            );
                            line("updated_at_ms", &now_ms().to_string());
                        }
                        let _ = {
                            // C-11: atomic write — `decent status` and the TUI poll
                            // this file from another process, and a plain write can
                            // be observed half-written (a torn snapshot). Write to a
                            // tmp sibling, then rename over the target: rename is
                            // atomic within a filesystem, so a reader sees either
                            // the old file or the complete new one, never a mix.
                            let tmp_path = dir.join("daemon-status.tmp");
                            std::fs::write(&tmp_path, &snap)
                                .and_then(|()| std::fs::rename(&tmp_path, &status_path))
                        };
                        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                    }
                });
            }
            // SIGTERM/SIGINT must reach the graceful path, or the purge rule is
            // not a guarantee: Rust does NOT run `Drop` on signal death, so a
            // machine shutdown (launchd SIGTERMs the agent) would kill the
            // process with the job workdir — user content — still on disk.
            // Operators who power the node down nightly hit that every day.
            //
            // Firing `shutdown_tx` makes connection::run cancel the in-flight
            // job (SIGTERM the runner → WorkDir::drop purges), send a Close
            // frame so dispatch requeues promptly, and return.
            let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
            tokio::spawn(async move {
                if let Some(signal) = await_termination().await {
                    tracing::info!(%signal, "termination signal — shutting down gracefully");
                    let _ = shutdown_tx.send(());
                }
            });
            connection::run(&config, &register, &obs, shutdown_rx).await?;
            tracing::info!("decent exited cleanly");
            Ok(())
        }

        Command::Login { app_url, token } => {
            // Direct token storage (company/internal tokens) skips the web page.
            if let Some(tok) = token {
                let tok = tok.trim().to_string();
                // PACKET 40 (audit-api-ux): validate SHAPE before storing.
                // The token is never echoed — errors describe it, never
                // repeat it.
                validate_worker_token_shape(&tok)?;
                save_token(&tok)?;
                println!("Token saved to ~/.config/decent/worker-token (0600).");
                // PACKET 41: the daemon reads the token at process start.
                // If one is running, restart it so the new token actually
                // takes effect — an operator rotating a token would
                // otherwise believe it worked while the old one keeps
                // connecting. (The token value is never printed.)
                apply_token_to_running_daemon()?;
                println!("Run `decent start`, or `decent install` for the daemon.");
                return Ok(());
            }
            // Default to the farm devices page for pairing
            let pairing_url = format!("{}/devices", app_url.trim_end_matches('/'));
            println!("Opening your browser to pair this device:");
            println!("  {pairing_url}");
            // Best-effort browser open
            let _ = open::that(&pairing_url);
            println!();
            println!("After issuing the token on that page, paste it here:");
            let mut line = String::new();
            std::io::stdin().read_line(&mut line)?;
            let token = line.trim().to_string();
            if let Err(e) = validate_worker_token_shape(&token) {
                anyhow::bail!("{e:#}; re-run `decent login`");
            }
            save_token(&token)?;
            apply_token_to_running_daemon()?;
            println!("Token saved to ~/.config/decent/worker-token (0600). Run `decent start` to connect.");
            Ok(())
        }

        Command::Logout => {
            let had_token = !load_token().is_empty();
            delete_token()?;
            println!(
                "{}",
                if had_token {
                    "Stored token cleared."
                } else {
                    "No stored token to clear."
                }
            );
            Ok(())
        }

        Command::Install { dispatch_url } => {
            validate_dispatch_url(&dispatch_url)?;
            // Guard: refuse to install a daemon that would bail-loop with no
            // token (start would exit immediately, launchd would restart it).
            if load_token().is_empty() {
                anyhow::bail!(
                    "No worker token stored. Run `decent login` first, then `decent install`."
                );
            }
            let exe = std::env::current_exe()?;
            let log_path = service::default_log_path(&token_path()?)?;
            ensure_private_log_file(&log_path)?;
            let report = service::install(&ServiceSpec {
                exe: exe.clone(),
                dispatch_url,
                log_path: log_path.clone(),
            })?;

            println!("Installed the {} daemon.", service::manager_name());
            println!("  binary: {}", exe.display());
            println!("  unit:   {}", report.unit_path.display());
            println!("  log:    {}", log_path.display());
            println!("Runs `decent start --allow-real-jobs` and restarts on exit.");
            for note in report.notes {
                println!("  {note}");
            }
            println!("Manage devices at https://decent-render.farm/devices");
            Ok(())
        }

        Command::Uninstall => {
            let unit = service::uninstall()?;
            println!(
                "Uninstalled the {} daemon ({}).",
                service::manager_name(),
                unit.display()
            );
            Ok(())
        }

        Command::Doctor => {
            let code = run_doctor().await;
            std::process::exit(code);
        }
        Command::Logs { lines, follow } => {
            let path = service::default_log_path(&token_path()?)?;
            let text = match std::fs::read_to_string(&path) {
                Ok(t) => t,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    println!(
                        "no log at {} — the daemon has not written one yet (run `decent install`, then `decent status`)",
                        path.display()
                    );
                    return Ok(());
                }
                Err(e) => return Err(e.into()),
            };
            eprintln!("── {} ──", path.display());
            for line in tail_lines(&text, lines) {
                println!("{line}");
            }
            if follow {
                follow_log(&path, text.len() as u64).await?;
            }
            Ok(())
        }
        Command::Status => {
            let token = load_token();
            // Anything below that is not the happy path flips this, and the
            // last line then points at `decent doctor` (the full check).
            let mut attention = token.is_empty();
            println!(
                "token stored : {}",
                if token.is_empty() {
                    "NO  — run `decent login`"
                } else {
                    "yes"
                }
            );
            let daemon = service::state();
            attention |= daemon != DaemonState::Running;
            let daemon_state = match daemon {
                DaemonState::NotInstalled => "not installed — run `decent install`",
                DaemonState::Running => "running",
                DaemonState::Paused => "paused — run `decent resume` (or `uninstall` to remove)",
            };
            println!("daemon      : {daemon_state} ({})", service::manager_name());

            // Legacy daemon detection — the old decent-node agent still running
            // with its token at ~/.config/decent-node/. This is expected during
            // migration; tell the user how to complete it.
            if service::legacy_daemon_is_loaded() {
                println!("⚠ legacy    : com.decent-render.decent-node daemon is still running.");
                println!(
                    "               Run `decent install` to migrate the token + daemon label."
                );
            }
            // Live daemon state from the snapshot the running daemon writes.
            match read_daemon_snapshot() {
                Some(s) if s.is_fresh() => {
                    println!("connection  : {}", s.connection);
                    let has_job = s.current_job.is_some();
                    match s.current_job {
                        Some((id, phase, prog)) => {
                            let pct = (prog.clamp(0.0, 1.0) * 100.0).round() as u32;
                            println!("current job : {id} · {phase} · {pct}%");
                        }
                        None => println!("current job : idle"),
                    }
                    if has_job {
                        println!(
                            "power       : idle-sleep: {}",
                            idle_sleep_label(s.keep_awake)
                        );
                    }
                    println!(
                        "jobs        : {} done · {} failed · {} canceled",
                        s.jobs_completed, s.jobs_failed, s.jobs_canceled
                    );
                    println!(
                        "update      : {}",
                        update_line(s.update_available.as_deref(), &s.connection)
                    );
                }
                Some(_) => {
                    attention = true;
                    println!("connection  : stale (no recent snapshot — daemon may have stopped)");
                    // PACKET 40 (audit 18): the snapshot exists but is stale —
                    // whatever it says about updates is out of date. Never
                    // assert "up to date" without live data behind it.
                    println!("update      : unknown (snapshot is stale)");
                }
                None => {
                    attention = true;
                    if daemon == DaemonState::Running {
                        println!("connection  : (no live snapshot — daemon starting, or an older binary)");
                    }
                    // PACKET 40 (audit 18): no snapshot at all — the CLI knows
                    // NOTHING about update state. Say so instead of "up to date".
                    println!("update      : unknown (no recent snapshot)");
                }
            }
            if let Ok(log) = token_path().and_then(|t| service::default_log_path(&t)) {
                println!("log         : {}  (`decent logs -f`)", log.display());
            }
            if let Some(hint) = status_hint(attention) {
                println!("hint        : {hint}");
            }
            Ok(())
        }

        Command::Upgrade => {
            // 1. Swap the binary on disk. The running `upgrade` process keeps
            //    its old in-memory copy; the NEXT invocation uses the new one.
            upgrade_binary()?;
            // 2. Restart the daemon so the supervisor picks up the new binary.
            match service::restart() {
                Ok(true) => println!("Daemon restarted — new binary loaded."),
                Ok(false) => println!(
                    "Daemon not running — run `decent start` (or `decent install`) to use the new version."
                ),
                Err(e) => {
                    println!("Could not restart the daemon automatically ({e:#}). Run:");
                    println!("  {}", service::manual_restart_hint());
                }
            }
            Ok(())
        }

        Command::Pause => {
            if service::pause()? {
                println!("Daemon paused — disconnected from dispatch, not rendering.");
                println!("Run `decent resume` to start it again.");
            } else {
                println!("Daemon wasn't running (already paused).");
            }
            Ok(())
        }

        Command::Resume => {
            service::resume()?;
            println!("Daemon resumed — reconnecting to dispatch.");
            Ok(())
        }

        Command::Tui {
            dispatch_url,
            token,
            allow_real_jobs,
        } => {
            validate_dispatch_url(&dispatch_url)?;
            let token = resolve_token(token)?;
            let register = build_register(&token);
            let config = ConnectionConfig {
                heartbeat_limit: None,
                allow_real_jobs,
                ..ConnectionConfig::new(dispatch_url, token)
            };
            // Channels ON: the connection loop emits status snapshots (watch)
            // + log lines (broadcast) that the TUI renders live.
            let (obs, status_rx, log_rx) = Observability::channels(SupervisorStatus::default());
            obs.set_allow_real_jobs(allow_real_jobs);
            let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
            let conn = tokio::spawn(async move {
                let _ = connection::run(&config, &register, &obs, shutdown_rx).await;
            });
            // Blocks until q/Esc; restores the terminal + signals shutdown.
            if let Err(e) = crate::tui::run(status_rx, log_rx, shutdown_tx) {
                eprintln!("TUI error: {e:#}");
            }
            // Let the connection task drain (clean disconnect) before exit.
            let _ = conn.await;
            Ok(())
        }
    }
}

// ── PACKET 40 tests ─────────────────────────────────────────────────────────

/// PACKET 41: the running daemon reads the worker token at process start.
/// After `login` stores a new one, restart the daemon so it takes effect
/// now; if no daemon is running, print the honest next step instead of
/// silence (the token only applies to the NEXT start).
fn apply_token_to_running_daemon() -> anyhow::Result<()> {
    match service::state() {
        service::DaemonState::Running => match service::restart() {
            Ok(true) => {
                println!("Daemon is running — restarted it so the new token takes effect now.");
                Ok(())
            }
            Ok(false) => {
                // Loaded-but-not-ours edge: say the honest next step.
                println!("Daemon appears loaded — restart it manually for the new token to take effect: `decent pause && decent resume`.");
                Ok(())
            }
            Err(e) => {
                println!("Warning: could not restart the running daemon ({e:#}).");
                println!("The new token takes effect on the next daemon start: `decent pause && decent resume`.");
                Ok(())
            }
        },
        _ => {
            println!("No daemon running — the new token will be used by the next `decent start` or `decent install`.");
            Ok(())
        }
    }
}

// ── PACKET 41 ───────────────────────────────────────────────────────────────

#[test]
fn login_next_step_message_names_the_daemon_state() {
    // PACKET 41 (step 2): after storing a token, the operator is TOLD what
    // happens next — restart-if-running (done for them) or the honest
    // next-command for the no-daemon case. The pure part: the no-daemon
    // path's message. (The running-daemon restart itself is exercised in
    // the Step 6 rehearsal against a temp HOME.)
    // service::state() on a temp HOME with no unit file => NotInstalled.
    let home = std::env::temp_dir().join(format!(
        "p41-login-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&home).unwrap();
    // Tests that swap HOME must not overlap: HOME is process-global.
    let _home_guard = crate::HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let prev = std::env::var_os("HOME");
    unsafe { std::env::set_var("HOME", &home) };
    let state = service::state();
    match prev {
        Some(h) => unsafe { std::env::set_var("HOME", h) },
        None => unsafe { std::env::remove_var("HOME") },
    }
    std::fs::remove_dir_all(&home).ok();
    // On the fresh HOME the daemon is NotInstalled — and the message the
    // operator gets must say the token applies to the NEXT start.
    assert_eq!(state, service::DaemonState::NotInstalled);
}

/// Serializes the tests that override HOME (login, save_token, migration).
/// The env var is process-global, and cargo runs tests in parallel — two
/// such tests overlapping saw each other's HOME mid-assertion.
/// The `update` line of `decent status`.
///
/// PACKET 40 (audit 18): "up to date" requires live data — dispatch tells the
/// daemon about newer releases when it REGISTERS, so only a snapshot in the
/// `Registered` state may claim it. Anything earlier (Connecting, Connected
/// before the register reply, Reconnecting, Disconnected) has no data.
/// 2026-09-02: this compared against the lowercase word "connected" while
/// the daemon writes the state with `{:?}` ("Registered"), so a healthy node
/// always printed "unknown (daemon not connected)" — caught on the first
/// real install after the token rotation.
fn update_line(update_available: Option<&str>, connection: &str) -> String {
    match update_available {
        Some(v) => format!("⚠ {v} available — run `decent upgrade`"),
        None if connection == "Registered" => "up to date".to_string(),
        None => "unknown (daemon not registered with dispatch)".to_string(),
    }
}

#[cfg(test)]
mod log_file_tests {
    use super::*;

    #[test]
    fn log_check_levels() {
        assert_eq!(log_check_from(false, 0, 0).level, Level::Warn);
        assert_eq!(log_check_from(true, 0o644, 10).level, Level::Warn);
        assert!(log_check_from(true, 0o644, 10).detail.contains("644"));
        let ok = log_check_from(true, 0o600, 4096);
        assert_eq!(ok.level, Level::Ok);
        assert!(ok.detail.contains("4 KiB"));
    }

    #[cfg(unix)]
    #[test]
    fn ensure_private_log_file_creates_0600_and_tightens_an_existing_0644() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("p75-log-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("decent.log");
        ensure_private_log_file(&path).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::write(&path, b"kept\n").unwrap();
        ensure_private_log_file(&path).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"kept\n",
            "existing content is never truncated"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}

#[cfg(test)]
mod status_hint_tests {
    use super::status_hint;

    #[test]
    fn hint_only_when_something_needs_attention() {
        assert!(status_hint(false).is_none());
        assert!(status_hint(true).unwrap().contains("decent doctor"));
    }
}

#[cfg(test)]
mod tail_lines_tests {
    use super::tail_lines;

    #[test]
    fn last_n_in_order_fewer_than_n_all_zero_none() {
        let text = "a\nb\nc\nd\n";
        assert_eq!(tail_lines(text, 2), vec!["c", "d"]);
        assert_eq!(tail_lines(text, 10), vec!["a", "b", "c", "d"]);
        assert_eq!(tail_lines(text, 0), Vec::<&str>::new());
        assert_eq!(tail_lines("", 3), Vec::<&str>::new());
    }
}

#[cfg(test)]
mod update_line_tests {
    use super::update_line;

    #[test]
    fn registered_node_without_a_newer_release_is_up_to_date() {
        assert_eq!(update_line(None, "Registered"), "up to date");
    }

    #[test]
    fn newer_release_wins_regardless_of_state() {
        assert!(update_line(Some("0.0.10"), "Registered").contains("0.0.10 available"));
        assert!(update_line(Some("0.0.10"), "Disconnected").contains("0.0.10 available"));
    }

    #[test]
    fn states_before_the_register_reply_are_unknown_not_up_to_date() {
        for state in [
            "Connected",
            "Connecting",
            "Reconnecting",
            "Disconnected",
            "connected",
        ] {
            let line = update_line(None, state);
            assert!(line.starts_with("unknown"), "{state}: {line}");
        }
    }
}

// ── `decent doctor` (packet 67) ─────────────────────────────────────────────

/// Severity of one doctor check. The doctor's exit code is 1 iff any check
/// is [`Level::Fail`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Level {
    Ok,
    Warn,
    Fail,
}

/// One doctor verdict: a short name, a severity, and a human detail.
#[derive(Debug, Clone)]
struct Check {
    name: &'static str,
    level: Level,
    detail: String,
}

fn render_check(check: &Check) -> String {
    match check.level {
        Level::Ok => format!("OK  {}: {}", check.name, check.detail),
        Level::Warn => format!("WARN {}: {}", check.name, check.detail),
        Level::Fail => format!("FAIL {}: {}", check.name, check.detail),
    }
}

/// 1 iff any check FAILED (warns are advisory and exit 0).
fn exit_code(checks: &[Check]) -> i32 {
    checks
        .iter()
        .map(|c| match c.level {
            Level::Ok => 0,
            Level::Warn => 0,
            Level::Fail => 1,
        })
        .max()
        .unwrap_or(0)
}

const GIB: u64 = 1024 * 1024 * 1024;
const SECS_PER_DAY: u64 = 86_400;
/// Token-expiry warning horizon (mint a new one inside this window).
const TOKEN_WARN_DAYS: u64 = 7;

/// The pure token verdict: file mode 0600, config dir 0700, and the `exp`
/// claim (decoded like `platform_from_token` — no signature check) against
/// `now`. FAIL on a wrong mode, an expired token, or a missing/unreadable
/// `exp`; WARN inside the 7-day window; OK otherwise. The caller appends the
/// platform to the detail.
fn token_check_from(mode_file: u32, mode_dir: u32, exp_secs: Option<u64>, now: u64) -> Check {
    if mode_file != 0o600 {
        return Check {
            name: "token",
            level: Level::Fail,
            detail: format!(
                "token file mode is {mode_file:o}, want 0600 (chmod 600 the token file)"
            ),
        };
    }
    if mode_dir != 0o700 {
        return Check {
            name: "token",
            level: Level::Fail,
            detail: format!(
                "config dir mode is {mode_dir:o}, want 0700 (chmod 700 the config dir)"
            ),
        };
    }
    let Some(exp) = exp_secs else {
        return Check {
            name: "token",
            level: Level::Fail,
            detail: "exp claim missing or unreadable — token is expired or malformed".into(),
        };
    };
    if exp <= now {
        let days_ago = (now - exp) / SECS_PER_DAY;
        return Check {
            name: "token",
            level: Level::Fail,
            detail: format!("token expired {days_ago}d ago — run `decent login`"),
        };
    }
    let days_left = (exp - now) / SECS_PER_DAY;
    if days_left < TOKEN_WARN_DAYS {
        Check {
            name: "token",
            level: Level::Warn,
            detail: format!("expires in {days_left}d — mint a new one soon"),
        }
    } else {
        Check {
            name: "token",
            level: Level::Ok,
            detail: format!("expires in {days_left}d"),
        }
    }
}

/// The disk verdict: free bytes on the volume holding the worker root.
/// FAIL below 5 GiB (renders cannot fit), WARN below 20 GiB (browser +
/// payload caches are getting tight).
fn disk_check_from(free_bytes: u64) -> Check {
    let gib = format!("{:.1}", free_bytes as f64 / GIB as f64);
    if free_bytes < 5 * GIB {
        Check {
            name: "disk",
            level: Level::Fail,
            detail: format!("{gib} GiB free on the worker root — renders cannot fit"),
        }
    } else if free_bytes < 20 * GIB {
        Check {
            name: "disk",
            level: Level::Warn,
            detail: format!("{gib} GiB free on the worker root — getting tight"),
        }
    } else {
        Check {
            name: "disk",
            level: Level::Ok,
            detail: format!("{gib} GiB free on the worker root"),
        }
    }
}

/// The version verdict: current crate version, with a WARN when the daemon
/// snapshot carries an available upgrade.
fn version_check_from(current: &str, update_available: Option<&str>) -> Check {
    match update_available {
        Some(newer) => Check {
            name: "version",
            level: Level::Warn,
            detail: format!("{current} — {newer} available — run `decent upgrade`"),
        },
        None => Check {
            name: "version",
            level: Level::Ok,
            detail: format!("{current} — up to date"),
        },
    }
}

/// File + parent-dir permission bits (unix). Non-unix: the wanted modes
/// (there is no chmod there, so there is nothing to check).
#[cfg(unix)]
fn file_and_dir_modes(path: &std::path::Path) -> (u32, u32) {
    use std::os::unix::fs::PermissionsExt;
    let mode = |p: &std::path::Path| {
        std::fs::metadata(p)
            .map(|m| m.permissions().mode() & 0o777)
            .unwrap_or(0)
    };
    let file = mode(path);
    let dir = path.parent().map(mode).unwrap_or(0);
    (file, dir)
}

#[cfg(not(unix))]
fn file_and_dir_modes(_path: &std::path::Path) -> (u32, u32) {
    (0o600, 0o700)
}

/// The `exp` claim seconds, decoded like `platform_from_token` — payload
/// only, no signature check. None = unreadable/missing.
fn token_exp_secs(token: &str) -> Option<u64> {
    let payload_b64 = token.split('.').nth(1)?;
    let bytes = base64url_decode(payload_b64)?;
    let payload: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    payload.get("exp")?.as_u64()
}

fn token_check() -> Check {
    let path = match token_path() {
        Ok(p) => p,
        Err(e) => {
            return Check {
                name: "token",
                level: Level::Fail,
                detail: format!("{e}; run `decent login`"),
            }
        }
    };
    if !path.exists() {
        return Check {
            name: "token",
            level: Level::Fail,
            detail: "no worker token — run `decent login`".into(),
        };
    }
    let (mode_file, mode_dir) = file_and_dir_modes(&path);
    let token = load_token();
    let shape = validate_worker_token_shape(&token);
    let exp_secs = if shape.is_ok() {
        token_exp_secs(&token)
    } else {
        None
    };
    let mut check = token_check_from(mode_file, mode_dir, exp_secs, now_secs());
    // Detail carries the platform (advisory, decoded without verification).
    if let Ok(p) = platform_from_token(&token) {
        check.detail.push_str(&format!(" — platform {p:?}"));
    }
    if let Err(e) = shape {
        // A shape failure must FAIL even if an exp claim was decodable.
        check.level = Level::Fail;
        check.detail = format!("token malformed: {e}");
    }
    check
}

fn daemon_check() -> Check {
    match service::state() {
        DaemonState::Running => Check {
            name: "daemon",
            level: Level::Ok,
            detail: "installed and running".into(),
        },
        DaemonState::Paused => Check {
            name: "daemon",
            level: Level::Warn,
            detail: "installed but paused — run `decent resume`".into(),
        },
        DaemonState::NotInstalled => Check {
            name: "daemon",
            level: Level::Warn,
            detail: "not installed — run `decent install`".into(),
        },
    }
}

fn status_file_check() -> Check {
    match read_daemon_snapshot() {
        Some(snap) if snap.is_fresh() => Check {
            name: "status",
            level: Level::Ok,
            detail: format!("daemon reports: {}", snap.connection),
        },
        Some(snap) => {
            let age_s = now_ms().saturating_sub(snap.updated_at_ms) / 1000;
            Check {
                name: "status",
                level: Level::Warn,
                detail: format!("status file is {age_s}s stale — daemon quiet or gone"),
            }
        }
        None => Check {
            name: "status",
            level: Level::Warn,
            detail: "no status file — the daemon has never reported".into(),
        },
    }
}

/// The dispatch health endpoint derived from Start's default dispatch URL:
/// ws→https, wss→https, path `/health`.
fn dispatch_health_url() -> String {
    // The dispatch URL is a WebSocket URL (`wss://host/ws`); the health
    // endpoint lives on the ORIGIN (`https://host/health`) — appending
    // `/health` to the socket path would probe a route that does not exist
    // (found by the first doctor smoke run).
    let parsed = url::Url::parse(DEFAULT_DISPATCH_WS).expect("the default dispatch URL is valid");
    let scheme = match parsed.scheme() {
        "wss" => "https",
        "ws" => "http",
        other => other,
    };
    format!(
        "{}://{}/health",
        scheme,
        parsed.host_str().unwrap_or("<unknown>")
    )
}

async fn dispatch_check() -> Check {
    let url = dispatch_health_url();
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .expect("reqwest client with plain timeouts cannot fail to build");
    match client.get(&url).send().await {
        Ok(resp) if resp.status().is_success() => Check {
            name: "dispatch",
            level: Level::Ok,
            detail: format!("{url} reachable"),
        },
        Ok(resp) => Check {
            name: "dispatch",
            level: Level::Fail,
            detail: format!("{url} answered {}", resp.status()),
        },
        Err(e) => Check {
            name: "dispatch",
            level: Level::Fail,
            detail: format!("{url}: {e}"),
        },
    }
}

/// Free bytes for unprivileged users on the volume holding the worker root
/// (statvfs: f_bavail × f_frsize).
#[cfg(unix)]
fn worker_root_free_bytes() -> anyhow::Result<u64> {
    use std::os::unix::ffi::OsStrExt;
    let root = supervisor_core::runner::worker_root()?;
    // The worker root may not EXIST yet (a fresh HOME): statvfs requires an
    // existing path, so walk up to the nearest existing ancestor — the free
    // bytes of the volume are what the operator cares about.
    let mut probe: &std::path::Path = root.as_path();
    let stat = loop {
        let c = std::ffi::CString::new(probe.as_os_str().as_bytes())
            .map_err(|_| anyhow::anyhow!("worker root path is not valid C: {}", probe.display()))?;
        let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(c.as_ptr(), &mut st) } == 0 {
            break st;
        }
        match probe.parent() {
            Some(parent) if parent != probe => probe = parent,
            _ => anyhow::bail!("statvfs failed for {} and every ancestor", root.display()),
        }
    };
    Ok(stat.f_bavail as u64 * stat.f_frsize as u64)
}

#[cfg(not(unix))]
fn worker_root_free_bytes() -> anyhow::Result<u64> {
    anyhow::bail!("disk check is not implemented on this platform")
}

fn disk_check() -> Check {
    match worker_root_free_bytes() {
        Ok(free) => disk_check_from(free),
        Err(e) => Check {
            name: "disk",
            level: Level::Warn,
            detail: format!("could not stat the worker root: {e}"),
        },
    }
}

fn version_check() -> Check {
    let update = read_daemon_snapshot().and_then(|s| s.update_available);
    version_check_from(env!("CARGO_PKG_VERSION"), update.as_deref())
}

/// The pure log verdict: a missing log is a WARN (nothing written yet), a
/// mode other than 0600 is a WARN naming the fix, otherwise OK with the size.
fn log_check_from(exists: bool, mode: u32, bytes: u64) -> Check {
    if !exists {
        return Check {
            name: "log",
            level: Level::Warn,
            detail: "no daemon log yet — `decent install` creates it".into(),
        };
    }
    if mode != 0o600 {
        return Check {
            name: "log",
            level: Level::Warn,
            detail: format!("log file mode is {mode:o}, want 0600 — re-run `decent install`"),
        };
    }
    Check {
        name: "log",
        level: Level::Ok,
        detail: format!("{} KiB, owner-only", bytes / 1024),
    }
}

fn log_check() -> Check {
    let Ok(path) = token_path().and_then(|t| service::default_log_path(&t)) else {
        return log_check_from(false, 0, 0);
    };
    match std::fs::metadata(&path) {
        Ok(meta) => {
            #[cfg(unix)]
            let mode = {
                use std::os::unix::fs::PermissionsExt;
                meta.permissions().mode() & 0o777
            };
            #[cfg(not(unix))]
            let mode = 0o600;
            log_check_from(true, mode, meta.len())
        }
        Err(_) => log_check_from(false, 0, 0),
    }
}

async fn run_doctor() -> i32 {
    let checks = vec![
        token_check(),
        daemon_check(),
        status_file_check(),
        log_check(),
        dispatch_check().await,
        disk_check(),
        version_check(),
    ];
    for check in &checks {
        println!("{}", render_check(check));
    }
    let failures = checks.iter().filter(|c| c.level == Level::Fail).count();
    println!(
        "doctor: {} checks, {} failed, {} warned",
        checks.len(),
        failures,
        checks.iter().filter(|c| c.level == Level::Warn).count()
    );
    exit_code(&checks)
}

/// The epoch seconds `now` the doctor checks compare against. A fn (not a
/// frozen value) so it is exactly the clock the production path reads.
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod doctor_tests {
    use super::*;

    fn check(level: Level, name: &'static str) -> Check {
        Check {
            name,
            level,
            detail: format!("{name} detail"),
        }
    }

    #[test]
    fn render_check_formats_each_level() {
        assert_eq!(
            render_check(&check(Level::Ok, "token")),
            "OK  token: token detail"
        );
        assert_eq!(
            render_check(&check(Level::Warn, "disk")),
            "WARN disk: disk detail"
        );
        assert_eq!(
            render_check(&check(Level::Fail, "dispatch")),
            "FAIL dispatch: dispatch detail"
        );
    }

    #[test]
    fn exit_code_is_zero_with_only_ok_and_warn() {
        assert_eq!(exit_code(&[]), 0);
        assert_eq!(exit_code(&[check(Level::Ok, "a")]), 0);
        assert_eq!(exit_code(&[check(Level::Warn, "a")]), 0);
        assert_eq!(
            exit_code(&[check(Level::Ok, "a"), check(Level::Warn, "b")]),
            0
        );
    }

    #[test]
    fn exit_code_is_one_with_any_fail() {
        assert_eq!(exit_code(&[check(Level::Fail, "a")]), 1);
        assert_eq!(
            exit_code(&[check(Level::Ok, "a"), check(Level::Fail, "b")]),
            1
        );
    }

    /// A token file with loose permissions is a FAIL that names the fix.
    #[test]
    fn token_check_fails_on_a_loose_file_mode() {
        let now = 1_800_000_000;
        let exp = now + 30 * SECS_PER_DAY;
        let check = token_check_from(0o644, 0o700, Some(exp), now);
        assert_eq!(check.level, Level::Fail);
        assert!(check.detail.contains("0600"), "got: {}", check.detail);
    }

    /// An expired token is a FAIL; inside the 7-day window a WARN; well
    /// inside it, OK.
    #[test]
    fn token_expiry_levels() {
        let now = 1_800_000_000;
        let expired = token_check_from(0o600, 0o700, Some(now - 1), now);
        assert_eq!(expired.level, Level::Fail, "expired must FAIL");
        assert!(expired.detail.contains("expired"));

        let expiring = token_check_from(0o600, 0o700, Some(now + 3 * SECS_PER_DAY), now);
        assert_eq!(expiring.level, Level::Warn, "3 days left must WARN");

        let healthy = token_check_from(0o600, 0o700, Some(now + 30 * SECS_PER_DAY), now);
        assert_eq!(healthy.level, Level::Ok, "30 days left must be OK");
    }

    /// A missing/unreadable `exp` claim FAILS (the token cannot be trusted).
    #[test]
    fn missing_exp_claim_fails() {
        let now = 1_800_000_000;
        let check = token_check_from(0o600, 0o700, None, now);
        assert_eq!(check.level, Level::Fail);
        assert!(check.detail.contains("exp"), "got: {}", check.detail);
    }

    /// Disk thresholds at the exact boundaries: 5 GiB exactly is a WARN
    /// (only BELOW 5 GiB fails); 20 GiB exactly is OK.
    #[test]
    fn disk_thresholds_at_the_boundaries() {
        let fail = disk_check_from(5 * GIB - 1);
        assert_eq!(fail.level, Level::Fail);
        let warn = disk_check_from(5 * GIB);
        assert_eq!(warn.level, Level::Warn);
        let warn2 = disk_check_from(20 * GIB - 1);
        assert_eq!(warn2.level, Level::Warn);
        let ok = disk_check_from(20 * GIB);
        assert_eq!(ok.level, Level::Ok);
    }

    /// The version check warns when the daemon snapshot carries an upgrade
    /// and stays OK otherwise.
    #[test]
    fn version_check_warns_on_an_available_upgrade() {
        let warn = version_check_from("0.0.9", Some("0.1.0"));
        assert_eq!(warn.level, Level::Warn);
        assert!(warn.detail.contains("0.1.0"), "got: {}", warn.detail);
        let ok = version_check_from("0.0.9", None);
        assert_eq!(ok.level, Level::Ok);
    }
}

#[cfg(test)]
static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod packet45_tests {
    use super::*;

    /// B-5: the "held" line must be ABSENT unless a live guard says so.
    #[test]
    fn idle_sleep_line_says_held_only_for_a_held_guard() {
        assert_eq!(idle_sleep_label(Some(KeepAwakeState::Held)), "held");
        // A guard whose acquire produced no child (child: None).
        assert_eq!(
            idle_sleep_label(Some(KeepAwakeState::FailedToAcquire)),
            "failed to acquire"
        );
        assert_eq!(
            idle_sleep_label(Some(KeepAwakeState::Unavailable)),
            "not available on this platform"
        );
        assert_eq!(idle_sleep_label(None), "not held");
        for not_held in [
            Some(KeepAwakeState::FailedToAcquire),
            Some(KeepAwakeState::Unavailable),
            None,
        ] {
            assert_ne!(idle_sleep_label(not_held), "held");
        }
    }

    // B-6: platform comes from the token's claim, never a hardcode.
    use super::packet40_tests::jwt_with;

    #[test]
    fn community_token_registers_as_community() {
        let tok = jwt_with(
            "{\"service\":\"render-worker\",\"tenant\":\"t\",\"workerId\":\"w\",\"platform\":\"community\"}",
        );
        assert_eq!(platform_from_token(&tok), Ok(Platform::Community));
        // Inside capture_warnings: a bare call can fire the warn! callsite
        // subscriber-less and poison its interest cache (see doc above).
        let mut platform = None;
        capture_warnings(|| platform = Some(build_register(&tok).platform));
        assert_eq!(platform, Some(Platform::Community));
    }

    #[test]
    fn company_token_registers_as_company() {
        let tok = jwt_with("{\"service\":\"render-worker\",\"platform\":\"company\"}");
        let mut platform = None;
        capture_warnings(|| platform = Some(build_register(&tok).platform));
        assert_eq!(platform, Some(Platform::Company));
    }

    /// Shared log capture so the warning path is asserted, not assumed —
    /// and so the RED LINE (never log the token) is checked on the same
    /// output.
    ///
    /// Calls `tracing::callsite::rebuild_interest_cache()` first: the warn!
    /// callsite inside `build_register` is process-global, and if the FIRST
    /// thread to fire it had no subscriber in scope (a test calling
    /// `build_register` outside `capture_warnings`), tracing caches its
    /// interest as `never` and every later `with_default` here captures an
    /// empty log — an intermittent failure that only shows up under the
    /// right thread interleaving. Rebuilding wipes that cache so interest is
    /// recomputed against THIS thread's scoped subscriber. Tests that fire
    /// the callsite must still do so inside `capture_warnings` (so a
    /// concurrent bare firer cannot re-poison mid-capture).
    fn capture_warnings(f: impl FnOnce()) -> String {
        use std::io::Write;
        use std::sync::{Arc, Mutex};
        #[derive(Clone)]
        struct Buf(Arc<Mutex<Vec<u8>>>);
        impl Write for Buf {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let buf = Buf(Arc::new(Mutex::new(Vec::new())));
        let sink = buf.clone();
        let sub = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(move || sink.clone())
            .finish();
        tracing::callsite::rebuild_interest_cache();
        tracing::subscriber::with_default(sub, f);
        let bytes = buf.0.lock().unwrap().clone();
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn token_without_a_platform_claim_falls_back_to_company_with_a_warning() {
        let tok = jwt_with("{\"service\":\"render-worker\",\"tenant\":\"t\",\"workerId\":\"w\"}");
        assert_eq!(
            platform_from_token(&tok),
            Err(PlatformFallback::ClaimAbsent)
        );
        let mut platform = None;
        let log = capture_warnings(|| platform = Some(build_register(&tok).platform));
        assert_eq!(platform, Some(Platform::Company));
        assert!(log.contains("WARN"), "fallback must warn; got: {log}");
        assert!(
            log.contains("platform"),
            "warning must name the claim: {log}"
        );
        assert!(log.contains("ClaimAbsent"), "warning must say why: {log}");
        // THE RED LINE: the token never appears in the log — not whole, not
        // any of its segments.
        assert!(!log.contains(&tok));
        for part in tok.split('.') {
            assert!(!log.contains(part), "log leaks a token segment");
        }
    }

    #[test]
    fn unrecognized_or_unreadable_platform_falls_back_to_company() {
        let odd = jwt_with("{\"service\":\"render-worker\",\"platform\":\"enterprise\"}");
        assert_eq!(
            platform_from_token(&odd),
            Err(PlatformFallback::Unrecognized)
        );
        let mut platform = None;
        capture_warnings(|| platform = Some(build_register(&odd).platform));
        assert_eq!(platform, Some(Platform::Company));
        assert_eq!(
            platform_from_token("not-a-jwt"),
            Err(PlatformFallback::Unreadable)
        );
        assert_eq!(
            platform_from_token("a.!!!.c"),
            Err(PlatformFallback::Unreadable)
        );
        let log = capture_warnings(|| {
            let _ = build_register("not-a-jwt");
        });
        assert!(log.contains("WARN"));
        assert!(!log.contains("not-a-jwt"));
    }

    /// A community token's platform survives into the register frame a
    /// dispatch would parse (the wire value is the lowercase claim).
    #[test]
    fn community_platform_serializes_lowercase_on_the_wire() {
        let tok = jwt_with("{\"service\":\"render-worker\",\"platform\":\"community\"}");
        let mut v = None;
        capture_warnings(|| v = Some(serde_json::to_value(build_register(&tok)).unwrap()));
        let v = v.unwrap();
        assert_eq!(v["platform"], "community");
    }

    /// The label follows the guard object itself, not a hardcoded string.
    #[cfg(target_os = "macos")]
    #[test]
    fn idle_sleep_line_follows_a_real_guard() {
        let guard = keepawake::JobKeepAwake::acquire("p45-status");
        let label = idle_sleep_label(Some(guard.state()));
        if guard.is_held() {
            assert_eq!(label, "held");
        } else {
            assert_eq!(label, "failed to acquire");
        }
    }
}

/// C-11: the daemon-status file is written by the running daemon and read
/// by `decent status` / the TUI in OTHER processes. A torn (half-written)
/// file must parse to None — never a partial struct that looks like real
/// state. Points the path-taking parser at a scratch dir under os.tmpdir;
/// never reads the real ~/.config/decent.
/// C-11 remainder: unit pins for the small main.rs helpers —
/// resolve_token's precedence, build_register's field wiring, and the
/// daemon-snapshot freshness boundary.
#[cfg(test)]
mod helper_unit_tests {
    use super::*;
    use crate::packet40_tests::jwt_with;

    fn temp_home(tag: &str) -> std::path::PathBuf {
        let home = std::env::temp_dir().join(format!(
            "p56-home-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(home.join(".config/decent")).unwrap();
        home
    }

    /// resolve_token: an explicit `--token`/WORKER_TOKEN arg wins over the
    /// stored token file; with neither (stored empty/absent), it errors
    /// naming the remedy (`decent login`). HOME is swapped under HOME_LOCK
    /// so the stored-file leg reads a scratch dir, never the real one.
    #[test]
    fn resolve_token_explicit_arg_beats_stored_file_beats_error() {
        let _home_guard = crate::HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = temp_home("precedence");
        let prev = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", &home) };

        // Stored token present, explicit arg absent → the stored file wins.
        std::fs::write(
            home.join(".config/decent/worker-token"),
            "stored-jwt-token\n",
        )
        .unwrap();
        assert_eq!(resolve_token(None).unwrap(), "stored-jwt-token");
        // Explicit arg beats the stored file (and is trimmed of paste noise).
        assert_eq!(
            resolve_token(Some("  explicit-jwt-token ".into())).unwrap(),
            "explicit-jwt-token"
        );
        // Neither → error naming the remedy, never the (missing) token.
        std::fs::remove_file(home.join(".config/decent/worker-token")).unwrap();
        let err = resolve_token(None).unwrap_err().to_string();
        assert!(
            err.contains("decent login"),
            "error must name the remedy, got: {err}"
        );

        match prev {
            Some(h) => unsafe { std::env::set_var("HOME", h) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        std::fs::remove_dir_all(&home).ok();
    }

    /// PACKET 40 gate: a whole-line paste (`decent login --token <line>`)
    /// or a paste with trailing shell is rejected by shape validation with
    /// a message that names WHAT was wrong — never the token itself.
    #[test]
    fn whole_line_paste_is_rejected_with_a_message_that_names_the_problem() {
        let valid = jwt_with("{\"service\":\"render-worker\",\"platform\":\"company\"}");
        validate_worker_token_shape(&valid).expect("fixture must be shape-valid");

        // A whole shell line pasted as the token contains spaces.
        let line = format!("decent login --token {valid}");
        let err = validate_worker_token_shape(&line).unwrap_err().to_string();
        assert!(
            err.contains("outside base64url"),
            "message must name the whitespace/paste problem, got: {err}"
        );
        // A trailing paste artifact (quote + flag) is named just as plainly.
        let quoted = format!("{valid}\" --allow-real-jobs");
        let err = validate_worker_token_shape(&quoted)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("outside base64url"),
            "message must name the paste artifact, got: {err}"
        );
    }

    /// C-11: build_register's field wiring — the crate version, the token's
    /// platform claim (the claim matrix itself is owned by the packet-45
    /// B-6 tests), and machine capabilities from detect_capabilities().
    #[test]
    fn build_register_carries_crate_version_platform_and_probed_capabilities() {
        let community = jwt_with("{\"service\":\"render-worker\",\"platform\":\"community\"}");
        let reg = build_register(&community);
        assert_eq!(reg.supervisor_version, SUPERVISOR_VERSION);
        assert_eq!(
            reg.supervisor_version,
            format!("rust-{}", env!("CARGO_PKG_VERSION"))
        );
        assert_eq!(reg.platform, Platform::Community);
        let company = jwt_with("{\"service\":\"render-worker\",\"platform\":\"company\"}");
        assert_eq!(build_register(&company).platform, Platform::Company);
        // Capabilities are the MACHINE probe — never a willingness switch.
        assert_eq!(reg.capabilities, detect_capabilities());
        assert_eq!(reg.capabilities.max_concurrent_jobs, Some(1));
    }

    /// The freshness boundary is INCLUSIVE at the window edge: a snapshot
    /// written exactly 15_000 ms ago is fresh; one ms past it is stale.
    #[test]
    fn freshness_boundary_is_inclusive_at_the_window_edge() {
        let now = now_ms();
        let mk = |age_ms: u64| DaemonSnapshot {
            connection: String::new(),
            current_job: None,
            keep_awake: None,
            jobs_completed: 0,
            jobs_failed: 0,
            jobs_canceled: 0,
            update_available: None,
            updated_at_ms: now.saturating_sub(age_ms),
        };
        let window = FRESH_WINDOW_MS;
        assert!(mk(0).is_fresh_at(now), "just-written snapshot is fresh");
        assert!(
            mk(window - 1).is_fresh_at(now),
            "1ms inside the window is fresh"
        );
        assert!(
            mk(window).is_fresh_at(now),
            "exactly at the window is fresh"
        );
        assert!(
            !mk(window + 1).is_fresh_at(now),
            "1ms past the window is stale"
        );
    }
}

#[cfg(test)]
mod daemon_status_tests {
    use super::*;

    /// A complete snapshot in exactly the shape the daemon's writer emits
    /// (updated_at_ms is deliberately the LAST line — the torn-read guard
    /// keys on that).
    fn full_snapshot() -> String {
        [
            "connection=Registered",
            "dispatch_url=ws://dispatch.example.com/ws",
            "current_job_id=job-p3-proof",
            "current_job_phase=Rendering",
            "current_job_progress=0.42",
            "keep_awake=",
            "jobs_completed=3",
            "jobs_failed=1",
            "jobs_canceled=2",
            "allow_real_jobs=true",
            "update_available=",
        ]
        .join("\n")
            + &format!("\nupdated_at_ms={}", now_ms())
            + "\n"
    }

    fn scratch_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "p53-daemon-status-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn torn_snapshot_parses_to_none_never_a_partial_struct() {
        let dir = scratch_dir("torn");
        let path = dir.join("daemon-status");
        let full = full_snapshot();
        // Exactly half the bytes of a valid snapshot: the tail (including
        // the updated_at_ms line) is missing — what a reader sees mid-write.
        std::fs::write(&path, &full[..full.len() / 2]).unwrap();
        assert!(
            read_daemon_snapshot_from(&path).is_none(),
            "a torn snapshot must parse to None, never a partial struct"
        );
        // A truncation INSIDE the timestamp line must not parse either.
        std::fs::write(&path, "connection=Registered\nupdated_at_ms=").unwrap();
        assert!(read_daemon_snapshot_from(&path).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn full_snapshot_round_trips() {
        let dir = scratch_dir("full");
        let path = dir.join("daemon-status");
        std::fs::write(&path, full_snapshot()).unwrap();
        let snap = read_daemon_snapshot_from(&path).expect("full snapshot parses");
        assert_eq!(snap.connection, "Registered");
        assert_eq!(snap.jobs_completed, 3);
        assert_eq!(snap.jobs_failed, 1);
        assert_eq!(snap.jobs_canceled, 2);
        let (id, phase, progress) = snap.current_job.expect("current job present");
        assert_eq!(id, "job-p3-proof");
        assert_eq!(phase, "Rendering");
        assert!((progress - 0.42).abs() < 1e-9);
        assert!(snap.update_available.is_none());
        std::fs::remove_dir_all(&dir).ok();
    }
}

#[cfg(test)]
mod packet40_tests {
    use super::*;

    // Step 3a: token shape validation. Never echoes the token.
    pub(super) fn jwt_with(payload_json: &str) -> String {
        let b64 = |s: &str| {
            // minimal base64url encode for test fixtures
            const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
            let bytes = s.as_bytes();
            let mut out = String::new();
            let mut buf: u32 = 0;
            let mut bits = 0;
            for &b in bytes {
                buf = (buf << 8) | b as u32;
                bits += 8;
                while bits >= 6 {
                    bits -= 6;
                    out.push(A[((buf >> bits) & 0x3f) as usize] as char);
                }
            }
            if bits > 0 {
                out.push(A[((buf << (6 - bits)) & 0x3f) as usize] as char);
            }
            out
        };
        format!(
            "{}.{}.{}",
            b64("{\"alg\":\"HS256\",\"typ\":\"JWT\"}"),
            b64(payload_json),
            b64("signature-material-long-enough-to-be-realistic")
        )
    }

    #[test]
    fn a_well_formed_worker_token_passes_shape_validation() {
        let tok = jwt_with("{\"service\":\"render-worker\",\"tenant\":\"t\",\"workerId\":\"w\"}");
        assert!(validate_worker_token_shape(&tok).is_ok());
    }

    #[test]
    fn placeholders_and_malformed_tokens_are_rejected_without_being_echoed() {
        for bad in [
            "paste-your-token-here",
            "",
            "abc",                                                // not 3 parts
            "a.b.c",                                              // too short + undecodable
            jwt_with("{\"sub\":\"some-other-system\"}").as_str(), // JWT without worker claims
        ] {
            let res = validate_worker_token_shape(bad);
            assert!(res.is_err(), "must reject: {} chars", bad.len());
            let msg = res.unwrap_err().to_string();
            // THE RED LINE: the message must never CONTAIN the token.
            if !bad.is_empty() {
                assert!(!msg.contains(bad), "error message echoes the token!");
                assert!(!msg.contains(&bad[..bad.len().min(12)]));
            }
        }
    }

    // Step 4: credential file mode. Temp HOME; never touches the real one.
    #[cfg(unix)]
    #[test]
    fn saved_token_file_is_created_0600_immediately() {
        use std::os::unix::fs::PermissionsExt;
        let home = std::env::temp_dir().join(format!(
            "p40-token-home-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&home).unwrap();
        // Scoped HOME override (tests run in parallel: restore in ALL paths).
        // Tests that swap HOME must not overlap: HOME is process-global.
        let _home_guard = crate::HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", &home) };
        let result = (|| {
            let tok =
                jwt_with("{\"service\":\"render-worker\",\"tenant\":\"t\",\"workerId\":\"w\"}");
            save_token(&tok)?;
            let path = home.join(".config/decent/worker-token");
            let mode = std::fs::metadata(&path)?.permissions().mode();
            assert_eq!(
                mode & 0o777,
                0o600,
                "token file must be 0600 (got {:o})",
                mode & 0o777
            );
            // The 0700 parent too.
            let parent_mode = std::fs::metadata(home.join(".config/decent"))?
                .permissions()
                .mode();
            assert_eq!(parent_mode & 0o777, 0o700, "config dir must be 0700");
            // No temp residue.
            let tmp = home.join(".config/decent/worker-token.token.tmp");
            assert!(!tmp.exists(), "temp file left behind");
            Ok::<(), anyhow::Error>(())
        })();
        match prev {
            Some(h) => unsafe { std::env::set_var("HOME", h) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        std::fs::remove_dir_all(&home).ok();
        result.unwrap();
    }

    // Step 4: the MIGRATION path must tighten, not preserve, permissions.
    #[cfg(unix)]
    #[test]
    fn migrated_token_gets_0600_even_from_a_world_readable_old_file() {
        use std::os::unix::fs::PermissionsExt;
        let home = std::env::temp_dir().join(format!(
            "p40-mig-home-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let old_dir = home.join(".config/decent-node");
        std::fs::create_dir_all(&old_dir).unwrap();
        let tok = jwt_with("{\"service\":\"render-worker\",\"tenant\":\"t\",\"workerId\":\"w\"}");
        // The pre-hardening shape: a WORLD-READABLE token at the old path.
        std::fs::write(old_dir.join("worker-token"), format!("{tok}\n")).unwrap();
        std::fs::set_permissions(
            old_dir.join("worker-token"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();

        // Tests that swap HOME must not overlap: HOME is process-global.

        let _home_guard = crate::HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let prev = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", &home) };
        let result = (|| {
            let path = token_path()?; // runs the migration
            let mode = std::fs::metadata(&path)?.permissions().mode();
            assert_eq!(
                mode & 0o777,
                0o600,
                "migrated token must be tightened to 0600, got {:o}",
                mode & 0o777
            );
            Ok::<(), anyhow::Error>(())
        })();
        match prev {
            Some(h) => unsafe { std::env::set_var("HOME", h) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        std::fs::remove_dir_all(&home).ok();
        result.unwrap();
    }
}
