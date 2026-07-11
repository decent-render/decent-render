//! decent — thin CLI over supervisor-core.
//!
//! `decent start --dispatch-url ws://localhost:8790/ws --token <jwt>`
//! (or env vars DISPATCH_URL / WORKER_TOKEN). Registers with the dispatch and
//! heartbeats; real rendering requires `--allow-real-jobs`.
//!
//! The CLI and the Tauri app share the same `connection::run` code path.
//! The only difference: the CLI passes `Observability::default()` (tracing
//! only), the app passes one with status/log channels attached.

mod tui;

use clap::{Parser, Subcommand};
use supervisor_core::connection::{self, ConnectionConfig};
use supervisor_core::protocol::{Capabilities, Platform, RegisterMessage, PROTOCOL_VERSION};
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

/// launchd label for the installed agent.
const LAUNCHD_LABEL: &str = "com.decent-render.decent";
const LEGACY_LAUNCHD_LABEL: &str = "com.decent-render.decent-node";

fn launch_agents_dir() -> anyhow::Result<std::path::PathBuf> {
    let home = std::env::var_os("HOME").ok_or_else(|| anyhow::anyhow!("HOME is not set"))?;
    let dir = std::path::PathBuf::from(home).join("Library/LaunchAgents");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn plist_path() -> anyhow::Result<std::path::PathBuf> {
    Ok(launch_agents_dir()?.join(format!("{LAUNCHD_LABEL}.plist")))
}

/// Build the launchd agent plist: runs `decent start --allow-real-jobs` at
/// login against dispatch, restarts on exit (KeepAlive), logs to the config dir.
fn build_plist(exe: &std::path::Path, dispatch_url: &str, log_path: &std::path::Path) -> String {
    let exe_str = exe.to_string_lossy();
    let mut args = String::new();
    for &arg in &[
        exe_str.as_ref(),
        "start",
        "--dispatch-url",
        dispatch_url,
        "--allow-real-jobs",
    ] {
        args.push_str("        <string>");
        args.push_str(arg);
        args.push_str("</string>\n");
    }
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
{args}    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>{log}</string>
    <key>StandardErrorPath</key>
    <string>{log}</string>
    <key>EnvironmentVariables</key>
    <dict>
        <key>RUST_LOG</key>
        <string>info</string>
    </dict>
</dict>
</plist>
"#,
        label = LAUNCHD_LABEL,
        log = log_path.display(),
    )
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
    /// Install as a macOS launchd agent: runs `decent start` at login and
    /// restarts on exit (KeepAlive), so the node renders unattended. Accepts
    /// real jobs. Run `decent login` first to store a token.
    Install {
        /// Dispatch WebSocket URL.
        #[arg(
            long,
            env = "DISPATCH_URL",
            default_value = "wss://decent-render-dispatch.fly.dev/ws"
        )]
        dispatch_url: String,
    },
    /// Uninstall the launchd agent (stops it and removes the plist).
    Uninstall,
    /// Show pairing + daemon status: is a token stored? is the launchd agent
    /// installed/loaded?
    Status,
    /// Upgrade decent via Homebrew, then restart the daemon (if loaded)
    /// so launchd relaunches it with the new binary. One-command fleet update.
    Upgrade,
    /// Stop the daemon (launchctl bootout): the node disconnects from dispatch
    /// and stops rendering, but the launchd agent stays installed. Use
    /// `resume` to start it again. Note: launchd re-loads LaunchAgents at
    /// login, so a paused daemon restarts after reboot — use `uninstall`
    /// for an off state that survives reboot.
    Pause,
    /// Start the daemon again after `pause` (launchctl bootstrap).
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

/// Best-effort hardware probe: sysctl on macOS, stubs elsewhere.
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
    format!("{} ({})", std::env::consts::ARCH, std::env::consts::OS)
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
    0 // stub on platforms without a probe
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

/// Build the register message from probed hardware + the real-jobs flag.
/// Shared by every foreground command (`start`, `tui`).
fn build_register(allow_real_jobs: bool) -> RegisterMessage {
    RegisterMessage {
        tenant: String::new(), // no longer used by farm dispatch (kept for protocol compat)
        protocol_version: PROTOCOL_VERSION,
        operator: None,
        platform: Platform::Company,
        chip: detect_chip(),
        ram_gb: detect_ram_gb(),
        supervisor_version: SUPERVISOR_VERSION.into(),
        payload_version: "none".into(),
        capabilities: Capabilities {
            gpu: allow_real_jobs,
        },
    }
}

/** Is the decent launchd agent currently loaded? Also checks the legacy
 * com.decent-render.decent-node label for upgraded installs. */
