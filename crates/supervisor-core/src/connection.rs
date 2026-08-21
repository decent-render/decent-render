//! The one outbound WebSocket to the dispatch service.
//!
//! Mirrors the connection behavior of the TS reference worker
//! (driffs `scripts/spike-worker.ts`): connect with `?token=` on the URL,
//! send `register` immediately, heartbeat every 20 s, retry the initial
//! connect with a short delay (the dispatch may still be starting), and
//! process server messages inline.
//!
//! The loop accepts an [`Observability`] bundle. When channels are attached
//! (Tauri app), it emits structured status snapshots and tailable log lines.
//! When they are `None` (CLI), it falls back to `tracing` only. Both skins
//! drive the exact same code path.
//!
//! There is no `ServerMessageHandler` trait — the observation mechanism is
//! the [`Observability`] bundle alone. One mechanism, not two.

use std::time::Duration;

use anyhow::{anyhow, Context};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::oneshot;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

use crate::protocol::{
    HeartbeatMessage, JobAcceptedMessage, JobRejectedMessage, RegisterMessage, RejectReason,
    ServerMessage, WorkerMessage,
};
use crate::runner::{run_job, InFlightJob};
use crate::status::{ConnectionState, JobPhase, JobStatus, LogLine, Observability};

#[derive(Debug, Clone)]
pub struct ConnectionConfig {
    /// Dispatch WebSocket URL, e.g. `ws://localhost:8790/ws`.
    pub dispatch_url: String,
    /// Worker JWT; sent as the `?token=` query parameter.
    pub token: String,
    /// Heartbeat period. The dispatch expects 20 s.
    pub heartbeat_interval: Duration,
    /// Initial-connect retries (the TS worker retries 15× at 1 s — the
    /// dispatch may start near-simultaneously with the worker).
    pub max_connect_attempts: u32,
    pub connect_retry_delay: Duration,
    /// If set, close the socket cleanly after sending this many heartbeats.
    /// Used for smoke tests; `None` runs until the server closes.
    pub heartbeat_limit: Option<u32>,
    /// Safety gate: default false refuses jobAssign. Real rendering only runs
    /// when the CLI/env opts in. This is the *initial* value — the live flag
    /// is read from `Observability::allows_real_jobs()` so the app can toggle
    /// it at runtime.
    pub allow_real_jobs: bool,
}

impl ConnectionConfig {
    pub fn new(dispatch_url: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            dispatch_url: dispatch_url.into(),
            token: token.into(),
            heartbeat_interval: Duration::from_secs(20),
            max_connect_attempts: 15,
            connect_retry_delay: Duration::from_secs(1),
            heartbeat_limit: None,
            allow_real_jobs: false,
        }
    }

    /// The handshake request, carrying the worker token in an Authorization
    /// header rather than the query string.
    ///
    /// `?token=` put a long-lived worker credential into every proxy, CDN and
    /// platform access log along the path. Dispatch still accepts the query
    /// parameter for supervisors released before this, so a mixed fleet keeps
    /// working; nothing here needs to send both, and sending both would put the
    /// token back in the logs.
    fn handshake_request(
        &self,
    ) -> anyhow::Result<tokio_tungstenite::tungstenite::handshake::client::Request> {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let mut request = self.dispatch_url.as_str().into_client_request()?;
        let value = format!("Bearer {}", self.token);
        request.headers_mut().insert(
            "authorization",
            value
                .parse()
                .map_err(|_| anyhow!("worker token is not a valid header value"))?,
        );
        Ok(request)
    }
}

