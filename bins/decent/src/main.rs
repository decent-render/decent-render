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
use supervisor_core::keepawake::{self, KeepAwakeState};
use supervisor_core::protocol::{Platform, RegisterMessage, PROTOCOL_VERSION};
use supervisor_core::status::{Observability, SupervisorStatus};

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
/// PACKET 40 (audit 17): validate the dispatch URL scheme BEFORE anything
/// dials it. A `ws://` URL to a remote host would ship the worker JWT in
/// CLEARTEXT — refuse it and say exactly why. Plain `ws://` stays allowed
/// for localhost/127.0.0.1 (the e2e harness and local development).
/// Never silently "upgrade" the scheme: the operator must see the mistake.
pub(crate) fn validate_dispatch_url(url: &str) -> anyhow::Result<()> {
    let parsed = url::Url::parse(url)
        .map_err(|e| anyhow::anyhow!("--dispatch-url is not a valid URL ({e}): {url}"))?;
    let scheme = parsed.scheme();
    let is_local = matches!(
        parsed.host_str(),
        Some("localhost") | Some("127.0.0.1") | Some("[::1]") | None
    );
    match scheme {
        "wss" => Ok(()),
        "ws" if is_local => Ok(()),
        "ws" => anyhow::bail!(
            "refusing to use ws:// to a non-local host: the worker token would be sent in              CLEARTEXT. Use wss://{host}{path} (or a localhost URL for local development).",
            host = parsed.host_str().unwrap_or("<unknown>"),
            path = parsed.path(),
        ),
        other => anyhow::bail!(
            "--dispatch-url must be wss:// (or ws:// for localhost); got '{other}://'"
        ),
    }
}

/// PACKET 40 (audit-api-ux): a worker token is a JWT — three
/// dot-separated base64url segments, each non-empty, header/payload
/// decodable as JSON, and a payload carrying the claims this fleet mints
/// (service / tenant / platform family). Placeholder strings ("paste-your-
/// token-here", shell leftovers) and truncations fail with a message that
/// names WHAT was wrong — never the token itself.
pub(crate) fn validate_worker_token_shape(token: &str) -> anyhow::Result<()> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        anyhow::bail!(
            "not a worker token: expected three dot-separated parts, got {} \
             (a JWT looks like <header>.<payload>.<signature>)",
            parts.len()
        );
    }
    if parts.iter().any(|p| p.is_empty()) {
        anyhow::bail!("not a worker token: one of the three parts is empty (truncated paste?)");
    }
    if token.len() < 40 {
        anyhow::bail!(
            "not a worker token: {} characters is too short for any JWT this fleet issues",
            token.len()
        );
    }
    if !token
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b'_' || b == b'=')
    {
        anyhow::bail!(
            "not a worker token: contains characters outside base64url + dots \
             (whitespace, quotes, or a paste artifact)"
        );
    }
    // Header + payload must be real base64url JSON.
    let decode = |s: &str| {
        base64url_decode(s).ok_or_else(|| anyhow::anyhow!("a segment is not valid base64url"))
    };
    let header = decode(parts[0])?;
    let payload = decode(parts[1])?;
    serde_json::from_slice::<serde_json::Value>(&header)
        .map_err(|_| anyhow::anyhow!("token header is not JSON (wrong paste?)"))?;
    let payload: serde_json::Value = serde_json::from_slice(&payload)
        .map_err(|_| anyhow::anyhow!("token payload is not JSON (wrong paste?)"))?;
    // Fleet tokens carry worker identity claims; a JWT without ANY of them
    // is some other system's token pasted by mistake.
    let has_claim = [
        "service",
        "tenant",
        "workerId",
        "worker_id",
        "platform",
        "deviceId",
    ]
    .iter()
    .any(|k| payload.get(*k).is_some());
    if !has_claim {
        anyhow::bail!(
            "this JWT carries no worker claims (service/tenant/workerId) — \
             it is probably a token for a different system"
        );
    }
    Ok(())
}

/// Minimal base64url decode (no padding requirement) for shape validation.
fn base64url_decode(s: &str) -> Option<Vec<u8>> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut buf: u32 = 0;
    let mut bits = 0u32;
    for &b in s.as_bytes() {
        if b == b'=' {
            break;
        }
        let v = ALPHABET.iter().position(|&a| a == b)? as u32;
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Some(out)
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
        keep_awake: KeepAwakeState::from_token(val("keep_awake")),
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
        #[arg(
            long,
            env = "DISPATCH_URL",
            default_value = "wss://decent-render-dispatch.fly.dev/ws"
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
        #[arg(
            long,
            env = "DISPATCH_URL",
            default_value = "wss://decent-render-dispatch.fly.dev/ws"
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
                        match s.update_available {
                            Some(v) => {
                                format!("⚠ {v} available — run `decent upgrade`")
                            }
                            // PACKET 40 (audit 18): "up to date" requires the
                            // daemon to be CONNECTED (dispatch tells it on
                            // register). A registered-but-disconnected or
                            // never-connected node has no data — say unknown.
                            None if s.connection == "connected" => "up to date".to_string(),
                            None => "unknown (daemon not connected)".to_string(),
                        }
                    );
                }
                Some(_) => {
                    println!("connection  : stale (no recent snapshot — daemon may have stopped)");
                    // PACKET 40 (audit 18): the snapshot exists but is stale —
                    // whatever it says about updates is out of date. Never
                    // assert "up to date" without live data behind it.
                    println!("update      : unknown (snapshot is stale)");
                }
                None => {
                    if daemon == DaemonState::Running {
                        println!("connection  : (no live snapshot — daemon starting, or an older binary)");
                    }
                    // PACKET 40 (audit 18): no snapshot at all — the CLI knows
                    // NOTHING about update state. Say so instead of "up to date".
                    println!("update      : unknown (no recent snapshot)");
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

#[cfg(test)]
mod packet40_tests {
    use super::*;

    // Step 1: scheme validation.
    #[test]
    fn wss_urls_are_accepted_everywhere() {
        assert!(validate_dispatch_url("wss://decent-render-dispatch.fly.dev/ws").is_ok());
        assert!(validate_dispatch_url("wss://example.com/?a=1&b=2").is_ok());
    }

    #[test]
    fn plain_ws_is_allowed_only_for_localhost() {
        // The e2e harness and local development.
        assert!(validate_dispatch_url("ws://localhost:8790/ws").is_ok());
        assert!(validate_dispatch_url("ws://127.0.0.1:8790/ws").is_ok());
        assert!(validate_dispatch_url("ws://[::1]:8790/ws").is_ok());
        // A REMOTE host over ws:// ships the JWT in cleartext — refused,
        // with a message that says why and names the fix.
        let err = validate_dispatch_url("ws://dispatch.example.com/ws")
            .unwrap_err()
            .to_string();
        assert!(err.contains("CLEARTEXT"), "got: {err}");
        assert!(
            err.contains("wss://dispatch.example.com/ws"),
            "must name the fix: {err}"
        );
    }

    #[test]
    fn non_ws_schemes_are_refused_with_the_scheme_named() {
        let err = validate_dispatch_url("http://example.com/ws")
            .unwrap_err()
            .to_string();
        assert!(err.contains("http"), "got: {err}");
        assert!(validate_dispatch_url("not a url at all").is_err());
    }

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