fn launchctl_has_label() -> bool {
    std::process::Command::new("launchctl")
        .arg("list")
        .output()
        .map(|o| {
            let out = String::from_utf8_lossy(&o.stdout);
            out.contains(LAUNCHD_LABEL) || out.contains(LEGACY_LAUNCHD_LABEL)
        })
        .unwrap_or(false)
}

/** Is ONLY the legacy decent-node daemon loaded (not the new one)? */
fn legacy_daemon_is_loaded() -> bool {
    std::process::Command::new("launchctl")
        .arg("list")
        .output()
        .map(|o| {
            let out = String::from_utf8_lossy(&o.stdout);
            out.contains(LEGACY_LAUNCHD_LABEL) && !out.contains(LAUNCHD_LABEL)
        })
        .unwrap_or(false)
}

/** Path to the legacy plist, if it exists. */
fn legacy_plist_path() -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME")?;
    let p = std::path::PathBuf::from(home)
        .join("Library/LaunchAgents")
        .join(format!("{LEGACY_LAUNCHD_LABEL}.plist"));
    if p.exists() {
        Some(p)
    } else {
        None
    }
}

/// Unload the legacy decent-node launchd agent if present (one-time migration
/// during the decent-node → decent rename). No-op if not found.
fn unload_legacy_agent() {
    let _ = std::process::Command::new("launchctl")
        .args([
            "bootout",
            &format!("gui/{}", current_uid().unwrap_or_default()),
            LEGACY_LAUNCHD_LABEL,
        ])
        .output();
}

