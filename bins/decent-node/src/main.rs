//! decent-node — thin CLI over supervisor-core.
//!
//! `decent-node start --dispatch-url ws://localhost:8790/ws --token <jwt>`
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
const TENANT: &str = "driffs";

/// Token storage: a 0600 file at ~/.config/decent-node/worker-token. Not the
/// macOS Keychain — the Keychain prompts on access for an unsigned binary,
/// which is hostile CLI UX; a revocable per-device worker token is fine in a
/// user-only file, the way `gh` / `npm` store theirs.
fn token_path() -> anyhow::Result<std::path::PathBuf> {
    let home = std::env::var_os("HOME")
        .ok_or_else(|| anyhow::anyhow!("HOME is not set; cannot locate token file"))?;
    Ok(std::path::PathBuf::from(home).join(".config/decent-node/worker-token"))
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

#[cfg(unix)]
fn set_owner_only(path: &std::path::Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
}

#[cfg(not(unix))]
fn set_owner_only(_path: &std::path::Path, _mode: u32) {}

/// launchd label for the installed agent.
const LAUNCHD_LABEL: &str = "com.decent-render.decent-node";

fn launch_agents_dir() -> anyhow::Result<std::path::PathBuf> {
    let home = std::env::var_os("HOME").ok_or_else(|| anyhow::anyhow!("HOME is not set"))?;
    let dir = std::path::PathBuf::from(home).join("Library/LaunchAgents");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn plist_path() -> anyhow::Result<std::path::PathBuf> {
    Ok(launch_agents_dir()?.join(format!("{LAUNCHD_LABEL}.plist")))
}

/// Build the launchd agent plist: runs `decent-node start --allow-real-jobs` at
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
    name = "decent-node",
    version,
    about = "Decent render network node supervisor"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Connect to the dispatch service, register, and heartbeat.
    Start {
        /// Dispatch WebSocket URL.
        #[arg(long, env = "DISPATCH_URL", default_value = "ws://localhost:8790/ws")]
        dispatch_url: String,
        /// Worker JWT. If omitted (and no WORKER_TOKEN env), reads the token
        /// stored by `decent-node login` (the token file).
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
        #[arg(long, env = "APP_URL", default_value = "https://decent-riffs.com")]
        app_url: String,
        /// Store a worker token directly instead of opening the web pairing
        /// page. For company/internal tokens minted via
        /// `scripts/mint-worker-token.ts` (skips the self-serve device flow).
        #[arg(long)]
        token: Option<String>,
    },
    /// Forget the stored worker token (clears the token file).
    Logout,
    /// Install as a macOS launchd agent: runs `decent-node start` at login and
    /// restarts on exit (KeepAlive), so the node renders unattended. Accepts
    /// real jobs. Run `decent-node login` first to store a token.
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
    /// Upgrade decent-node via Homebrew, then restart the daemon (if loaded)
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
        /// stored by `decent-node login`.
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
/// else the token file written by `decent-node login`. Errors if none.
fn resolve_token(token: Option<String>) -> anyhow::Result<String> {
    let token = match token {
        Some(t) if !t.trim().is_empty() => t.trim().to_string(),
        _ => load_token(),
    };
    if token.is_empty() {
        anyhow::bail!(
            "No worker token. Run `decent-node login` to pair this machine, \
             or pass --token / set WORKER_TOKEN."
        );
    }
    Ok(token)
}

/// Build the register message from probed hardware + the real-jobs flag.
/// Shared by every foreground command (`start`, `tui`).
fn build_register(allow_real_jobs: bool) -> RegisterMessage {
    RegisterMessage {
        tenant: TENANT.into(),
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

/** Is the decent-node launchd agent currently loaded? */
fn launchctl_has_label() -> bool {
    std::process::Command::new("launchctl")
        .arg("list")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains(LAUNCHD_LABEL))
        .unwrap_or(false)
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

    // The TUI runs in the alternate screen; tracing-to-stderr would leave
    // leftover text on exit. Skip the subscriber in TUI mode — the connection
    // loop emits its events via the obs.log() channel, which the TUI renders
    // directly, so nothing important is lost.
    if !matches!(cli.command, Command::Tui { .. }) {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "info".into()),
            )
            .init();
    }

    match cli.command {
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
                "starting decent-node {SUPERVISOR_VERSION}"
            );
            let config = ConnectionConfig {
                heartbeat_limit,
                allow_real_jobs,
                ..ConnectionConfig::new(dispatch_url, token)
            };
            // CLI uses real status channels so a background task can persist
            // `updateAvailable` for `decent-node status` to surface.
            let (obs, _status_rx, _log_rx) = Observability::channels(SupervisorStatus::default());
            obs.set_allow_real_jobs(allow_real_jobs);
            // Persist update-available state to a file the separate `status`
            // command can read. Cleared on each connect (optimistic up-to-date).
            let obs_persist = obs.clone();
            if let Some(path) = token_path()
                .ok()
                .and_then(|p| p.parent().map(|d| d.join("update-available")))
            {
                tokio::spawn(async move {
                    loop {
                        let latest = obs_persist.borrow_status().update_available.clone();
                        let _ = match latest {
                            Some(v) => std::fs::write(&path, v),
                            None => std::fs::remove_file(&path).or_else(|e| {
                                if e.kind() == std::io::ErrorKind::NotFound {
                                    Ok(())
                                } else {
                                    Err(e)
                                }
                            }),
                        };
                        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                    }
                });
            }
            // CLI never signals shutdown — runs until heartbeat-limit or server close.
            let (_shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
            connection::run(&config, &register, &obs, shutdown_rx).await?;
            tracing::info!("decent-node exited cleanly");
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
                println!("Token saved to ~/.config/decent-node/worker-token (0600).");
                println!("Run `decent-node start`, or `decent-node install` for the daemon.");
                return Ok(());
            }
            let pairing_url = format!("{}/settings/devices", app_url.trim_end_matches('/'));
            println!("Open this page to issue a worker token for this machine:");
            println!("  {pairing_url}");
            // Best-effort browser open (no-op on a headless box); never fatal.
            let _ = open::that(&pairing_url);
            println!();
            println!("After issuing the token, paste it here (shown once on the page):");
            let mut line = String::new();
            std::io::stdin().read_line(&mut line)?;
            let token = line.trim().to_string();
            if token.split('.').count() != 3 {
                anyhow::bail!(
                    "That doesn't look like a worker token (expected three dot-separated parts). \
                     Re-run `decent-node login`."
                );
            }
            save_token(&token)?;
            println!("Token saved to ~/.config/decent-node/worker-token (0600). Run `decent-node start` to connect.");
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
                    "No worker token stored. Run `decent-node login` first, then `decent-node install`."
                );
            }
            let exe = std::env::current_exe()?;
            let plist = plist_path()?;
            let log_path = token_path()?
                .parent()
                .ok_or_else(|| anyhow::anyhow!("token file has no parent"))?
                .join("decent-node.log");
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
            println!("Runs `decent-node start --allow-real-jobs` at login; restarts on exit (KeepAlive).");
            println!("Tip: run `decent-node login` first if this machine has no token yet.");
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
                    "NO  — run `decent-node login`"
                } else {
                    "yes"
                }
            );
            let plist_present = plist_path().map(|p| p.exists()).unwrap_or(false);
            let loaded = launchctl_has_label();
            let daemon_state = if !plist_present {
                "not installed — run `decent-node install`"
            } else if loaded {
                "running"
            } else {
                "paused — run `decent-node resume` (or `uninstall` to remove)"
            };
            println!("daemon      : {daemon_state}");
            let update = token_path()
                .ok()
                .and_then(|p| p.parent().map(|d| d.join("update-available")))
                .and_then(|p| std::fs::read_to_string(&p).ok())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            println!(
                "update       : {}",
                match update {
                    Some(v) => {
                        format!("⚠ {v} available — `brew upgrade decent-node` + restart")
                    }
                    None => "up to date".to_string(),
                }
            );
            Ok(())
        }

        Command::Upgrade => {
            // 1. brew upgrade decent-node — swaps the binary on disk. The
            //    running `upgrade` process keeps its old in-memory copy; the
            //    NEXT invocation uses the new binary.
            let brew = std::process::Command::new("brew")
                .args(["upgrade", "decent-node"])
                .status();
            match brew {
                Ok(s) if s.success() => {}
                Ok(s) => {
                    anyhow::bail!("`brew upgrade decent-node` failed (exit {:?})", s.code())
                }
                Err(_) => anyhow::bail!(
                    "Could not run `brew` — is Homebrew installed? Upgrade manually and restart."
                ),
            }
            println!("Upgraded decent-node via Homebrew.");
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
                println!(
                    "Launchd agent not loaded — run `decent-node start` to use the new version."
                );
            }
            Ok(())
        }

        Command::Pause => {
            let plist = plist_path()?;
            if !plist.exists() {
                anyhow::bail!(
                    "No launchd agent installed — run `decent-node install` first."
                );
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
                    println!("Run `decent-node resume` to start it again.");
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
                anyhow::bail!(
                    "No launchd agent installed — run `decent-node install` first."
                );
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
                            "Could not resume the daemon. Try `decent-node install` to reload it."
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
            let (obs, status_rx, log_rx) =
                Observability::channels(SupervisorStatus::default());
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
