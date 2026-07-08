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

/// OS keychain entry — shared with the Tauri app, so a token stored by either
/// surface is read by the other.
const KEYCHAIN_SERVICE: &str = "decent-render";
const KEYCHAIN_USER: &str = "worker-token";

fn load_token() -> String {
    keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_USER)
        .and_then(|e| e.get_password())
        .unwrap_or_default()
}

fn save_token(token: &str) -> anyhow::Result<()> {
    let entry = keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_USER)?;
    if token.is_empty() {
        entry.delete_credential()?;
    } else {
        entry.set_password(token)?;
    }
    Ok(())
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
        /// stored by `decent-node login` from the OS keychain.
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
    /// token, and store it in the OS keychain for `start` to use.
    Login {
        /// The web app URL to pair against.
        #[arg(long, env = "APP_URL", default_value = "https://decent-riffs.com")]
        app_url: String,
    },
    /// Forget the stored worker token (clears the OS keychain entry).
    Logout,
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
            // keychain entry written by `decent-node login`.
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
            let pairing_url =
                format!("{}/settings/devices", app_url.trim_end_matches('/'));
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
            println!("Token saved to the OS keychain. Run `decent-node start` to connect.");
            Ok(())
        }

        Command::Logout => {
            match keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_USER) {
                Ok(entry) => match entry.delete_credential() {
                    Ok(()) => println!("Stored token cleared."),
                    Err(keyring::Error::NoEntry) => println!("No stored token to clear."),
                    Err(err) => anyhow::bail!("failed to clear keychain entry: {err}"),
                },
                Err(err) => anyhow::bail!("failed to open keychain: {err}"),
            }
            Ok(())
        }
    }
}