/// Current numeric UID, for the launchctl `gui/<uid>/<label>` service target.
fn current_uid() -> Option<String> {
    std::process::Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
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
            let register = build_register(allow_real_jobs);
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
            // CLI never signals shutdown — runs until heartbeat-limit or server close.
            let (_shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
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
            // One-time migration: unload the legacy decent-node agent if present.
            unload_legacy_agent();
            let exe = std::env::current_exe()?;
            let plist = plist_path()?;
            let log_path = token_path()?
                .parent()
                .ok_or_else(|| anyhow::anyhow!("token file has no parent"))?
                .join("decent.log");
            let xml = build_plist(&exe, &dispatch_url, &log_path);
            // Best-effort unload for a clean reinstall (suppress output —
            // "not loaded" is the expected first-install case, not an error).
            let _ = std::process::Command::new("launchctl")
                .args(["unload", &plist.to_string_lossy()])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
            std::fs::write(&plist, xml)?;
            let status = std::process::Command::new("launchctl")
                .args(["load", &plist.to_string_lossy()])
                .status()?;
            if !status.success() {
                anyhow::bail!(
                    "launchctl load failed; inspect the plist at {}",
                    plist.display()
                );
            }
            println!("Installed launchd agent {LAUNCHD_LABEL}.");
            println!("  binary: {}", exe.display());
            println!("  plist:  {}", plist.display());
            println!("  log:    {}", log_path.display());
            println!(
                "Runs `decent start --allow-real-jobs` at login; restarts on exit (KeepAlive)."
            );
            println!("Manage devices at https://decent-render.farm/devices");

            // Clean up the legacy plist file (the agent was already unloaded
            // above; remove the old plist so it doesn't reload on next login).
            if let Some(legacy_plist) = legacy_plist_path() {
                let _ = std::fs::remove_file(&legacy_plist);
                println!("Removed legacy plist: {}", legacy_plist.display());
            }

            Ok(())
        }

        Command::Uninstall => {
            let plist = plist_path()?;
            let _ = std::process::Command::new("launchctl")
                .args(["unload", &plist.to_string_lossy()])
                .status();
            if plist.exists() {
                std::fs::remove_file(&plist)?;
            }
            println!("Uninstalled launchd agent {LAUNCHD_LABEL}.");
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
            let plist_present = plist_path().map(|p| p.exists()).unwrap_or(false);
            let loaded = launchctl_has_label();
            let daemon_state = if !plist_present && !loaded {
                "not installed — run `decent install`"
            } else if loaded {
                "running"
            } else {
                "paused — run `decent resume` (or `uninstall` to remove)"
            };
            println!("daemon      : {daemon_state}");

            // Legacy daemon detection — the old decent-node agent still running
            // with its token at ~/.config/decent-node/. This is expected during
            // migration; tell the user how to complete it.
            if legacy_daemon_is_loaded() {
                println!("⚠ legacy    : com.decent-render.decent-node daemon is still running.");
                println!(
                    "               Run `decent install` to migrate the token + daemon label."
                );
            }
            // Live daemon state from the snapshot the running daemon writes.
            match read_daemon_snapshot() {
                Some(s) if s.is_fresh() => {
                    println!("connection  : {}", s.connection);
                    match s.current_job {
                        Some((id, phase, prog)) => {
                            let pct = (prog.clamp(0.0, 1.0) * 100.0).round() as u32;
                            println!("current job : {id} · {phase} · {pct}%");
                        }
                        None => println!("current job : idle"),
                    }
                    println!(
                        "jobs        : {} done · {} failed · {} canceled",
                        s.jobs_completed, s.jobs_failed, s.jobs_canceled
                    );
                    println!(
                        "update      : {}",
                        match s.update_available {
                            Some(v) => {
                                format!("⚠ {v} available — `brew upgrade decent` + restart")
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
                    if loaded {
                        println!("connection  : (no live snapshot — daemon starting, or an older binary)");
                    }
                    println!("update      : up to date");
                }
            }
            Ok(())
        }

        Command::Upgrade => {
            // 1. brew upgrade decent — swaps the binary on disk. The
            //    running `upgrade` process keeps its old in-memory copy; the
            //    NEXT invocation uses the new binary.
            let brew = std::process::Command::new("brew")
                .args(["upgrade", "decent"])
                .status();
            match brew {
                Ok(s) if s.success() => {}
                Ok(s) => {
                    anyhow::bail!("`brew upgrade decent` failed (exit {:?})", s.code())
                }
                Err(_) => anyhow::bail!(
                    "Could not run `brew` — is Homebrew installed? Upgrade manually and restart."
                ),
            }
            println!("Upgraded decent via Homebrew.");
            // 2. Restart the daemon so launchd relaunches with the new binary.
            //    Only if the agent is loaded; KeepAlive makes `kickstart -k`
            //    sufficient (kill + relaunch).
            if launchctl_has_label() {
                if let Some(uid) = current_uid() {
                    let target = format!("gui/{uid}/{LAUNCHD_LABEL}");
                    let kicked = std::process::Command::new("launchctl")
                        .args(["kickstart", "-k", &target])
                        .status()
                        .map(|s| s.success())
                        .unwrap_or(false);
                    if kicked {
                        println!("Daemon restarted (kickstart -k {target}) — new binary loaded.");
                    } else {
                        println!("launchctl kickstart failed; restart manually:");
                        println!("  launchctl kickstart -k gui/$(id -u)/{LAUNCHD_LABEL}");
                    }
                } else {
                    println!("Could not determine UID; restart the daemon manually:");
                    println!("  launchctl kickstart -k gui/$(id -u)/{LAUNCHD_LABEL}");
                }
            } else {
                println!("Launchd agent not loaded — run `decent start` to use the new version.");
            }
            Ok(())
        }

        Command::Pause => {
            let plist = plist_path()?;
            if !plist.exists() {
                anyhow::bail!("No launchd agent installed — run `decent install` first.");
            }
            if let Some(uid) = current_uid() {
                let target = format!("gui/{uid}/{LAUNCHD_LABEL}");
                // bootout returns non-zero if the agent isn't loaded — that's
                // "already stopped", not an error.
                let stopped = std::process::Command::new("launchctl")
                    .args(["bootout", &target])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false);
                if stopped {
                    println!("Daemon paused — disconnected from dispatch, not rendering.");
                    println!("Run `decent resume` to start it again.");
                } else {
                    println!("Daemon wasn't running (already paused).");
                }
                Ok(())
            } else {
                anyhow::bail!("Could not determine UID.")
            }
        }

        Command::Resume => {
            let plist = plist_path()?;
            if !plist.exists() {
                anyhow::bail!("No launchd agent installed — run `decent install` first.");
            }
            if let Some(uid) = current_uid() {
                let domain = format!("gui/{uid}");
                let bootstrapped = std::process::Command::new("launchctl")
                    .args(["bootstrap", &domain, &plist.to_string_lossy()])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false);
                if bootstrapped {
                    println!("Daemon resumed — reconnecting to dispatch.");
                } else {
                    // bootstrap fails if already loaded — kick it instead.
                    let target = format!("gui/{uid}/{LAUNCHD_LABEL}");
                    let kicked = std::process::Command::new("launchctl")
                        .args(["kickstart", &target])
                        .status()
                        .map(|s| s.success())
                        .unwrap_or(false);
                    if kicked {
                        println!("Daemon was already loaded — kicked (running).");
                    } else {
                        anyhow::bail!(
                            "Could not resume the daemon. Try `decent install` to reload it."
                        );
                    }
                }
                Ok(())
            } else {
                anyhow::bail!("Could not determine UID.")
            }
        }

        Command::Tui {
            dispatch_url,
            token,
            allow_real_jobs,
        } => {
            let token = resolve_token(token)?;
            let register = build_register(allow_real_jobs);
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