/// Connect, register, heartbeat, and process server messages.
///
/// Returns `Ok(())` on a clean self-initiated close (heartbeat limit reached,
/// shutdown signal, or server close) or when the server closes the socket after
/// we ever connected; returns an error if the dispatch is unreachable after all
/// connect attempts.
///
/// `obs` is the sole observation surface. The CLI passes
/// `Observability::default()` (tracing-only); the Tauri app passes one with
/// status/log channels attached. There is no handler callback — everything
/// the caller needs to observe flows through `obs`.
///
/// `shutdown` is a oneshot receiver that triggers graceful shutdown: cancels
/// any in-flight job (SIGTERM runner → purge workdir), sends a Close frame,
/// and returns `Ok(())`. The CLI fires it on SIGTERM/SIGINT — without that the
/// process dies on the signal and `Drop` never runs, leaving the job workdir
/// (user content) on disk. The Tauri app fires it from the Stop button.
///
/// It is honoured during the initial connect-retry loop too, so a node
/// signalled while dispatch is unreachable still exits cleanly.
pub async fn run(
    config: &ConnectionConfig,
    register: &RegisterMessage,
    obs: &Observability,
    mut shutdown: oneshot::Receiver<()>,
) -> anyhow::Result<()> {
    let request = config.handshake_request()?;

    // Initialize status snapshot with identity + dispatch URL.
    obs.update_status(|s| {
        s.connection = ConnectionState::Connecting;
        s.dispatch_url = Some(config.dispatch_url.clone());
        s.node_identity = Some(crate::status::NodeIdentity::from_register_fields(
            &register.chip,
            match register.platform {
                crate::protocol::Platform::Company => "company",
                crate::protocol::Platform::Community => "community",
            },
            &register.supervisor_version,
        ));
        s.allow_real_jobs = obs.allows_real_jobs();
        s.last_error = None;
        // Optimistic: assume up to date until dispatch says otherwise. Cleared
        // on every connect so a freshly-upgraded node stops showing a stale
        // "update available" once it matches the latest.
        s.update_available = None;
    });

    // Initial-connect retry loop (mirrors spike-worker.ts MAX_CONNECT_ATTEMPTS).
    let mut attempts = 0u32;
    let ws = loop {
        attempts += 1;
        // Shutdown must be honoured while we are still trying to connect. With
        // `max_connect_attempts` retries this loop can run for many seconds, and
        // a node signalled during it (machine shutdown while dispatch is
        // unreachable) would otherwise be killed outright rather than exiting.
        // No job can be in flight yet, so there is nothing to purge — this is
        // about exiting cleanly instead of dying on the signal.
        let connect = tokio::select! {
            biased;
            _ = &mut shutdown => {
                tracing::info!("shutdown signal received before connect — exiting");
                obs.update_status(|s| s.connection = ConnectionState::Disconnected);
                return Ok(());
            }
            result = connect_async(request.clone()) => result,
        };
        match connect {
            Ok((ws, _resp)) => break ws,
            Err(e) if attempts < config.max_connect_attempts => {
                tracing::info!(
                    attempt = attempts,
                    max = config.max_connect_attempts,
                    error = %e,
                    "dispatch not reachable yet — retrying"
                );
                obs.log(LogLine::warn(format!(
                    "Dispatch unreachable (attempt {}/{}), retrying…",
                    attempts, config.max_connect_attempts
                )));
                tokio::select! {
                    biased;
                    _ = &mut shutdown => {
                        tracing::info!("shutdown signal received while retrying — exiting");
                        obs.update_status(|s| s.connection = ConnectionState::Disconnected);
                        return Ok(());
                    }
                    _ = tokio::time::sleep(config.connect_retry_delay) => {}
                }
            }
            Err(e) => {
                let msg = format!(
                    "Failed to connect to dispatch after {} attempts: {e}",
                    config.max_connect_attempts
                );
                obs.update_status(|s| {
                    s.connection = ConnectionState::Disconnected;
                    s.last_error = Some(msg.clone());
                });
                obs.log(LogLine::error(&msg));
                return Err(e).with_context(|| msg);
            }
        }
    };
    tracing::info!(url = %config.dispatch_url, "connected to dispatch");
    obs.update_status(|s| s.connection = ConnectionState::Connected);
    obs.log(LogLine::info("Connected to dispatch"));

    // Startup sweep (ITEM 3): SIGKILL/power-loss killed a previous
    // supervisor before WorkDir::Drop could run, orphaning job workdirs
    // (customer content) under the temp dir. Once we are connected — and
    // before any job can be assigned — remove abandoned ones. The sweep's
    // own safety rule (pid-liveness for supervisor dirs, age gate for
    // runner dirs) is what keeps a live sibling supervisor's workdir safe.
    let swept = crate::sweep::sweep_stale_workdirs();
    if swept > 0 {
        obs.log(LogLine::info(format!(
            "Swept {swept} abandoned job workdir(s) left by a hard-killed supervisor"
        )));
    }

    let (mut sink, mut stream) = ws.split();

    let send = |msg: WorkerMessage| {
        let frame = serde_json::to_string(&msg).expect("worker messages always serialize");
        tracing::info!(frame = %frame, "→ send");
        frame
    };
    let emit = |obs: &Observability, frame: &WorkerMessage| match frame {
        WorkerMessage::JobProgress(p) => {
            obs.update_status(|s| {
                if let Some(job) = &mut s.current_job {
                    job.progress = p.progress;
                }
            });
        }
        WorkerMessage::JobComplete(c) => {
            obs.update_status(|s| {
                s.current_job = None;
                s.jobs_completed += 1;
            });
            obs.log(LogLine::info(format!("Job {} complete", c.job_id)));
        }
        WorkerMessage::JobFailed(f) => {
            obs.update_status(|s| {
                if let Some(job) = &s.current_job {
                    if job.id == f.job_id && job.phase != JobPhase::Canceled {
                        s.jobs_failed += 1;
                    } else if job.id == f.job_id && job.phase == JobPhase::Canceled {
                        s.jobs_canceled += 1;
                    }
                }
                s.current_job = None;
            });
            obs.log(LogLine::warn(format!(
                "Job {} failed: {}",
                f.job_id, f.reason
            )));
        }
        _ => {}
    };

    sink.send(Message::Text(send(WorkerMessage::Register(
        register.clone(),
    ))))
    .await
    .context("failed to send register")?;

    obs.update_status(|s| s.connection = ConnectionState::Registered);
    obs.log(LogLine::info(format!(
        "Registered as {} ({:?})",
        register.chip, register.platform
    )));

    // First heartbeat one full interval after register, then periodic.
    let mut heartbeat = tokio::time::interval_at(
        tokio::time::Instant::now() + config.heartbeat_interval,
        config.heartbeat_interval,
    );
    let mut heartbeats_sent = 0u32;
    let (worker_tx, mut worker_rx) = tokio::sync::mpsc::unbounded_channel::<WorkerMessage>();
    let mut in_flight: Option<InFlightJob> = None;
    // Job id of the last dispatch-initiated cancel whose terminal frame has
    // not been observed yet. Recorded at cancel receipt — BEFORE the runner is
    // killed — so the render abort that follows is never mistaken for a
    // genuine failure. Cleared when that job's terminal frame arrives.
    let mut canceled_job: Option<String> = None;

    // PACKET 5: a dispatch-canceled job whose terminate is still running.
    // The Cancel arm hands the job here instead of dropping its task handle —
    // `run()` awaits it on EVERY exit path so the TERM -> grace -> SIGKILL ->
    // browser-sweep -> purge sequence cannot be abandoned when the socket
    // dies mid-grace (dispatch redeploy / network blip).
    let mut draining: Option<InFlightJob> = None;

    // PACKET 5: teardown completion guarantee. Every exit path must cancel
    // the in-flight job AND await its task to completion before run() returns.
    // The terminate sequence (group TERM -> CANCEL_GRACE -> group KILL ->
    // pidfile sweep -> purge) runs inside the job task; returning while it is
    // mid-grace drops the future at its await point and strands a live render
    // tree. In start mode the process then exits, so a detached spawn cannot
    // save it — the await is the only completion guarantee. Bounded by
    // construction: TERM-grace(10s) + KILL + wait + sweep are all bounded.
    async fn drain_in_flight_jobs(
        in_flight: &mut Option<InFlightJob>,
        draining: &mut Option<InFlightJob>,
    ) {
        if let Some(mut job) = in_flight.take() {
            let _ = job.cancel.take().map(|tx| tx.send(()));
            // The job already had its chance to report; the socket is gone.
            let _ = job.handle.await;
        }
        // A job canceled by dispatch moments before the socket died: its
        // terminate (TERM -> grace -> SIGKILL -> sweep -> purge) is still
        // running inside the job task. Await it to completion — abandoning
        // it here is exactly the stranded-tree defect this packet fixes.
        if let Some(job) = draining.take() {
            let _ = job.handle.await;
        }
    }

    loop {
        let current_job_count = u32::from(in_flight.is_some());
        tokio::select! {
            // Graceful shutdown: cancel in-flight job, close socket, exit.
            _ = &mut shutdown => {
                tracing::info!("shutdown signal received — closing connection");
                obs.log(LogLine::info("Shutting down connection…"));
                if in_flight.is_some() {
                    obs.update_status(|s| {
                        if let Some(j) = &mut s.current_job {
                            j.phase = JobPhase::Canceled;
                        }
                    });
                    obs.log(LogLine::warn(
                        "In-flight job canceled by shutdown — draining before exit".to_string(),
                    ));
                }
                sink.send(Message::Close(None)).await.ok();
                obs.update_status(|s| s.connection = ConnectionState::Disconnected);
                obs.log(LogLine::info("Connection closed"));
                // Close the socket FIRST so dispatch can requeue immediately;
                // then finish killing what we started before returning.
                drain_in_flight_jobs(&mut in_flight, &mut draining).await;
                return Ok(());
            }
            _ = heartbeat.tick() => {
                let msg = WorkerMessage::Heartbeat(HeartbeatMessage {
                    tenant: register.tenant.clone(),
                    current_job_count,
                });
                if let Err(e) = sink.send(Message::Text(send(msg))).await {
                    // PACKET 5: the socket died with a job still in flight —
                    // cancel and drain it BEFORE returning, or the render tree
                    // is stranded live on the operator machine.
                    let _ = e;
                    obs.update_status(|s| s.connection = ConnectionState::Disconnected);
                    drain_in_flight_jobs(&mut in_flight, &mut draining).await;
                    return Err(anyhow::Error::from(e).context("failed to send heartbeat"));
                }
                heartbeats_sent += 1;
                if let Some(limit) = config.heartbeat_limit {
                    if heartbeats_sent >= limit {
                        tracing::info!(heartbeats = heartbeats_sent, "heartbeat limit reached — closing cleanly");
                        obs.log(LogLine::info("Heartbeat limit reached — closing"));
                        sink.send(Message::Close(None)).await.ok();
                        obs.update_status(|s| s.connection = ConnectionState::Disconnected);
                        drain_in_flight_jobs(&mut in_flight, &mut draining).await;
                        return Ok(());
                    }
                }
            }
            Some(msg) = worker_rx.recv() => {
                let terminal_job_id = match &msg {
                    WorkerMessage::JobComplete(c) => Some(c.job_id.clone()),
                    WorkerMessage::JobFailed(f) => Some(f.job_id.clone()),
                    _ => None,
                };
                // A render failure for a job dispatch already canceled is the
                // expected outcome of that cancel (the runner was killed), not
                // a real failure — dispatch has already marked the job
                // canceled and refunded it. Suppress the jobFailed frame; the
                // workdir purge already happened in the runner regardless.
                let suppress_failed = matches!(&msg, WorkerMessage::JobFailed(f)
                    if canceled_job.as_deref() == Some(f.job_id.as_str()));
                if let Some(id) = terminal_job_id.as_deref() {
                    if in_flight.as_ref().map(|j| j.job_id.as_str()) == Some(id) {
                        in_flight = None;
                    }
                    if canceled_job.as_deref() == Some(id) {
                        canceled_job = None;
                    }
                }
                if suppress_failed {
                    if let WorkerMessage::JobFailed(f) = &msg {
                        tracing::info!(
                            job_id = %f.job_id,
                            reason = %f.reason,
                            "render aborted after cancel — suppressing jobFailed"
                        );
                        obs.update_status(|s| {
                            s.jobs_canceled += 1;
                            if s.current_job.as_ref().is_some_and(|j| j.id == f.job_id) {
                                s.current_job = None;
                            }
                        });
                        obs.log(LogLine::info(format!(
                            "Job {} render aborted after cancel — not reporting jobFailed",
                            f.job_id
                        )));
                    }
                } else {
                    emit(obs, &msg);
                    if let Err(e) = sink.send(Message::Text(send(msg))).await {
                        // PACKET 5: drain before returning — see heartbeat arm.
                        obs.update_status(|s| s.connection = ConnectionState::Disconnected);
                        drain_in_flight_jobs(&mut in_flight, &mut draining).await;
                        return Err(anyhow::Error::from(e).context("failed to send worker job frame"));
                    }
                }
            }
            frame = stream.next() => {
                match frame {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<ServerMessage>(&text) {
                            Ok(msg) => {
                                log_server_message(&msg, obs.allows_real_jobs());
                                match msg {
                                    ServerMessage::JobAssign(assign) => {
                                        if !obs.allows_real_jobs() {
                                            tracing::warn!(job_id = %assign.job_id, "refusing jobAssign; allow_real_jobs is OFF");
                                            obs.log(LogLine::warn(format!(
                                                "Job {} assigned but refused — \"Accept real jobs\" is OFF",
                                                assign.job_id
                                            )));
                                            // Tell dispatch, or the job sits assigned until it is
                                            // hard-failed ~10 minutes later.
                                            let _ = worker_tx.send(WorkerMessage::JobRejected(JobRejectedMessage {
                                                tenant: assign.tenant.clone(),
                                                job_id: assign.job_id.clone(),
                                                attempt: assign.attempt,
                                                reason: RejectReason::NotAccepting,
                                            }));
                                            continue;
                                        }
                                        if in_flight.is_some() {
                                            tracing::warn!(job_id = %assign.job_id, "refusing jobAssign while busy");
                                            let _ = worker_tx.send(WorkerMessage::JobRejected(JobRejectedMessage {
                                                tenant: assign.tenant.clone(),
                                                job_id: assign.job_id.clone(),
                                                attempt: assign.attempt,
                                                reason: RejectReason::Busy,
                                            }));
                                            continue;
                                        }
                                        let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
                                        let mut pending_cancel = Some(cancel_tx);
                                        let job_id_owned = assign.job_id.clone();
                                        let tier = format!("{:?}", assign.kind).to_lowercase();
                                        obs.update_status(|s| {
                                            s.current_job = Some(JobStatus {
                                                id: assign.job_id.clone(),
                                                tier,
                                                progress: 0.0,
                                                phase: JobPhase::Downloading,
                                            });
                                        });
                                        obs.log(LogLine::info(format!("Job {} assigned — accepting", assign.job_id)));
                                        sink.send(Message::Text(send(WorkerMessage::JobAccepted(JobAcceptedMessage {
                                            tenant: assign.tenant.clone(),
                                            job_id: assign.job_id.clone(),
                                            attempt: assign.attempt,
                                        })))).await.context("failed to send jobAccepted")?;
                                        // Transition to rendering phase once the runner starts.
                                        obs.update_status(|s| {
                                            if let Some(job) = &mut s.current_job {
                                                job.phase = JobPhase::Rendering;
                                            }
                                        });
                                        let handle = tokio::spawn(run_job(*assign, cancel_rx, worker_tx.clone()));
                                        // PACKET 5: keep the JoinHandle — every exit path awaits it.
                                        in_flight = Some(InFlightJob {
                                            job_id: job_id_owned,
                                            cancel: pending_cancel.take(),
                                            handle,
                                        });
                                    }
                                    ServerMessage::Cancel(cancel)
                                        if in_flight.as_ref().map(|j| j.job_id.as_str())
                                            == Some(cancel.job_id.as_str()) =>
                                    {
                                        if let Some(mut job) = in_flight.take() {
                                            // Mark the job canceled BEFORE killing the
                                            // render, so the abort that follows can
                                            // never race past the marker and surface
                                            // as a genuine jobFailed.
                                            canceled_job = Some(job.job_id.clone());
                                            obs.update_status(|s| {
                                                if let Some(j) = &mut s.current_job {
                                                    j.phase = JobPhase::Canceled;
                                                }
                                            });
                                            let _ = job.cancel.take().map(|tx| tx.send(()));
                                            obs.log(LogLine::warn(format!("Job {} canceled by dispatch", cancel.job_id)));
                                            // PACKET 5: keep ownership of the task. The
                                            // terminate sequence runs INSIDE this task;
                                            // dropping the handle here would orphan it
                                            // mid-grace if the socket dies next.
                                            draining = Some(job);
                                        }
                                    }
                                    ServerMessage::Cancel(_) => {}
                                    ServerMessage::UpdateAvailable(u) => {
                                        obs.update_status(|s| {
                                            s.update_available =
                                                Some(u.supervisor_version.clone());
                                        });
                                    }
                                    _ => {}
                                }
                            }
                            Err(e) => tracing::warn!(error = %e, frame = %text, "unparseable frame from server"),
                        }
                    }
                    Some(Ok(Message::Close(close))) => {
                        // W3.5: if dispatch closed with an "upgrade-required"
                        // reason, surface it prominently and exit cleanly
                        // (no silent retry/hang). Otherwise log + disconnect.
                        if let Some(frame) = &close {
                            let reason: &str = frame.reason.as_ref();
                            if reason.starts_with("upgrade-required") {
                                tracing::warn!(%reason, "dispatch rejected connection (upgrade required)");
                                eprintln!("\nUPGRADE REQUIRED — {reason}");
                                eprintln!("Upgrade decent and restart.\n");
                                obs.update_status(|s| s.connection = ConnectionState::Disconnected);
                                // PACKET 5 FOLLOW-UP: this branch is an eighth
                                // exit path and it held a live job too. It is
                                // reached exactly when dispatch redeploys with
                                // a raised minimum protocol version and closes
                                // live connections — i.e. while nodes are
                                // mid-render. Worse than the generic close: it
                                // exits deliberately without retry, so in
                                // `start` mode the process dies and takes the
                                // runtime with it, leaving a wedged runner and
                                // a daemonized Chrome with nothing left to
                                // escalate. Drain before giving up.
                                drain_in_flight_jobs(&mut in_flight, &mut draining).await;
                                return Ok(());
                            }
                            tracing::info!(%reason, "socket closed by server");
                        } else {
                            tracing::info!("socket closed by server");
                        }
                        obs.log(LogLine::info("Socket closed by server"));
                        obs.update_status(|s| s.connection = ConnectionState::Disconnected);
                        // PACKET 5: the tree we started still needs killing
                        // before this task returns — mid-grace drop strands it.
                        drain_in_flight_jobs(&mut in_flight, &mut draining).await;
                        return Ok(());
                    }
                    // tungstenite answers Ping frames automatically.
                    Some(Ok(_)) => {}
                    Some(Err(e)) => {
                        let msg = format!("WebSocket error: {e}");
                        obs.update_status(|s| {
                            s.connection = ConnectionState::Disconnected;
                            s.last_error = Some(msg.clone());
                        });
                        obs.log(LogLine::error(&msg));
                        drain_in_flight_jobs(&mut in_flight, &mut draining).await;
                        return Err(anyhow!(e).context("websocket error"));
                    }
                    None => {
                        tracing::info!("socket stream ended");
                        obs.log(LogLine::info("Socket stream ended"));
                        obs.update_status(|s| s.connection = ConnectionState::Disconnected);
                        drain_in_flight_jobs(&mut in_flight, &mut draining).await;
                        return Ok(());
                    }
                }
            }
        }
    }
}

