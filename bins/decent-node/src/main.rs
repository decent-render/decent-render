//! decent-node — thin CLI over supervisor-core.
//!
//! `decent-node start --dispatch-url ws://localhost:8790/ws --token <jwt>`
//! (or env vars DISPATCH_URL / WORKER_TOKEN). Registers with the dispatch and
//! heartbeats; job execution lands in a later supervisor version.

use clap::{Parser, Subcommand};
use supervisor_core::connection::{self, ConnectionConfig};
use supervisor_core::protocol::{self, Capabilities, Platform, RegisterMessage, PROTOCOL_VERSION};

const SUPERVISOR_VERSION: &str = "rust-0.0.1";
const TENANT: &str = "driffs";

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
        /// Worker JWT (mint via the platform's mint-worker-token script).
        #[arg(long, env = "WORKER_TOKEN")]
        token: String,
        /// Exit cleanly after this many heartbeats (smoke-test mode).
        #[arg(long)]
        heartbeat_limit: Option<u32>,
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
            connection::run(&config, &register, &mut |_: &protocol::ServerMessage| {}).await?;
            tracing::info!("decent-node exited cleanly");
            Ok(())
        }
    }
}
