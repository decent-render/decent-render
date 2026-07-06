//! supervisor-core — the open-source core of the Decent render network node
//! supervisor.
//!
//! Five pillars:
//! - [`protocol`] — the dispatch ⇄ worker wire contract (protocol v2), a Rust
//!   mirror of the platform's `protocol.ts`.
//! - [`connection`] — the single outbound WebSocket: register, heartbeat,
//!   message pump. NAT-friendly by construction (GitHub-Actions-runner model).
//! - [`runner`] — job execution orchestration: download the assigned payload,
//!   verify its sha256, extract it, spawn the bundled `decent-render-runner`,
//!   stream progress/done/error NDJSON, upload the output, honor cancel. Gated
//!   behind `allow_real_jobs` (default off).
//! - [`status`] — observable status bus: `watch::channel` for connection /
//!   job state + `broadcast::channel` for log lines, so a UI (or tracing) can
//!   attach without coupling to the connection loop.
//! - [`purge`] — the purge rule as a type: per-job workdirs that cannot
//!   outlive the job. Auditable source is the point.

pub mod connection;
pub mod protocol;
pub mod purge;
pub mod runner;
pub mod status;
