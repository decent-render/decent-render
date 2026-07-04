//! Decent Render App — Tauri backend.
//!
//! Bridges supervisor-core to the webview. The supervisor connection runs in
//! a background tokio task; status updates and log lines flow to the UI via
//! Tauri events via proper async forwarder tasks (no polling). UI commands
//! (start, stop, toggle) flow back through the shared Observability bundle.
//!
//! This is the proof of "two skins, one core": the app drives the exact same
//! `connection::run` as the CLI. The only difference is that channels are
//! attached.

#![cfg_attr(
    all(not(debug_assertions), target_os = "macos"),
    windows_subsystem = "windows"
)]

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use supervisor_core::connection::{self, ConnectionConfig};
use supervisor_core::protocol::{Capabilities, Platform, RegisterMessage, PROTOCOL_VERSION};
use supervisor_core::status::{ConnectionState, LogLine, Observability, SupervisorStatus};
use tauri::{Emitter, State};
use tokio::sync::Mutex;

const SUPERVISOR_VERSION: &str = "rust-0.0.1-app";
const TENANT: &str = "driffs";

/// App state: the observability bundle + a handle to abort the connection task.
struct AppState {
    obs: Observability,
    conn_handle: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
}

/// Persisted app config (dispatch URL + workdir + allow-real-jobs default).
/// The token is stored separately in the OS keychain.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AppConfig {
    dispatch_url: String,
    workdir_root: Option<String>,
    allow_real_jobs_default: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            dispatch_url: "ws://localhost:8790/ws".into(),
            workdir_root: None,
            allow_real_jobs_default: false,
        }
    }
}

fn config_path() -> std::path::PathBuf {
    let dir = dirs_next::config_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("decent-render");
    let _ = std::fs::create_dir_all(&dir);
    dir.join("config.json")
}

fn load_config() -> AppConfig {
    match std::fs::read_to_string(config_path()) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => AppConfig::default(),
    }
}

fn save_config(config: &AppConfig) {
    if let Ok(text) = serde_json::to_string_pretty(config) {
        let _ = std::fs::write(config_path(), text);
    }
}

// ── Token storage (OS keychain via keyring crate) ──────────────────────────

const KEYCHAIN_SERVICE: &str = "decent-render";
const KEYCHAIN_USER: &str = "worker-token";

fn load_token() -> String {
    keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_USER)
        .and_then(|e| e.get_password())
        .unwrap_or_default()
}

fn save_token(token: &str) {
    if let Ok(entry) = keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_USER) {
        if token.is_empty() {
            let _ = entry.delete_credential();
        } else {
            let _ = entry.set_password(token);
        }
    }
}

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
    0
}

fn make_register(allow_real_jobs: bool) -> RegisterMessage {
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

// ── Tauri commands ─────────────────────────────────────────────────────────

#[tauri::command]
fn get_config() -> AppConfig {
    load_config()
}

#[tauri::command]
fn save_app_config(
    dispatch_url: String,
    workdir_root: Option<String>,
    allow_real_jobs_default: bool,
) {
    save_config(&AppConfig {
        dispatch_url,
        workdir_root,
        allow_real_jobs_default,
    });
}

#[tauri::command]
fn get_token() -> String {
    load_token()
}

#[tauri::command]
fn save_token_cmd(token: String) {
    save_token(&token);
}

#[tauri::command]
fn get_status(state: State<'_, AppState>) -> SupervisorStatus {
    state.obs.borrow_status()
}

#[tauri::command]
fn get_allow_real_jobs(state: State<'_, AppState>) -> bool {
    state.obs.allows_real_jobs()
}

#[tauri::command]
fn set_allow_real_jobs(state: State<'_, AppState>, value: bool) {
    state.obs.set_allow_real_jobs(value);
}

#[tauri::command]
async fn start_connection(
    state: State<'_, AppState>,
    app: tauri::AppHandle,
    dispatch_url: String,
    token: String,
) -> Result<(), String> {
    let mut handle_guard = state.conn_handle.lock().await;
    if handle_guard.is_some() {
        return Err("Connection already running".into());
    }

    // Reset status for a fresh connection.
    state.obs.update_status(|s| {
        s.connection = ConnectionState::Disconnected;
        s.current_job = None;
        s.last_error = None;
    });
    state.obs.log(LogLine::info("Starting connection…"));

    // Save token to keychain for next launch.
    save_token(&token);

    let obs = state.obs.clone();
    let register = make_register(obs.allows_real_jobs());
    let config = ConnectionConfig::new(dispatch_url, token);

    // Spawn two forwarder tasks: one for status, one for logs.
    // Each consumes its own receiver — no polling, no duplication.
    let status_app = app.clone();
    let status_rx = obs.status_tx.as_ref().map(|tx| tx.subscribe());
    let status_forward = tokio::spawn(async move {
        let mut rx = match status_rx {
            Some(rx) => rx,
            None => return,
        };
        // Emit initial snapshot.
        let _ = status_app.emit("status-update", &*rx.borrow());
        while rx.changed().await.is_ok() {
            let status = rx.borrow_and_update().clone();
            let _ = status_app.emit("status-update", &status);
        }
    });

    let log_app = app.clone();
    let log_rx = obs.log_tx.as_ref().map(|tx| tx.subscribe());
    let log_forward = tokio::spawn(async move {
        let mut rx = match log_rx {
            Some(rx) => rx,
            None => return,
        };
        while let Ok(line) = rx.recv().await {
            let _ = log_app.emit("log-line", &line);
        }
    });

    let handle = tokio::spawn(async move {
        let result = connection::run(&config, &register, &obs).await;
        match result {
            Ok(()) => {
                obs.log(LogLine::info("Connection closed"));
            }
            Err(e) => {
                obs.log(LogLine::error(format!("Connection error: {e}")));
            }
        }
        // Forwarders exit when the status/log senders are dropped (which won't
        // happen here since obs lives in AppState). Aborting them explicitly
        // would break reconnection; instead they sit idle waiting for the next
        // connection.
    });

    // When the connection task finishes, clean up forwarders.
    let forward_guard = tokio::spawn(async move {
        handle.await.ok();
        status_forward.abort();
        log_forward.abort();
    });

    *handle_guard = Some(forward_guard);
    Ok(())
}

#[tauri::command]
async fn stop_connection(state: State<'_, AppState>) -> Result<(), String> {
    let mut handle_guard = state.conn_handle.lock().await;
    if let Some(handle) = handle_guard.take() {
        handle.abort();
        state.obs.update_status(|s| {
            s.connection = ConnectionState::Disconnected;
            s.current_job = None;
        });
        state
            .obs
            .log(LogLine::info("Connection stopped by operator"));
    }
    Ok(())
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let (obs, _status_rx, _log_rx) = Observability::channels(SupervisorStatus::default());

    // Load saved config and initialize allow flag.
    let saved = load_config();
    obs.set_allow_real_jobs(saved.allow_real_jobs_default);

    let app_state = AppState {
        obs,
        conn_handle: Arc::new(Mutex::new(None)),
    };

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .manage(app_state)
        .invoke_handler(tauri::generate_handler![
            get_config,
            save_app_config,
            get_token,
            save_token_cmd,
            get_status,
            get_allow_real_jobs,
            set_allow_real_jobs,
            start_connection,
            stop_connection,
        ])
        .setup(|_app| {
            tracing::info!("Decent Render app starting");
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
