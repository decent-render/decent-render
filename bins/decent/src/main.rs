//! decent — thin CLI over supervisor-core.
//!
//! `decent start --dispatch-url ws://localhost:8790/ws --token <jwt>`
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
use supervisor_core::protocol::{Platform, RegisterMessage, PROTOCOL_VERSION};
use supervisor_core::status::{Observability, SupervisorStatus};

const SUPERVISOR_VERSION: &str = concat!("rust-", env!("CARGO_PKG_VERSION"));
/// Minimum dispatch server version this client is compatible with.
#[allow(dead_code)]
const MIN_DISPATCH_VERSION: &str = "0.0.1";

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
    if !new_path.exists() && old_path.exists() {
        if let Some(parent) = new_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match std::fs::copy(&old_path, &new_path) {
            Ok(_) => {
                eprintln!("Migrated token from ~/.config/decent-node/ → ~/.config/decent/");
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
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The daemon's live status snapshot, parsed from the `daemon-status` file the
/// running daemon writes every few seconds. Read by the separate `status`
/// command so an operator can see connection/job state without the TUI.
struct DaemonSnapshot {
    connection: String,
    current_job: Option<(String, String, f64)>,
    jobs_completed: u64,
    jobs_failed: u64,
    jobs_canceled: u64,
    update_available: Option<String>,
    updated_at_ms: u64,
}

impl DaemonSnapshot {
    /// Fresh = written within the last 15s (the daemon writes every 3s).
    fn is_fresh(&self) -> bool {
        now_ms().saturating_sub(self.updated_at_ms) < 15_000
    }
}

fn read_daemon_snapshot() -> Option<DaemonSnapshot> {
    let content = token_path()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("daemon-status")))
        .and_then(|p| std::fs::read_to_string(&p).ok())?;
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
        jobs_completed: val("jobs_completed").parse().unwrap_or(0),
        jobs_failed: val("jobs_failed").parse().unwrap_or(0),
        jobs_canceled: val("jobs_canceled").parse().unwrap_or(0),
        update_available: if upd.is_empty() {
            None
        } else {
            Some(upd.to_string())
        },
        updated_at_ms: val("updated_at_ms").parse().unwrap_or(0),
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
        #[arg(long, env = "DISPATCH_URL", default_value = "ws://localhost:8790/ws")]
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
        /// `scripts/mint-worker-token.ts` (skips the self-serve device flow).
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
            default_value = "wss://decent-render-dispatch.fly.dev/ws"
        )]
        dispatch_url: String,
    },
    /// Uninstall the daemon: stops it and removes the unit file.
    Uninstall,
    /// Show pairing + daemon status: is a token stored? is the daemon
    /// installed and running?
    Status,
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
        #[arg(long, env = "DISPATCH_URL", default_value = "ws://localhost:8790/ws")]
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

/// Build the register message from probed hardware.
///
/// Deliberately takes no arguments: it used to accept `allow_real_jobs` and
/// report it as the GPU capability, so a node's willingness to work was
/// advertised as hardware. Capability is a property of the machine.
///
/// Shared by every foreground command (`start`, `tui`).
fn build_register() -> RegisterMessage {
    RegisterMessage {
        tenant: String::new(), // no longer used by farm dispatch (kept for protocol compat)
        protocol_version: PROTOCOL_VERSION,
        operator: None,
        platform: Platform::Company,
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
        tracing_subscriber::fmt()
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
            let token = resolve_token(token)?;
            let register = build_register();
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
                        let _ = std::fs::write(&status_path, snap);
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
                if tok.split('.').count() != 3 {
                    anyhow::bail!(
                        "That doesn't look like a worker token (expected three dot-separated parts)."
                    );
                }
                save_token(&tok)?;
                println!("Token saved to ~/.config/decent/worker-token (0600).");
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
            if token.split('.').count() != 3 {
                anyhow::bail!(
                    "That doesn't look like a worker token (expected three dot-separated parts). \
                     Re-run `decent login`."
                );
            }
            save_token(&token)?;
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
            // Guard: refuse to install a daemon that would bail-loop with no
            // token (start would exit immediately, launchd would restart it).
            if load_token().is_empty() {
                anyhow::bail!(
                    "No worker token stored. Run `decent login` first, then `decent install`."
                );
            }
            let exe = std::env::current_exe()?;
            let log_path = service::default_log_path(&token_path()?)?;
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

        Command::Status => {
            let token = load_token();
            println!(
                "token stored : {}",
                if token.is_empty() {
                    "NO  — run `decent login`"
                } else {
                    "yes"
                }
            );
            let daemon = service::state();
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
                        println!("power       : idle-sleep held while rendering");
                    }
                    println!(
                        "jobs        : {} done · {} failed · {} canceled",
                        s.jobs_completed, s.jobs_failed, s.jobs_canceled
                    );
                    println!(
                        "update      : {}",
                        match s.update_available {
                            Some(v) => {
                                format!("⚠ {v} available — run `decent upgrade`")
                            }
                            None => "up to date".to_string(),
                        }
                    );
                }
                Some(_) => {
                    println!("connection  : stale (no recent snapshot — daemon may have stopped)");
                    println!("update      : up to date");
                }
                None => {
                    if daemon == DaemonState::Running {
                        println!("connection  : (no live snapshot — daemon starting, or an older binary)");
                    }
                    println!("update      : up to date");
                }
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
            let token = resolve_token(token)?;
            let register = build_register();
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
