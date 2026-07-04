//! supervisor-core — the open-source core of the Decent render network node
//! supervisor.
//!
//! Three pillars:
//! - [`protocol`] — the dispatch ⇄ worker wire contract (protocol v2), a Rust
//!   mirror of the platform's `protocol.ts`.
//! - [`connection`] — the single outbound WebSocket: register, heartbeat,
//!   message pump. NAT-friendly by construction (GitHub-Actions-runner model).
//! - [`purge`] — the purge rule as a type: per-job workdirs that cannot
//!   outlive the job. Auditable source is the point.

pub mod connection;
pub mod protocol;
pub mod purge;
pub mod runner;
pub mod status;
