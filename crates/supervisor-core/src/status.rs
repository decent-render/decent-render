//! Observable status types + the observability channel bundle.
//!
//! The connection loop ([`crate::connection::run`]) accepts an
//! [`Observability`] reference. When channels are attached (Tauri app),
//! the loop emits structured status updates and tailable log lines.
//! When they are `None` (CLI), the loop only uses `tracing`.
//!
//! This is how "two skins, one core" works: the same `run()` drives
//! both the headless CLI and the GUI app — the only difference is
//! whether anyone is listening.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::{broadcast, watch};

use crate::protocol::PROTOCOL_VERSION;

// ── Status types ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ConnectionState {
    Disconnected,
    Connecting,
    Connected,
    Registered,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum JobPhase {
    Downloading,
    Rendering,
    Uploading,
    Done,
    Failed,
    Canceled,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobStatus {
    pub id: String,
    pub tier: String,
    pub progress: f64,
    pub phase: JobPhase,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeIdentity {
    pub chip: String,
    pub platform: String,
    pub protocol_version: u32,
    pub supervisor_version: String,
}

impl NodeIdentity {
    pub fn from_register_fields(chip: &str, platform: &str, supervisor_version: &str) -> Self {
        Self {
            chip: chip.to_string(),
            platform: platform.to_string(),
            protocol_version: PROTOCOL_VERSION,
            supervisor_version: supervisor_version.to_string(),
        }
    }
}

/// The complete observable state snapshot. Pushed via `watch` on every change.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SupervisorStatus {
    pub connection: ConnectionState,
    pub dispatch_url: Option<String>,
    pub node_identity: Option<NodeIdentity>,
    pub current_job: Option<JobStatus>,
    pub jobs_completed: u64,
    pub jobs_failed: u64,
    pub jobs_canceled: u64,
    pub last_error: Option<String>,
    pub allow_real_jobs: bool,
}

impl Default for SupervisorStatus {
    fn default() -> Self {
        Self {
            connection: ConnectionState::Disconnected,
            dispatch_url: None,
            node_identity: None,
            current_job: None,
            jobs_completed: 0,
            jobs_failed: 0,
            jobs_canceled: 0,
            last_error: None,
            allow_real_jobs: false,
        }
    }
}

// ── Log stream ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LogLine {
    pub timestamp_ms: u64,
    pub level: LogLevel,
    pub message: String,
}

impl LogLine {
    pub fn new(level: LogLevel, msg: impl Into<String>) -> Self {
        let timestamp_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        Self {
            timestamp_ms,
            level,
            message: msg.into(),
        }
    }

    pub fn info(msg: impl Into<String>) -> Self {
        Self::new(LogLevel::Info, msg)
    }

    pub fn warn(msg: impl Into<String>) -> Self {
        Self::new(LogLevel::Warn, msg)
    }

    pub fn error(msg: impl Into<String>) -> Self {
        Self::new(LogLevel::Error, msg)
    }
}

// ── Observability bundle ───────────────────────────────────────────────────

/// Bundles the optional status/log channels + the live `allow_real_jobs` flag.
///
/// The `allow_flag` is always present — both the CLI and the app set it.
/// The CLI initializes it from `--allow-real-jobs` and never changes it.
/// The app starts at `false` and toggles it via the UI.
///
/// `status_tx` / `log_tx` are `None` for the CLI (tracing-only), `Some` for
/// the Tauri app (structured event stream to the webview).
#[derive(Clone)]
pub struct Observability {
    pub status_tx: Option<watch::Sender<SupervisorStatus>>,
    pub log_tx: Option<broadcast::Sender<LogLine>>,
    pub allow_flag: Arc<AtomicBool>,
}

impl Default for Observability {
    fn default() -> Self {
        Self {
            status_tx: None,
            log_tx: None,
            allow_flag: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl Observability {
    /// Create an observable bundle with real channels (app mode).
    /// Returns the bundle + receivers for the UI to read.
    pub fn channels(
        initial: SupervisorStatus,
    ) -> (
        Self,
        watch::Receiver<SupervisorStatus>,
        broadcast::Receiver<LogLine>,
    ) {
        let (status_tx, status_rx) = watch::channel(initial);
        let (log_tx, log_rx) = broadcast::channel(512);
        let obs = Self {
            status_tx: Some(status_tx),
            log_tx: Some(log_tx.clone()),
            allow_flag: Arc::new(AtomicBool::new(false)),
        };
        (obs, status_rx, log_rx)
    }

    /// Update the status snapshot if a channel is attached.
    pub fn update_status(&self, f: impl FnOnce(&mut SupervisorStatus)) {
        if let Some(tx) = &self.status_tx {
            tx.send_modify(|s| f(s));
        }
    }

    /// Read a clone of the current status (or default if no channel).
    pub fn borrow_status(&self) -> SupervisorStatus {
        if let Some(tx) = &self.status_tx {
            return tx.borrow().clone();
        }
        SupervisorStatus::default()
    }

    /// Emit a log line if a channel is attached.
    pub fn log(&self, line: LogLine) {
        if let Some(tx) = &self.log_tx {
            let _ = tx.send(line);
        }
    }

    /// Read the live `allow_real_jobs` flag.
    pub fn allows_real_jobs(&self) -> bool {
        self.allow_flag.load(Ordering::Relaxed)
    }

    /// Set the `allow_real_jobs` flag + reflect in status.
    pub fn set_allow_real_jobs(&self, value: bool) {
        self.allow_flag.store(value, Ordering::Relaxed);
        self.update_status(|s| s.allow_real_jobs = value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_obs_refuses_jobs() {
        let obs = Observability::default();
        assert!(!obs.allows_real_jobs());
    }

    #[test]
    fn toggle_allow_flag() {
        // With channels attached, set_allow_real_jobs reflects in the status.
        let (obs, status_rx, _log_rx) = Observability::channels(SupervisorStatus::default());
        assert!(!obs.allows_real_jobs());
        obs.set_allow_real_jobs(true);
        assert!(obs.allows_real_jobs());
        assert!(status_rx.borrow().allow_real_jobs);
    }

    #[tokio::test]
    async fn channels_round_trip() {
        let (obs, status_rx, mut log_rx) = Observability::channels(SupervisorStatus::default());

        obs.update_status(|s| s.connection = ConnectionState::Registered);
        assert_eq!(status_rx.borrow().connection, ConnectionState::Registered);

        obs.log(LogLine::info("hello"));
        let line = log_rx.recv().await.unwrap();
        assert_eq!(line.message, "hello");
        assert_eq!(line.level, LogLevel::Info);
    }

    #[test]
    fn default_obs_is_silent() {
        // No channels → update_status and log are no-ops, no panic.
        let obs = Observability::default();
        obs.update_status(|s| s.connection = ConnectionState::Connected);
        obs.log(LogLine::warn("test"));
        // borrow_status returns default since no channel
        assert_eq!(
            obs.borrow_status().connection,
            ConnectionState::Disconnected
        );
    }
}
