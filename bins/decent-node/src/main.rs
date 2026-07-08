//! decent-node — thin CLI over supervisor-core.
//!
//! `decent-node start --dispatch-url ws://localhost:8790/ws --token <jwt>`
//! (or env vars DISPATCH_URL / WORKER_TOKEN). Registers with the dispatch and
//! heartbeats; real rendering requires `--allow-real-jobs`.
//!
//! The CLI and the Tauri app share the same `connection::run` code path.
//! The only difference: the CLI passes `Observability::default()` (tracing
//! only), the app passes one with status/log channels attached.

use clap::{Parser, Subcommand};
use supervisor_core::connection::{self, ConnectionConfig};
use supervisor_core::protocol::{Capabilities, Platform, RegisterMessage, PROTOCOL_VERSION};
use supervisor_core::status::Observability;

const SUPERVISOR_VERSION: &str = "rust-0.0.1";
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Start {
            dispatch_url,
            token,
            heartbeat_limit,
            allow_real_jobs,
        } => {
            // Resolve token: explicit --token / WORKER_TOKEN env, else the
            // token file written by `decent-node login`.
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
            let register = RegisterMessage {
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
            };
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
            // CLI uses tracing-only observability (no status/log channels).
            let obs = Observability::default();
            obs.set_allow_real_jobs(allow_real_jobs);
            // CLI never signals shutdown — runs until heartbeat-limit or server close.
            let (_shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
            connection::run(&config, &register, &obs, shutdown_rx).await?;
            tracing::info!("decent-node exited cleanly");
            Ok(())
        }

        Command::Login { app_url } => {
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
            println!(
                "agent plist  : {}",
                if plist_present {
                    "yes — `decent-node uninstall` to remove"
                } else {
                    "no  — run `decent-node install`"
                }
            );
            let loaded = std::process::Command::new("launchctl")
                .arg("list")
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).contains(LAUNCHD_LABEL))
                .unwrap_or(false);
            println!(
                "agent loaded : {}",
                if loaded {
                    "yes — running under launchd"
                } else {
                    "no"
                }
            );
            Ok(())
        }
    }
}