fn log_server_message(msg: &ServerMessage, allow_real_jobs: bool) {
    match msg {
        ServerMessage::JobAssign(assign) => {
            if allow_real_jobs {
                tracing::info!(
                    job_id = %assign.job_id,
                    kind = ?assign.kind,
                    frames = assign.duration_frames,
                    "← jobAssign — accepting real job"
                );
            } else {
                tracing::warn!(
                    job_id = %assign.job_id,
                    kind = ?assign.kind,
                    frames = assign.duration_frames,
                    "← jobAssign — real jobs disabled; NOT accepting"
                );
            }
        }
        ServerMessage::Ping(_) => tracing::debug!("← ping"),
        ServerMessage::Cancel(c) => tracing::info!(job_id = %c.job_id, "← cancel"),
        ServerMessage::UpdateAvailable(u) => tracing::info!(
            supervisor = %u.supervisor_version,
            payload = %u.payload_version,
            "← updateAvailable"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Capabilities, Platform, PROTOCOL_VERSION};
    use tokio::net::{TcpListener, TcpStream};
    use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
    use tokio_tungstenite::WebSocketStream;

    /// Create a shutdown receiver that never fires (tests use heartbeat_limit
    /// or server close for clean exit instead). mem::forget the sender so the
    /// oneshot never resolves to Err(Closed).
    fn never_shutdown() -> oneshot::Receiver<()> {
        let (tx, rx) = oneshot::channel();
        std::mem::forget(tx);
        rx
    }

    /// Config with no heartbeat limit — runs until shutdown or server close.
    fn long_config(port: u16) -> ConnectionConfig {
        ConnectionConfig {
            heartbeat_interval: Duration::from_millis(50),
            max_connect_attempts: 20,
            connect_retry_delay: Duration::from_millis(50),
            heartbeat_limit: None,
            ..ConnectionConfig::new(format!("ws://127.0.0.1:{port}/ws"), "test-jwt.token")
        }
    }

    fn test_register() -> RegisterMessage {
        RegisterMessage {
            tenant: "driffs".into(),
            protocol_version: PROTOCOL_VERSION,
            operator: None,
            platform: Platform::Company,
            chip: "test-chip".into(),
            ram_gb: 8,
            supervisor_version: "rust-0.0.1".into(),
            payload_version: "none".into(),
            capabilities: Capabilities {
                gpu: false,
                max_concurrent_jobs: None,
                os: None,
                arch: None,
            },
        }
    }

    fn fast_config(port: u16) -> ConnectionConfig {
        ConnectionConfig {
            heartbeat_interval: Duration::from_millis(50),
            max_connect_attempts: 20,
            connect_retry_delay: Duration::from_millis(50),
            heartbeat_limit: Some(2),
            ..ConnectionConfig::new(format!("ws://127.0.0.1:{port}/ws"), "test-jwt.token")
        }
    }

    /// What the server saw during the handshake.
    #[derive(Default, Clone)]
    struct Handshake {
        uri: String,
        authorization: Option<String>,
    }

    async fn accept_ws_capturing(
        listener: &TcpListener,
    ) -> (WebSocketStream<TcpStream>, Handshake) {
        let (tcp, _) = listener.accept().await.unwrap();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Handshake::default()));
        let seen_clone = seen.clone();
        // tungstenite's handshake callback signature carries a large Err type.
        #[allow(clippy::result_large_err)]
        let callback = move |req: &Request, resp: Response| {
            let mut slot = seen_clone.lock().unwrap();
            slot.uri = req.uri().to_string();
            slot.authorization = req
                .headers()
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .map(|v| v.to_string());
            Ok(resp)
        };
        let ws = tokio_tungstenite::accept_hdr_async(tcp, callback)
            .await
            .unwrap();
        let seen = seen.lock().unwrap().clone();
        (ws, seen)
    }

    async fn accept_ws(listener: &TcpListener) -> (WebSocketStream<TcpStream>, String) {
        let (ws, handshake) = accept_ws_capturing(listener).await;
        (ws, handshake.uri)
    }

    async fn next_text(ws: &mut WebSocketStream<TcpStream>) -> String {
        loop {
            match ws.next().await.expect("stream ended").expect("ws error") {
                Message::Text(t) => return t,
                _ => continue,
            }
        }
    }

    /// Seed a fake cached render payload so `ensure_payload` short-circuits
    /// (no download). The "runner" is a shell script standing in for
    /// `decent-render-runner`. Returns the payload dir for cleanup.
    #[cfg(unix)]
    /// Per-process worker state root for tests, redirected away from the real
    /// `~/.decent-worker`.
    ///
    /// That directory is LIVE OPERATOR STATE — it holds the sha-named payloads
    /// and browsers a real node renders with. Seeding fixtures into it meant
    /// every cleanup was an `rm -rf` glob aimed at the real cache, and it had
    /// accumulated ~55 stray `test-*` payload dirs against 9 real ones by the
    /// time this was fixed. First call wins and installs the redirect, so no
    /// test needs to run first.
    #[cfg(unix)]
    fn test_worker_root() -> std::path::PathBuf {
        static ROOT: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
        ROOT.get_or_init(|| {
            let root =
                std::env::temp_dir().join(format!("decent-test-worker-{}", std::process::id()));
            std::fs::create_dir_all(&root).expect("test worker root");
            crate::runner::set_worker_root_for_tests(root.clone());
            root
        })
        .clone()
    }

    /// The tests must never write into the real `~/.decent-worker`.
    ///
    /// Guards the seam directly rather than trusting it: if the redirect is
    /// ever dropped, seeding silently starts writing sha-named fixtures into
    /// live operator state again and every cleanup becomes an `rm -rf` glob
    /// pointed at a real node's payload cache.
    ///
    /// Note this is not the only proof — every containment test seeds a
    /// payload and then relies on the SUPERVISOR's own `ensure_artifact` to
    /// find it. If production and test disagreed about the root, those tests
    /// would try to download from a dummy URL and fail. This test just makes
    /// the invariant explicit and cheap to diagnose.
    #[cfg(unix)]
    #[test]
    fn tests_never_seed_into_live_operator_state() {
        let real = std::path::PathBuf::from(std::env::var("HOME").expect("HOME set in tests"))
            .join(".decent-worker");
        let seeded = seed_fake_payload("test-root-isolation-probe", "#!/bin/sh\nexit 0\n");

        assert!(
            seeded.starts_with(test_worker_root()),
            "payload was seeded outside the test root: {}",
            seeded.display()
        );
        assert!(
            !seeded.starts_with(&real),
            "payload was seeded into LIVE operator state at {}",
            seeded.display()
        );

        std::fs::remove_dir_all(&seeded).ok();
    }

    #[cfg(unix)]
    fn seed_fake_payload(sha: &str, script: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let dir = test_worker_root().join("payloads").join(sha);
        std::fs::create_dir_all(&dir).unwrap();
        let runner = dir.join("decent-render-runner");
        std::fs::write(&runner, script).unwrap();
        std::fs::set_permissions(&runner, std::fs::Permissions::from_mode(0o755)).unwrap();
        dir
    }

    #[cfg(unix)]
    fn job_assign_json(job_id: &str, payload_sha: &str) -> String {
        format!(
            r#"{{"type":"jobAssign","tenant":"driffs","jobId":"{job_id}","kind":"standard","durationFrames":1,"fps":30,"codec":"h264","bundleSha256":"s","bundleGetUrl":"u","payloadSha256":"{payload_sha}","payloadGetUrl":"u","inputPropsGetUrl":"u","assetGetUrls":[],"outputPutUrl":"u","outputKey":"k","purgeAfter":true}}"#
        )
    }

    /// Workdirs (see `WorkDir::new`) still on disk for the given job id.
    #[cfg(unix)]
    fn job_workdirs(job_id: &str) -> Vec<std::path::PathBuf> {
        let prefix = format!("job-{job_id}-");
        std::fs::read_dir(std::env::temp_dir())
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(&prefix))
            })
            .collect()
    }

    /// (a) Cancel, then the render aborts: the abort is the expected cancel
    /// outcome — NO jobFailed frame may reach dispatch (which has already
    /// marked the job canceled + refunded), and the workdir must be purged.
    #[cfg(unix)]
    #[tokio::test]
    async fn cancel_then_render_abort_suppresses_job_failed_and_purges() {
        let pid = std::process::id();
        let sha = format!("test-cancel-suppress-{pid}");
        let job_id = format!("job-cancel-suppress-{pid}");
        // Runner that renders "forever" — only a cancel ends it.
        let payload_dir = seed_fake_payload(&sha, "#!/bin/sh\nsleep 30\n");

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let config = long_config(port);
        let register = test_register();
        let (obs, mut status_rx, _log_rx) =
            Observability::channels(crate::status::SupervisorStatus::default());
        obs.set_allow_real_jobs(true);

        let obs2 = obs.clone();
        let client =
            tokio::spawn(async move { run(&config, &register, &obs2, never_shutdown()).await });

        let (mut ws, _uri) = accept_ws(&listener).await;
        let _register = next_text(&mut ws).await;

        ws.send(Message::Text(job_assign_json(&job_id, &sha)))
            .await
            .unwrap();

        // Collect frames until the job is accepted (heartbeats may interleave).
        let mut frames: Vec<serde_json::Value> = Vec::new();
        loop {
            let t = tokio::time::timeout(Duration::from_secs(5), next_text(&mut ws))
                .await
                .expect("expected jobAccepted before timeout");
            let v: serde_json::Value = serde_json::from_str(&t).unwrap();
            let accepted = v["type"] == "jobAccepted";
            frames.push(v);
            if accepted {
                break;
            }
        }

        // Dispatch cancels the in-flight job.
        ws.send(Message::Text(format!(
            r#"{{"type":"cancel","tenant":"driffs","jobId":"{job_id}"}}"#
        )))
        .await
        .unwrap();

        // The suppressed terminal bumps jobs_canceled and clears current_job —
        // that is the deterministic "canceled render fully processed" signal.
        tokio::time::timeout(
            Duration::from_secs(5),
            status_rx.wait_for(|s| s.jobs_canceled == 1 && s.current_job.is_none()),
        )
        .await
        .expect("canceled render was not processed in time")
        .expect("status channel closed");

        // Purge still happened (WorkDir dropped in the runner cancel path).
        assert!(
            job_workdirs(&job_id).is_empty(),
            "workdir must be purged after cancel"
        );

        // Close and drain everything the client ever sent.
        ws.close(None).await.ok();
        while let Some(Ok(frame)) = ws.next().await {
            if let Message::Text(t) = frame {
                frames.push(serde_json::from_str(&t).unwrap());
            }
        }
        client.await.unwrap().expect("clean exit");

        assert!(
            frames.iter().all(|v| v["type"] != "jobFailed"),
            "jobFailed must be suppressed after cancel, got {frames:?}"
        );
        std::fs::remove_dir_all(&payload_dir).ok();
    }

    /// (b) A genuine render failure with no cancel in play must still emit
    /// jobFailed exactly as before (and purge the workdir).
    #[cfg(unix)]
    #[tokio::test]
    async fn genuine_failure_without_cancel_emits_job_failed() {
        let pid = std::process::id();
        let sha = format!("test-genuine-failure-{pid}");
        let job_id = format!("job-genuine-failure-{pid}");
        // Deliberately never reads stdin, and exits at once. That races the
        // supervisor's write of the jobAssign frame and loses with EPIPE on
        // Linux (macOS buffers it, which is why this only failed in CI). The
        // runner's own message must still win over the write error — otherwise
        // the operator sees "Broken pipe" instead of what actually went wrong.
        let payload_dir = seed_fake_payload(
            &sha,
            "#!/bin/sh\necho '{\"type\":\"error\",\"message\":\"render exploded\"}'\nexit 1\n",
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let config = long_config(port);
        let register = test_register();
        let (obs, _status_rx, _log_rx) =
            Observability::channels(crate::status::SupervisorStatus::default());
        obs.set_allow_real_jobs(true);

        let obs2 = obs.clone();
        let client =
            tokio::spawn(async move { run(&config, &register, &obs2, never_shutdown()).await });

        let (mut ws, _uri) = accept_ws(&listener).await;
        let _register = next_text(&mut ws).await;

        ws.send(Message::Text(job_assign_json(&job_id, &sha)))
            .await
            .unwrap();

        // Read until jobFailed shows up (heartbeats/jobAccepted interleave).
        let failed = loop {
            let t = tokio::time::timeout(Duration::from_secs(5), next_text(&mut ws))
                .await
                .expect("expected jobFailed before timeout");
            let v: serde_json::Value = serde_json::from_str(&t).unwrap();
            if v["type"] == "jobFailed" {
                break v;
            }
        };
        assert_eq!(failed["jobId"], job_id.as_str());
        assert_eq!(failed["reason"], "render exploded");

        // Workdir purged before the failure was reported.
        assert!(
            job_workdirs(&job_id).is_empty(),
            "workdir must be purged after a genuine failure"
        );

        ws.close(None).await.ok();
        while ws.next().await.is_some() {}
        client.await.unwrap().expect("clean exit");
        std::fs::remove_dir_all(&payload_dir).ok();
    }

    /// Shutdown during the connect-retry loop must exit cleanly rather than
    /// leaving the process to be killed by the signal. No job is in flight yet,
    /// so nothing is purged — this is purely about a clean exit.
    #[tokio::test]
    async fn shutdown_during_connect_retries_exits_cleanly() {
        // Port with nothing listening: run() stays in the retry loop.
        let dead = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = dead.local_addr().unwrap().port();
        drop(dead);

        let mut config = fast_config(port);
        config.max_connect_attempts = 1000;
        config.connect_retry_delay = Duration::from_millis(20);
        let register = test_register();
        let obs = Observability::default();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();

        let client = tokio::spawn(async move { run(&config, &register, &obs, shutdown_rx).await });

        tokio::time::sleep(Duration::from_millis(60)).await;
        shutdown_tx.send(()).unwrap();

        let result = tokio::time::timeout(Duration::from_secs(5), client)
            .await
            .expect("must not hang after shutdown");
        result.unwrap().expect("clean exit, not a connect error");
    }

    #[tokio::test]
    async fn registers_heartbeats_and_closes_cleanly() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let config = fast_config(port);
        let register = test_register();
        let obs = Observability::default();

        let client =
            tokio::spawn(async move { run(&config, &register, &obs, never_shutdown()).await });

        let (mut ws, handshake) = accept_ws_capturing(&listener).await;
        // The credential rides a header, never the URL: a query-string token is
        // written verbatim into every proxy and platform access log en route.
        assert_eq!(
            handshake.authorization.as_deref(),
            Some("Bearer test-jwt.token"),
            "worker token must arrive in the Authorization header"
        );
        assert!(
            !handshake.uri.contains("test-jwt.token"),
            "token leaked into the URL: {}",
            handshake.uri
        );

        let first: serde_json::Value = serde_json::from_str(&next_text(&mut ws).await).unwrap();
        assert_eq!(first["type"], "register");
        assert_eq!(first["protocolVersion"], 2);
        assert_eq!(first["platform"], "company");
        assert_eq!(first["capabilities"]["gpu"], false);
        assert_eq!(first["operator"], serde_json::Value::Null);

        for _ in 0..2 {
            let hb: serde_json::Value = serde_json::from_str(&next_text(&mut ws).await).unwrap();
            assert_eq!(hb["type"], "heartbeat");
            assert_eq!(hb["currentJobCount"], 0);
            assert_eq!(hb["tenant"], "driffs");
        }

        // heartbeat_limit = 2 → client closes.
        client.await.unwrap().expect("clean exit");
    }

    #[tokio::test]
    async fn delivers_server_messages_and_does_not_accept_jobs() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let config = fast_config(port);
        let register = test_register();
        let obs = Observability::default();

        let client =
            tokio::spawn(async move { run(&config, &register, &obs, never_shutdown()).await });

        let (mut ws, _uri) = accept_ws(&listener).await;
        let _register = next_text(&mut ws).await;

        ws.send(Message::Text(r#"{"type":"ping","tenant":"driffs"}"#.into()))
            .await
            .unwrap();
        ws.send(Message::Text(
			r#"{"type":"jobAssign","tenant":"driffs","jobId":"job-render-x","kind":"gpu","durationFrames":10,"fps":30,"codec":"h264","bundleSha256":"s","bundleGetUrl":"u","payloadSha256":"p","payloadGetUrl":"u","inputPropsGetUrl":"u","assetGetUrls":[],"outputPutUrl":"u","outputKey":"k","purgeAfter":true}"#.into(),
		))
		.await
		.unwrap();

        // Drain until the client closes; collect everything it sent meanwhile.
        let mut sent_types = Vec::new();
        while let Some(Ok(frame)) = ws.next().await {
            if let Message::Text(t) = frame {
                let v: serde_json::Value = serde_json::from_str(&t).unwrap();
                sent_types.push(v["type"].as_str().unwrap().to_string());
            }
        }
        client.await.unwrap().expect("clean exit");

        // The node must NOT accept the job when allow_real_jobs is off…
        assert!(
            !sent_types.iter().any(|t| t == "jobAccepted"),
            "must not accept a job with allow_real_jobs off, got {sent_types:?}"
        );
        // …but it MUST say so. Staying silent (the behaviour this test used to
        // assert) left the job assigned until dispatch hard-failed it after
        // max(10min, expected × 20).
        assert_eq!(
            sent_types.iter().filter(|t| *t == "jobRejected").count(),
            1,
            "exactly one jobRejected expected, got {sent_types:?}"
        );
        assert!(
            sent_types
                .iter()
                .all(|t| t == "heartbeat" || t == "jobRejected"),
            "only heartbeats and the rejection expected, got {sent_types:?}"
        );
    }

    /// The rejection has to carry the reason and the assignment's attempt, or
    /// dispatch cannot fence it against a newer assignment of the same job.
    #[tokio::test]
    async fn rejects_with_reason_and_attempt_when_not_accepting() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let config = fast_config(port);
        let register = test_register();
        let obs = Observability::default();

        let client =
            tokio::spawn(async move { run(&config, &register, &obs, never_shutdown()).await });

        let (mut ws, _uri) = accept_ws(&listener).await;
        let _register = next_text(&mut ws).await;
        ws.send(Message::Text(
			r#"{"type":"jobAssign","tenant":"driffs","jobId":"job-render-x","attempt":3,"kind":"gpu","durationFrames":10,"fps":30,"codec":"h264","bundleSha256":"s","bundleGetUrl":"u","payloadSha256":"p","payloadGetUrl":"u","inputPropsGetUrl":"u","assetGetUrls":[],"outputPutUrl":"u","outputKey":"k","purgeAfter":true}"#.into(),
		))
		.await
		.unwrap();

        let mut rejection = None;
        while let Some(Ok(frame)) = ws.next().await {
            if let Message::Text(t) = frame {
                let v: serde_json::Value = serde_json::from_str(&t).unwrap();
                if v["type"] == "jobRejected" {
                    rejection = Some(v);
                }
            }
        }
        client.await.unwrap().expect("clean exit");

        let rejection = rejection.expect("a jobRejected frame");
        assert_eq!(rejection["jobId"], "job-render-x");
        assert_eq!(rejection["attempt"], 3);
        assert_eq!(rejection["reason"], "not-accepting");
        assert_eq!(rejection["tenant"], "driffs");
    }

    #[tokio::test]
    async fn retries_initial_connect_until_dispatch_is_up() {
        // Reserve a port, then release it so the first connect attempts fail.
        let probe = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);

        let config = fast_config(port);
        let register = test_register();
        let obs = Observability::default();
        let client =
            tokio::spawn(async move { run(&config, &register, &obs, never_shutdown()).await });

        // Let a few attempts fail before the "dispatch" comes up.
        tokio::time::sleep(Duration::from_millis(150)).await;
        let listener = TcpListener::bind(("127.0.0.1", port)).await.unwrap();
        let (mut ws, _uri) = accept_ws(&listener).await;

        let first: serde_json::Value = serde_json::from_str(&next_text(&mut ws).await).unwrap();
        assert_eq!(first["type"], "register");
        while ws.next().await.is_some() {} // drain to close
        client
            .await
            .unwrap()
            .expect("clean exit after retrying connect");
    }

    #[tokio::test]
    async fn obs_tracks_connection_state_transitions() {
        // Verify the status channel reflects state transitions.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut config = fast_config(port);
        // This test controls the socket lifetime explicitly. A heartbeat limit
        // races the Registered assertion against the second heartbeat timeout.
        config.heartbeat_limit = None;
        let register = test_register();

        let (obs, mut status_rx, _log_rx) =
            Observability::channels(crate::status::SupervisorStatus::default());

        let client =
            tokio::spawn(async move { run(&config, &register, &obs, never_shutdown()).await });

        let (mut ws, _uri) = accept_ws(&listener).await;
        let _register = next_text(&mut ws).await;

        // After connect + register, wait for the status event rather than a
        // wall-clock delay that can also let the connection close.
        tokio::time::timeout(
            Duration::from_secs(1),
            status_rx.wait_for(|status| status.connection == ConnectionState::Registered),
        )
        .await
        .expect("status did not reach Registered")
        .expect("status channel closed before Registered");
        assert_eq!(status_rx.borrow().connection, ConnectionState::Registered);
        assert!(status_rx.borrow().node_identity.is_some());

        // The server closes the connection; the client must publish the final
        // Disconnected state before returning.
        tokio::time::timeout(Duration::from_secs(1), ws.close(None))
            .await
            .expect("server close handshake timed out")
            .expect("server close failed");
        tokio::time::timeout(Duration::from_secs(1), client)
            .await
            .expect("client did not exit after server close")
            .expect("client task panicked")
            .expect("clean exit");

        assert_eq!(status_rx.borrow().connection, ConnectionState::Disconnected);
    }

    #[tokio::test]
    async fn obs_receives_job_assign_warning_when_jobs_disabled() {
        // With allow_real_jobs=false, a jobAssign must produce a warning log
        // but no jobAccepted frame.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let config = fast_config(port);
        let register = test_register();

        let (obs, status_rx, mut log_rx) =
            Observability::channels(crate::status::SupervisorStatus::default());

        let client =
            tokio::spawn(async move { run(&config, &register, &obs, never_shutdown()).await });

        let (mut ws, _uri) = accept_ws(&listener).await;
        let _register = next_text(&mut ws).await;

        ws.send(Message::Text(
			r#"{"type":"jobAssign","tenant":"driffs","jobId":"job-refused-1","kind":"standard","durationFrames":1,"fps":30,"codec":"h264","bundleSha256":"s","bundleGetUrl":"u","payloadSha256":"p","payloadGetUrl":"u","inputPropsGetUrl":"u","assetGetUrls":[],"outputPutUrl":"u","outputKey":"k","purgeAfter":true}"#.into(),
		))
		.await
		.unwrap();

        tokio::time::sleep(Duration::from_millis(100)).await;

        // Status: no current job (refused).
        assert!(status_rx.borrow().current_job.is_none());

        // Log: should have the refusal warning.
        let mut found_refusal = false;
        while let Ok(line) = log_rx.try_recv() {
            if line.message.contains("refused") {
                found_refusal = true;
            }
        }
        assert!(found_refusal, "expected a job refusal warning log line");

        // Drain to close.
        while ws.next().await.is_some() {}
        client.await.unwrap().expect("clean exit");
    }

    #[tokio::test]
    async fn runtime_toggle_accepts_jobs() {
        // Start with allow_real_jobs=false, then flip the atomic at runtime.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let config = fast_config(port);
        let register = test_register();

        let (obs, _rx, _lr) = Observability::channels(crate::status::SupervisorStatus::default());
        // Start with jobs refused.
        assert!(!obs.allows_real_jobs());

        let obs2 = obs.clone();
        let client =
            tokio::spawn(async move { run(&config, &register, &obs2, never_shutdown()).await });

        let (mut ws, _uri) = accept_ws(&listener).await;
        let _register = next_text(&mut ws).await;

        // Send a jobAssign while jobs are refused.
        ws.send(Message::Text(
			r#"{"type":"jobAssign","tenant":"driffs","jobId":"job-1","kind":"standard","durationFrames":1,"fps":30,"codec":"h264","bundleSha256":"s","bundleGetUrl":"u","payloadSha256":"p","payloadGetUrl":"u","inputPropsGetUrl":"u","assetGetUrls":[],"outputPutUrl":"u","outputKey":"k","purgeAfter":true}"#.into(),
		))
		.await
		.unwrap();

        tokio::time::sleep(Duration::from_millis(50)).await;

        // No jobAccepted should be sent (only heartbeats).
        // Now flip allow_real_jobs on.
        obs.set_allow_real_jobs(true);
        assert!(obs.allows_real_jobs());

        // Drain to close — no jobAccepted expected since we can't run a real
        // runner in this test, but the toggle itself is proven.
        while let Some(Ok(frame)) = ws.next().await {
            if let Message::Text(t) = frame {
                let v: serde_json::Value = serde_json::from_str(&t).unwrap();
                assert_ne!(
                    v["type"], "jobAccepted",
                    "first job must not be accepted while allow was off"
                );
            }
        }
        client.await.unwrap().expect("clean exit");
    }

    #[tokio::test]
    async fn shutdown_signal_closes_socket_gracefully() {
        // Fire the shutdown signal and verify: socket closes cleanly,
        // status goes to Disconnected, no panic.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let config = long_config(port);
        let register = test_register();

        let (obs, status_rx, _log_rx) =
            Observability::channels(crate::status::SupervisorStatus::default());

        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

        let obs2 = obs.clone();
        let client = tokio::spawn(async move { run(&config, &register, &obs2, shutdown_rx).await });

        let (mut ws, _uri) = accept_ws(&listener).await;
        let _register = next_text(&mut ws).await;

        // Wait until registered.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(status_rx.borrow().connection, ConnectionState::Registered);

        // Fire shutdown.
        let _ = shutdown_tx.send(());
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Status should be Disconnected.
        assert_eq!(status_rx.borrow().connection, ConnectionState::Disconnected);

        // Connection task should exit cleanly.
        client.await.unwrap().expect("clean exit on shutdown");
    }

    /// The token travels in a header, and NOT in the URL. A query-string
    /// credential is copied verbatim into every proxy, CDN and platform access
    /// log on the path, so this asserts both halves: the header is present and
    /// the URL is clean.
    #[test]
    fn sends_the_token_as_a_header_not_in_the_url() {
        let config = ConnectionConfig::new("ws://localhost:8790/ws", "tok+en/with=specials");
        let request = config.handshake_request().expect("build handshake request");

        assert_eq!(
            request.headers().get("authorization").unwrap(),
            "Bearer tok+en/with=specials"
        );
        let uri = request.uri().to_string();
        assert!(
            !uri.contains("token"),
            "token must not appear in the URL: {uri}"
        );
        assert!(
            !uri.contains("specials"),
            "token value leaked into the URL: {uri}"
        );
    }

    #[test]
    fn handshake_request_preserves_an_existing_query_string() {
        let config = ConnectionConfig::new("ws://localhost:8790/ws?region=eu", "t");
        let request = config.handshake_request().unwrap();
        assert!(request.uri().to_string().contains("region=eu"));
    }

    /// Wait until the fake runner's pid side-file appears, then read it.
    /// Used by the containment tests (the pid is out-of-band, never a frame).
    #[cfg(unix)]
    async fn wait_for_pid_file(pid_file: &std::path::Path) -> u32 {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(text) = std::fs::read_to_string(pid_file) {
                if let Ok(pid) = text.trim().parse::<u32>() {
                    return pid;
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "runner never wrote its grandchild pid file"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Shared helper for the containment tests: a fake runner shell script
    /// that spawns a never-exiting grandchild (the Chrome stand-in) and
    /// reports its pid on stdout as a progress frame BEFORE anything else,
    /// then idles. `trap_term` controls whether the runner and grandchild
    /// ignore SIGTERM (the adversarial case only the group-KILL stage can
    /// settle).
    #[cfg(unix)]
    #[allow(clippy::too_many_arguments)]
    fn group_kill_payload(
        trap_term: bool,
        then: &str,
        pid_file: &str,
        grandchild: &str,
        started_marker: &str,
    ) -> String {
        let trap = if trap_term { "trap '' TERM INT" } else { "" };
        format!(
            r#"#!/bin/sh
{trap_runner}
"{grandchild}" >/dev/null 2>&1 &
GC=$!
echo "$GC" > "{pid_file}"
# Let the grandchild finish exec (install its traps) BEFORE the error event
# fires — the supervisor terminates the tree the moment it reads the error,
# and a TERM during the grandchild's exec window destroys the evidence.
# Wait for the grandchild's OWN .started marker instead of a flat sleep:
# under full-suite parallel load a flat sleep races the grandchild's exec
# (pre-existing flake, reproduced at ae671f9 baseline 2026-08-21).
i=0
while [ ! -f "{started_marker}" ] && [ $i -lt 3000 ]; do
  sleep 0.1 2>/dev/null || sleep 1
  i=$((i+1))
done
echo "{{\"type\":\"progress\",\"progress\":0.5}}"
{then}
while true; do sleep 5; done
"#,
            trap_runner = trap,
            grandchild = grandchild,
            then = then,
            pid_file = pid_file,
            started_marker = started_marker,
        )
    }

    /// The Chrome stand-in: a STATIC script written by the test (not by a
    /// heredoc inside the runner — that quoting maze caused two vacuous-test
    /// bugs: dollar-expansion at write time made the grandchild die instantly
    /// and read as a reaped zombie). This variant RECORDS that TERM reached
    /// it, then exits 143 so the tree can settle.
    #[cfg(unix)]
    fn grandchild_script_evidence(marker: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::temp_dir().join(format!(
            "decent-gc-ev-{}-{}.sh",
            std::process::id(),
            marker.rsplit('/').next().unwrap_or("m")
        ));
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\ntrap 'touch {marker}.term; exit 143' TERM\ntouch {marker}.started\n# Short sleeps: dash runs traps only BETWEEN commands, so a long sleep\n# would defer the TERM evidence past the test deadline.\ni=0\nwhile [ $i -lt 600 ]; do sleep 0.1 2>/dev/null || sleep 1; i=$((i+1)); done\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// The adversarial Chrome stand-in: ignores TERM and INT entirely, so
    /// only the group SIGKILL escalation can stop it. Dies on its own after
    /// 120s so a broken test cannot leak it forever.
    #[cfg(unix)]
    fn grandchild_script_immune() -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let started =
            std::env::temp_dir().join(format!("decent-gc-im-{}.started", std::process::id()));
        let path = std::env::temp_dir().join(format!("decent-gc-im-{}.sh", std::process::id()));
        let _ = std::fs::remove_file(&started);
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\ntrap '' TERM INT\ntouch {}\nsleep 60\nsleep 60\n",
                started.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// ITEM 1: a cancel must kill the runner's whole process GROUP. The
    /// grandchild is Chrome-like (records TERM, then exits 143). Evidence is
    /// BOTH the marker file AND the grandchild's death: deadness alone cannot
    /// discriminate a group TERM from an orphaned-zombie artifact, and a
    /// marker alone cannot prove the process actually stopped.
    #[cfg(unix)]
    #[tokio::test]
    async fn cancel_kills_the_runners_whole_process_group() {
        let pid = std::process::id();
        let sha = format!("test-group-kill-{pid}");
        let job_id = format!("job-group-kill-{pid}");
        let marker = std::env::temp_dir().join(format!("decent-gc-kill-{pid}"));
        let mark_term = marker.with_extension("term");
        let _ = std::fs::remove_file(&mark_term);
        // Evidence traps: the grandchild records that TERM reached IT (not
        // just the runner), then exits so the tree can settle.
        let gc_script = grandchild_script_evidence(&marker.to_string_lossy());
        let pid_file = std::env::temp_dir().join(format!("{job_id}.gc-pid"));
        let _ = std::fs::remove_file(&pid_file);
        let payload_dir = seed_fake_payload(
            &sha,
            &group_kill_payload(
                false,
                "",
                pid_file.to_str().unwrap(),
                gc_script.to_str().unwrap(),
                marker.with_extension("started").to_str().unwrap(),
            ),
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let config = long_config(port);
        let register = test_register();
        let (obs, mut status_rx, _log_rx) =
            Observability::channels(crate::status::SupervisorStatus::default());
        obs.set_allow_real_jobs(true);

        let obs2 = obs.clone();
        let client =
            tokio::spawn(async move { run(&config, &register, &obs2, never_shutdown()).await });

        let (mut ws, _uri) = accept_ws(&listener).await;
        let _register = next_text(&mut ws).await;

        ws.send(Message::Text(job_assign_json(&job_id, &sha)))
            .await
            .unwrap();

        // The runner writes the grandchild pid to a side file: the pid is
        // not part of the protocol and must not ride a frame.
        let grandchild_pid = wait_for_pid_file(&pid_file).await;
        // Wait until the grandchild has ACTUALLY exec'd (its .started marker
        // exists) — sending the group TERM during its exec window would kill
        // it before the trap is installed and the evidence would be lost.
        let mark_started = marker.with_extension("started");
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !mark_started.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "grandchild never started"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            unsafe { libc::kill(grandchild_pid as libc::pid_t, 0) } == 0,
            "grandchild must be alive before cancel"
        );

        ws.send(Message::Text(format!(
            r#"{{"type":"cancel","tenant":"driffs","jobId":"{job_id}"}}"#
        )))
        .await
        .unwrap();

        tokio::time::timeout(
            Duration::from_secs(20),
            status_rx.wait_for(|s| s.jobs_canceled == 1 && s.current_job.is_none()),
        )
        .await
        .expect("canceled render was not processed in time")
        .expect("status channel closed");

        // THE assertions: the Chrome stand-in RECEIVED the group TERM
        // (marker) and is dead. Pre-fix (pid-only signal) it is orphaned
        // alive with no marker.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let alive = unsafe { libc::kill(grandchild_pid as libc::pid_t, 0) } == 0;
            if !alive {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "grandchild (Chrome stand-in) survived the cancel — group TERM never reached it"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        assert!(
            mark_term.exists(),
            "grandchild never received SIGTERM — the kill did not reach the process GROUP"
        );

        ws.close(None).await.ok();
        while let Some(Ok(_)) = ws.next().await {}
        client.await.unwrap().expect("clean exit");
        std::fs::remove_dir_all(&payload_dir).ok();
    }

    /// ITEM 1 (escalation): when the runner AND its grandchild both ignore
    /// SIGTERM, the two-stage terminate must escalate to a group SIGKILL
    /// after CANCEL_GRACE. Only a group-scoped KILL can settle this tree;
    /// signalling only the runner pid (the old behaviour) hangs forever.
    #[cfg(unix)]
    #[tokio::test]
    async fn cancel_escalates_to_group_sigkill_when_term_is_ignored() {
        let pid = std::process::id();
        let sha = format!("test-group-escalate-{pid}");
        let job_id = format!("job-group-escalate-{pid}");
        let gc_script = grandchild_script_immune();
        // The immune stand-in touches this marker once its traps are installed.
        let started_marker = std::env::temp_dir().join(format!(
            "decent-gc-im-{pid}.started",
            pid = std::process::id()
        ));
        let pid_file = std::env::temp_dir().join(format!("{job_id}.gc-pid"));
        let _ = std::fs::remove_file(&pid_file);
        let payload_dir = seed_fake_payload(
            &sha,
            &group_kill_payload(
                true,
                "",
                pid_file.to_str().unwrap(),
                gc_script.to_str().unwrap(),
                started_marker.to_str().unwrap(),
            ),
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let config = long_config(port);
        let register = test_register();
        let (obs, mut status_rx, _log_rx) =
            Observability::channels(crate::status::SupervisorStatus::default());
        obs.set_allow_real_jobs(true);

        let obs2 = obs.clone();
        let client =
            tokio::spawn(async move { run(&config, &register, &obs2, never_shutdown()).await });

        let (mut ws, _uri) = accept_ws(&listener).await;
        let _register = next_text(&mut ws).await;

        ws.send(Message::Text(job_assign_json(&job_id, &sha)))
            .await
            .unwrap();

        let grandchild_pid = wait_for_pid_file(&pid_file).await;
        // Gate on the grandchild having exec'd: its trap must be installed
        // before the group TERM lands (immune variant also records .started).
        let gc_started = gc_script.with_extension("started");
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !gc_started.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "grandchild never started"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            unsafe { libc::kill(grandchild_pid as libc::pid_t, 0) } == 0,
            "grandchild must be alive before cancel"
        );

        ws.send(Message::Text(format!(
            r#"{{"type":"cancel","tenant":"driffs","jobId":"{job_id}"}}"#
        )))
        .await
        .unwrap();

        tokio::time::timeout(
            // TERM ignored → grace (10s) → KILL. 30s ceiling for CI slack.
            Duration::from_secs(30),
            status_rx.wait_for(|s| s.jobs_canceled == 1 && s.current_job.is_none()),
        )
        .await
        .expect("canceled render was not processed in time")
        .expect("status channel closed");

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let alive = unsafe { libc::kill(grandchild_pid as libc::pid_t, 0) } == 0;
            if !alive {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "TERM-immune grandchild survived — group SIGKILL escalation failed"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }

        ws.close(None).await.ok();
        while let Some(Ok(_)) = ws.next().await {}
        client.await.unwrap().expect("clean exit");
        std::fs::remove_dir_all(&payload_dir).ok();
    }

    /// DEFECT A (packet 3): Chrome daemonizes — Remotion spawns it
    /// `detached`, so it leaves the runner's process group and session
    /// entirely (own pgid, ppid eventually 1). The group kill CANNOT reach
    /// it, no matter how it is scoped. Containment therefore rides the exec
    /// boundary the supervisor owns: `DECENT_BROWSER_EXECUTABLE` is a
    /// wrapper that records the spawned pid (which becomes the browser's
    /// group-leader pid) before exec'ing the real binary.
    ///
    /// This test models Chrome FAITHFULLY, which the packet-2 sh stand-ins
    /// did not: the "browser" is spawned EXACTLY as Remotion spawns Chrome
    /// (own session via setsid — new session, new group, daemonized) and
    /// only then parks. The runner reads the wrapper path from the env the
    /// supervisor set, like a real payload does. A stand-in that stays in
    /// the runner's group would prove nothing — the group kill already
    /// covers that case; THIS test is about the process that left.
    #[cfg(unix)]
    #[tokio::test]
    async fn cancel_kills_daemonized_browser_recorded_at_exec() {
        use std::os::unix::fs::PermissionsExt;
        let pid = std::process::id();
        let sha = format!("test-daemon-browser-{pid}");
        let job_id = format!("job-daemon-browser-{pid}");

        // The "browser": daemonizes then parks. macOS has NO setsid(1)
        // binary (verified packet 2), so the daemonization is python's
        // os.setsid() — new session AND group, pgid == own pid: exactly
        // Remotion's `detached: true` Chrome. Static script written by the
        // test (no heredoc quoting maze). Bounded so a broken test cannot
        // leak it forever.
        let browser_stand_in = std::env::temp_dir().join(format!("decent-chrome-si-{pid}.py"));
        let started = std::env::temp_dir().join(format!("decent-chrome-si-{pid}.started"));
        let _ = std::fs::remove_file(&started);
        std::fs::write(
            &browser_stand_in,
            format!(
                "#!/usr/bin/env python3\nimport os, sys, time\nos.setsid()\nopen({m:?}, 'w').close()\ntime.sleep(120)\n",
                m = started.display().to_string()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&browser_stand_in, std::fs::Permissions::from_mode(0o755))
            .unwrap();

        // Seed a browser artifact so ensure_browser resolves: the manifest
        // points at the stand-in "executable" above.
        let browser_sha = format!("test-chrome-{pid}");
        let browser_dir = test_worker_root().join("browsers").join(&browser_sha);
        std::fs::create_dir_all(&browser_dir).unwrap();
        // The manifest must name the executable RELATIVE to the artifact
        // root — absolute paths are rejected by the escape guard in
        // browser_executable_in. So the stand-in lives INSIDE the artifact.
        std::fs::copy(&browser_stand_in, browser_dir.join("chrome-stand-in.sh")).unwrap();
        std::fs::write(browser_dir.join("executable"), "chrome-stand-in.sh").unwrap();

        // The runner: reads DECENT_BROWSER_EXECUTABLE (the supervisor's
        // wrapper) and spawns it EXACTLY as Remotion spawns Chrome —
        // detached, own group. Reports progress so the supervisor knows it
        // is alive. The wrapper records the pid before exec'ing the
        // stand-in.
        // The runner: reads DECENT_BROWSER_EXECUTABLE (the supervisor's
        // wrapper) and spawns it EXACTLY as Remotion spawns Chrome —
        // detached, own group (the `&` under `/bin/sh` with job control off
        // leaves the wrapper's setsid to do the group escape).
        let payload_script = r#"#!/bin/sh
"$DECENT_BROWSER_EXECUTABLE" about:blank --user-data-dir=/tmp >/dev/null 2>&1 &
echo '{"type":"progress","progress":0.5}'
while true; do sleep 5; done
"#;
        let payload_dir = seed_fake_payload(&sha, payload_script);

        // jobAssign WITH browser fields so ensure_browser engages.
        let assign = format!(
            r#"{{"type":"jobAssign","tenant":"driffs","jobId":"{job_id}","kind":"standard","durationFrames":1,"fps":30,"codec":"h264","bundleSha256":"s","bundleGetUrl":"u","payloadSha256":"{sha}","payloadGetUrl":"u","browserSha256":"{browser_sha}","browserGetUrl":"u","inputPropsGetUrl":"u","assetGetUrls":[],"outputPutUrl":"u","outputKey":"k","purgeAfter":true}}"#
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let config = long_config(port);
        let register = test_register();
        let (obs, mut status_rx, _log_rx) =
            Observability::channels(crate::status::SupervisorStatus::default());
        obs.set_allow_real_jobs(true);

        let obs2 = obs.clone();
        let client =
            tokio::spawn(async move { run(&config, &register, &obs2, never_shutdown()).await });

        let (mut ws, _uri) = accept_ws(&listener).await;
        let _register = next_text(&mut ws).await;

        ws.send(Message::Text(assign)).await.unwrap();

        // Wait until the browser stand-in is fully daemonized: the .started
        // marker is touched INSIDE the setsid'd child, so its existence
        // proves the daemon has its own session (not merely spawned).
        // 60s, not 5s: under full-suite parallel load the payload spawn +
        // exec chain can take far longer than solo runs (packet-2 lesson —
        // the error-path test's 5s deadline flakes for exactly this reason).
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        while !started.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "browser stand-in never daemonized"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // Identify the daemon by the CONTAINMENT'S OWN DATA SOURCE: the
        // exec wrapper recorded its pid to .decent-browser-pids in the job
        // workdir, and the wrapper exec'd INTO the daemon (same pid). This
        // asserts the mechanism itself, not a cmdline heuristic.
        let workdirs = job_workdirs(&job_id);
        assert!(!workdirs.is_empty(), "job workdir not found");
        let pidfile_contents = std::fs::read_to_string(workdirs[0].join(".decent-browser-pids"))
            .expect("wrapper never recorded the browser pid — containment data source missing");
        let daemon_pid: u32 = pidfile_contents
            .trim()
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .parse()
            .expect("pidfile holds a pid");
        // ...and it must be OUTSIDE the runner's group: read its pgid via ps.
        let ps = std::process::Command::new("ps")
            .args(["-o", "pgid=", "-p", &daemon_pid.to_string()])
            .output()
            .unwrap();
        let daemon_pgid: u32 = String::from_utf8_lossy(&ps.stdout).trim().parse().unwrap();
        // The runner's pid equals its pgid (process_group(0)); find it via
        // the payload script marker.
        let runner_probe = std::process::Command::new("pgrep")
            .arg("-f")
            .arg(format!("test-daemon-browser-{pid}"))
            .output()
            .unwrap();
        let runner_pids: Vec<u32> = String::from_utf8_lossy(&runner_probe.stdout)
            .split_whitespace()
            .filter_map(|s| s.parse().ok())
            .collect();
        let runner_pgid = runner_pids
            .first()
            .map(|rp| {
                let ps = std::process::Command::new("ps")
                    .args(["-o", "pgid=", "-p", &rp.to_string()])
                    .output()
                    .unwrap();
                String::from_utf8_lossy(&ps.stdout)
                    .trim()
                    .parse::<u32>()
                    .unwrap()
            })
            .expect("runner process found");
        assert_ne!(
            daemon_pgid, runner_pgid,
            "stand-in failed to model Chrome: it must leave the runner's group"
        );

        // Sanity: alive before cancel.
        assert!(
            unsafe { libc::kill(daemon_pid as libc::pid_t, 0) } == 0,
            "daemonized browser must be alive before cancel"
        );

        ws.send(Message::Text(format!(
            r#"{{"type":"cancel","tenant":"driffs","jobId":"{job_id}"}}"#
        )))
        .await
        .unwrap();

        tokio::time::timeout(
            Duration::from_secs(20),
            status_rx.wait_for(|s| s.jobs_canceled == 1 && s.current_job.is_none()),
        )
        .await
        .expect("canceled render was not processed in time")
        .expect("status channel closed");

        // THE assertion: the daemonized browser — outside the runner's
        // group since before the cancel — is dead. Pre-fix, the group TERM/
        // KILL never reached it and it parked for its full 120s.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let alive = unsafe { libc::kill(daemon_pid as libc::pid_t, 0) } == 0;
            if !alive {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "daemonized browser survived the cancel — exec-boundary containment failed"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }

        ws.close(None).await.ok();
        while let Some(Ok(_)) = ws.next().await {}
        client.await.unwrap().expect("clean exit");
        std::fs::remove_dir_all(&payload_dir).ok();
        std::fs::remove_dir_all(&browser_dir).ok();
    }

    /// PACKET 5: the terminate path must run to completion even when the
    /// dispatch WebSocket dies MID-GRACE. Pre-fix, `run()` returned Err on
    /// the WS error, dropping the terminate future: no SIGKILL escalation,
    /// no browser sweep, no purge — runner + daemonized browser survived.
    /// This is the production shape: dispatch redeploys or the network
    /// blips during a cancel grace window.
    #[cfg(unix)]
    #[tokio::test]
    async fn ws_drop_mid_grace_still_kills_tree_and_purges() {
        use std::os::unix::fs::PermissionsExt;
        let pid = std::process::id();
        let sha = format!("test-wsdrop-{pid}");
        let job_id = format!("job-wsdrop-{pid}");

        // Daemonizing browser stand-in: python os.setsid() (macOS has no
        // setsid(1)) — new session AND group, pgid == own pid, exactly
        // Remotion's detached Chrome. Bounded at 120s so a broken test
        // cannot leak it forever.
        let browser_stand_in = std::env::temp_dir().join(format!("decent-chrome-wsdrop-{pid}.py"));
        let started = std::env::temp_dir().join(format!("decent-chrome-wsdrop-{pid}.started"));
        let _ = std::fs::remove_file(&started);
        std::fs::write(
            &browser_stand_in,
            format!(
                "#!/usr/bin/env python3\nimport os, sys, time\nos.setsid()\nopen({m:?}, 'w').close()\ntime.sleep(120)\n",
                m = started.display().to_string()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&browser_stand_in, std::fs::Permissions::from_mode(0o755))
            .unwrap();

        let browser_sha = format!("test-chrome-wsdrop-{pid}");
        let browser_dir = test_worker_root().join("browsers").join(&browser_sha);
        std::fs::create_dir_all(&browser_dir).unwrap();
        std::fs::copy(&browser_stand_in, browser_dir.join("chrome-stand-in.sh")).unwrap();
        std::fs::write(browser_dir.join("executable"), "chrome-stand-in.sh").unwrap();

        // The runner spawns the supervisor wrapper (DECENT_BROWSER_EXECUTABLE)
        // exactly as Remotion spawns Chrome, reports progress, then WEDGES:
        // traps TERM and freezes the event loop. The supervisor cannot get a
        // graceful exit; it must ride the 10s grace to SIGKILL — and that
        // ride now spans a dead WebSocket.
        let payload_script = r#"#!/bin/sh
trap '' TERM INT
"$DECENT_BROWSER_EXECUTABLE" about:blank --user-data-dir=/tmp >/dev/null 2>&1 &
echo '{"type":"progress","progress":0.5}'
while true; do sleep 5; done
"#;
        let payload_dir = seed_fake_payload(&sha, payload_script);

        let assign = format!(
            r#"{{"type":"jobAssign","tenant":"driffs","jobId":"{job_id}","kind":"standard","durationFrames":1,"fps":30,"codec":"h264","bundleSha256":"s","bundleGetUrl":"u","payloadSha256":"{sha}","payloadGetUrl":"u","browserSha256":"{browser_sha}","browserGetUrl":"u","inputPropsGetUrl":"u","assetGetUrls":[],"outputPutUrl":"u","outputKey":"k","purgeAfter":true}}"#
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let config = long_config(port);
        let register = test_register();
        let (obs, _status_rx, _log_rx) =
            Observability::channels(crate::status::SupervisorStatus::default());
        obs.set_allow_real_jobs(true);

        let obs2 = obs.clone();
        let mut client =
            tokio::spawn(async move { run(&config, &register, &obs2, never_shutdown()).await });

        let (mut ws, _uri) = accept_ws(&listener).await;
        let _register = next_text(&mut ws).await;

        ws.send(Message::Text(assign)).await.unwrap();

        // Wait for full daemonization: marker touched INSIDE the setsid'd child.
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        while !started.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "browser stand-in never daemonized"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // Daemon pid from the containment's own data source.
        let workdirs = job_workdirs(&job_id);
        assert!(!workdirs.is_empty(), "job workdir not found");
        let pidfile_contents = std::fs::read_to_string(workdirs[0].join(".decent-browser-pids"))
            .expect("wrapper never recorded the browser pid — containment data source missing");
        let daemon_pid: u32 = pidfile_contents
            .trim()
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .parse()
            .expect("pidfile holds a pid");

        // Prove the daemon REALLY daemonized: own group, pgid == pid.
        let ps = std::process::Command::new("ps")
            .args(["-o", "pgid=", "-p", &daemon_pid.to_string()])
            .output()
            .unwrap();
        let daemon_pgid: u32 = String::from_utf8_lossy(&ps.stdout).trim().parse().unwrap();
        assert_eq!(
            daemon_pgid, daemon_pid,
            "stand-in failed to model Chrome: pgid must equal its own pid"
        );

        // Sanity: alive, and outside any runner group.
        assert!(
            unsafe { libc::kill(daemon_pid as libc::pid_t, 0) } == 0,
            "daemonized browser must be alive before cancel"
        );

        // CANCEL. The runner ignores TERM (trap ''); the supervisor enters
        // the 10s CANCEL_GRACE.
        ws.send(Message::Text(format!(
            r#"{{"type":"cancel","tenant":"driffs","jobId":"{job_id}"}}"#
        )))
        .await
        .unwrap();

        // Give the supervisor a moment to enter the grace window (TERM sent,
        // waiting on the child), then RIP THE SOCKET — the production shape:
        // dispatch redeploys mid-grace. Hard abort of the server side.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        drop(ws);

        // THE assertion, part 1: `run()` must not give up. Pre-fix it
        // returned Err immediately when the WS died (ws error -> `?`),
        // dropping the terminate future. Post-fix it drains the job to
        // completion: TERM -> 10s grace -> SIGKILL -> browser sweep -> purge.
        // The drain necessarily takes ~10s (grace) — a fast pass here means
        // the grace path was never exercised.
        let t0 = std::time::Instant::now();
        let outcome = tokio::time::timeout(Duration::from_secs(45), &mut client)
            .await
            .expect("run() hung — drain did not complete in 45s")
            .expect("client task panicked");
        let elapsed = t0.elapsed();
        let ran_for = elapsed.as_secs_f64();
        assert!(
            ran_for >= 7.0,
            "drain finished in {ran_for:.1}s — the grace path was not exercised (a dropped job returns ~0s; the 10s grace minus the 1.5s pre-drop cancel lead is ~8.5s)"
        );
        // The WS error itself is honest (the socket really died); what matters
        // is that run() did not RETURN until the drain finished. Ok(()) or
        // Err(websocket error) are both acceptable — the tree assertions
        // below are the load-bearing proof.
        match &outcome {
            Ok(()) => {}
            Err(e) => assert!(
                format!("{e:#}").contains("websocket error"),
                "unexpected error from run(): {e:#}"
            ),
        }

        // THE assertion, part 2: the whole tree is dead — the wedged runner
        // (group SIGKILL) AND the daemonized browser (pidfile sweep), which
        // no group signal can reach.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let alive = unsafe { libc::kill(daemon_pid as libc::pid_t, 0) } == 0;
            if !alive {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "daemonized browser survived a WS drop mid-grace — teardown was abandoned"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }

        // THE assertion, part 3: purge invariant held — no workdir remains.
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            job_workdirs(&job_id).is_empty(),
            "workdir survived a WS drop mid-grace — purge invariant violated"
        );

        std::fs::remove_dir_all(&payload_dir).ok();
        std::fs::remove_dir_all(&browser_dir).ok();
    }

    /// PACKET 5 FOLLOW-UP (orchestrator verification finding V-3): the
    /// `upgrade-required` close is an EIGHTH exit path, and packet 5's own
    /// exit inventory classified it into neither bucket — not among the seven
    /// that drain, not among the pre-loop returns deemed unreachable with a
    /// live job. It returned `Ok(())` while holding one.
    ///
    /// Reachable exactly when dispatch redeploys with a raised minimum
    /// protocol version and closes live connections — i.e. while nodes are
    /// mid-render. Worse than the generic close path: this branch exits
    /// deliberately without retry, so in `start` mode the process dies and
    /// takes the runtime with it, leaving the render tree with nothing left
    /// to escalate it.
    ///
    /// Focused by design: the drain MECHANISM (browser sweep, group KILL) is
    /// already proven by `ws_drop_mid_grace_still_kills_tree_and_purges`. What
    /// was unproven is whether THIS branch calls it, so the load-bearing
    /// signals here are the grace timing and the purge invariant.
    #[cfg(unix)]
    #[tokio::test]
    async fn upgrade_required_close_mid_grace_still_drains() {
        use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
        use tokio_tungstenite::tungstenite::protocol::CloseFrame;

        let pid = std::process::id();
        let sha = format!("test-upgrade-drain-{pid}");
        let job_id = format!("job-upgrade-drain-{pid}");

        // Wedged runner: traps TERM so the supervisor cannot get a graceful
        // exit and must ride the full 10s CANCEL_GRACE to SIGKILL. It records
        // its own pid first — gating on the WORKDIR is not enough, because
        // the supervisor creates that before it ever spawns the runner, so a
        // runner that died instantly would still sail past the gate and leave
        // this test asserting on an empty drain.
        let pid_file = std::env::temp_dir().join(format!("{job_id}.runner-pid"));
        let _ = std::fs::remove_file(&pid_file);
        let payload_script = format!(
            r#"#!/bin/sh
trap '' TERM INT
echo $$ > "{pidfile}"
echo '{{"type":"progress","progress":0.5}}'
while true; do sleep 5; done
"#,
            pidfile = pid_file.display()
        );
        let payload_dir = seed_fake_payload(&sha, &payload_script);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let config = long_config(port);
        let register = test_register();
        let (obs, _status_rx, _log_rx) =
            Observability::channels(crate::status::SupervisorStatus::default());
        obs.set_allow_real_jobs(true);

        let obs2 = obs.clone();
        let mut client =
            tokio::spawn(async move { run(&config, &register, &obs2, never_shutdown()).await });

        let (mut ws, _uri) = accept_ws(&listener).await;
        let _register = next_text(&mut ws).await;

        ws.send(Message::Text(job_assign_json(&job_id, &sha)))
            .await
            .unwrap();

        // Gate on the runner REALLY running — its own pid, written by itself.
        let runner_pid = wait_for_pid_file(&pid_file).await;
        assert!(
            unsafe { libc::kill(runner_pid as libc::pid_t, 0) } == 0,
            "runner must be alive before cancel"
        );

        ws.send(Message::Text(format!(
            r#"{{"type":"cancel","tenant":"driffs","jobId":"{job_id}"}}"#
        )))
        .await
        .unwrap();

        // Let the supervisor enter the grace window, then close the way a
        // version-bumping dispatch redeploy does.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        // We must genuinely be MID-grace: TERM has landed and been ignored,
        // SIGKILL has not. If the runner were already dead here the drain
        // would have nothing to await and the timing assert below would pass
        // for the wrong reason.
        assert!(
            unsafe { libc::kill(runner_pid as libc::pid_t, 0) } == 0,
            "runner died before the close — not mid-grace, so this test would prove nothing"
        );
        ws.send(Message::Close(Some(CloseFrame {
            code: CloseCode::Normal,
            reason: "upgrade-required: protocol 3 or newer".into(),
        })))
        .await
        .unwrap();

        // THE assertion: `run()` must not return until the drain finishes.
        // Pre-fix it returned immediately, dropping the terminate future.
        let t0 = std::time::Instant::now();
        let outcome = tokio::time::timeout(Duration::from_secs(45), &mut client)
            .await
            .expect("run() hung — drain did not complete in 45s")
            .expect("client task panicked");
        let ran_for = t0.elapsed().as_secs_f64();
        assert!(
            ran_for >= 7.0,
            "run() returned in {ran_for:.1}s — the upgrade-required branch abandoned the grace (a dropped job returns ~0s; 10s grace minus the 1.5s cancel lead is ~8.5s)"
        );
        outcome.expect("upgrade-required close is a clean exit");

        // Purge invariant: teardown ran to completion, not just far enough.
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            job_workdirs(&job_id).is_empty(),
            "workdir survived an upgrade-required close mid-grace — teardown was abandoned"
        );
        assert!(
            unsafe { libc::kill(runner_pid as libc::pid_t, 0) } != 0,
            "wedged runner survived an upgrade-required close mid-grace — escalation was abandoned"
        );

        let _ = std::fs::remove_file(&pid_file);
        std::fs::remove_dir_all(&payload_dir).ok();
    }

    /// ITEM 4: an `error` event does not imply the runner exited. This fake
    /// emits error and keeps running (with a live grandchild). The
    /// supervisor must terminate the tree BEFORE Drop purges the workdir —
    /// pre-fix it returned immediately and purged around a live writer.
    #[cfg(unix)]
    #[tokio::test]
    async fn error_event_terminates_child_tree_before_purge() {
        let pid = std::process::id();
        let sha = format!("test-error-kill-{pid}");
        let job_id = format!("job-error-kill-{pid}");
        let marker = std::env::temp_dir().join(format!("decent-gc-err-{pid}"));
        let mark_term = marker.with_extension("term");
        let _ = std::fs::remove_file(&mark_term);
        let gc_script = grandchild_script_evidence(&marker.to_string_lossy());
        let pid_file = std::env::temp_dir().join(format!("{job_id}.gc-pid"));
        let _ = std::fs::remove_file(&pid_file);
        let payload_dir = seed_fake_payload(
            &sha,
            &group_kill_payload(
                false,
                r#"echo "{\"type\":\"error\",\"message\":\"render exploded but I keep running\"}""#,
                pid_file.to_str().unwrap(),
                gc_script.to_str().unwrap(),
                marker.with_extension("started").to_str().unwrap(),
            ),
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let config = long_config(port);
        let register = test_register();
        let (obs, mut status_rx, _log_rx) =
            Observability::channels(crate::status::SupervisorStatus::default());
        obs.set_allow_real_jobs(true);

        let obs2 = obs.clone();
        let client =
            tokio::spawn(async move { run(&config, &register, &obs2, never_shutdown()).await });

        let (mut ws, _uri) = accept_ws(&listener).await;
        let _register = next_text(&mut ws).await;

        ws.send(Message::Text(job_assign_json(&job_id, &sha)))
            .await
            .unwrap();

        let grandchild_pid = wait_for_pid_file(&pid_file).await;
        // Gate on exec completion: TERM during the exec window kills the
        // grandchild pre-trap and destroys the evidence.
        let gc_started = marker.with_extension("started");
        // 60s, not 5s: under full-suite parallel load the payload spawn chain
        // can exceed 5s (pre-existing flake — reproduced at ae671f9 baseline,
        // 2026-08-21). The assertion is unchanged; only the wait is honest
        // about scheduling latency.
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        while !gc_started.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "grandchild never started"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        // Drain frames until the jobFailed (the error event) reaches us.
        loop {
            let t = tokio::time::timeout(Duration::from_secs(5), next_text(&mut ws))
                .await
                .expect("expected jobFailed before timeout");
            let v: serde_json::Value = serde_json::from_str(&t).unwrap();
            if v["type"] == "jobFailed" {
                break;
            }
        }

        tokio::time::timeout(
            Duration::from_secs(20),
            status_rx.wait_for(|s| s.jobs_failed == 1 && s.current_job.is_none()),
        )
        .await
        .expect("jobFailed was not processed in time")
        .expect("status channel closed");

        // Purge still happened (Drop ran on the error path).
        assert!(
            job_workdirs(&job_id).is_empty(),
            "workdir must be purged after the error path"
        );

        // THE assertion: the tree is dead — terminate ran BEFORE Drop, not
        // after, and the group TERM reached the grandchild too.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let alive = unsafe { libc::kill(grandchild_pid as libc::pid_t, 0) } == 0;
            if !alive {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "grandchild survived the error-path purge — terminate-before-drop failed"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }

        ws.close(None).await.ok();
        while let Some(Ok(_)) = ws.next().await {}
        client.await.unwrap().expect("clean exit");
        std::fs::remove_dir_all(&payload_dir).ok();
    }
}
