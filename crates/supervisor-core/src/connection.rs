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
//!
//! The crate's tests for everything in this file live in the child module
//! `connection/tests.rs` (declared below as `#[cfg(test)] mod tests;`).

use std::time::{Duration, Instant, SystemTime};

use anyhow::{anyhow, Context};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::oneshot;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::{Message, Utf8Bytes};

use crate::protocol::{
    HeartbeatMessage, JobAcceptedMessage, JobRejectedMessage, RegisterMessage, RejectReason,
    ServerMessage, WorkerMessage,
};
use crate::runner::{run_job, InFlightJob};
use crate::status::{ConnectionState, JobPhase, JobStatus, LogLine, Observability};

/// Policy for handing an available supervisor update back to the process
/// owner at a structurally idle point. The core only closes the connection;
/// package-manager and service-manager work stays outside supervisor-core.
#[derive(Debug, Clone)]
pub struct AutoUpgradePolicy {
    pub quiet_period: Duration,
    /// A target whose last unattended attempt failed recently. Dispatch may
    /// repeat the notification on every reconnect; suppressing it here avoids
    /// a package-manager/restart loop while still surfacing the banner.
    pub suppressed_version: Option<String>,
    /// Remaining suppression duration at process-loop construction. The gate
    /// keeps the target and automatically re-enables it when this expires.
    pub suppressed_for: Option<Duration>,
}

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
    /// PACKET 23: reconnect after a disconnect instead of returning (and, in
    /// `decent start`, exiting the process). Abrupt TLS closes (the
    /// `fly deploy` shape), RSTs and clean server closes alike loop back to a
    /// fresh connect with backoff. Terminal exits — shutdown signal,
    /// smoke-test heartbeat limit, upgrade-required close — still end the
    /// run. Tests that pin a single session's teardown set this to false.
    pub reconnect: bool,
    /// PACKET 28: base delay for the jittered exponential reconnect
    /// backoff (attempt n targets base·2^(n−1), full jitter below, capped
    /// at `reconnect_backoff_max`). The initial-connect retry loop inside
    /// a session keeps the FLAT `connect_retry_delay` — launchd/systemd
    /// restart policy owns cold-start cadence.
    pub reconnect_backoff_base: Duration,
    /// PACKET 28: hard cap on the reconnect backoff target (and thus on
    /// any jittered delay — jitter is drawn in [0, target], never above).
    /// Doubles as the healthy-session threshold: a session that stayed
    /// connected this long resets the curve.
    pub reconnect_backoff_max: Duration,
    /// None preserves the historical/manual-only behaviour byte for byte.
    pub auto_upgrade: Option<AutoUpgradePolicy>,
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
            reconnect: true,
            reconnect_backoff_base: Duration::from_secs(1),
            reconnect_backoff_max: Duration::from_secs(60),
            auto_upgrade: None,
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

/// PACKET 40 (audit-api-ux): classify tungstenite connect errors.
/// `Error::Http(resp)` is the handshake being REFUSED at the HTTP layer —
/// auth/version policy, not reachability.
fn is_auth_rejection(e: &tokio_tungstenite::tungstenite::Error) -> bool {
    matches!(
        rejection_status_opt(e),
        Some(reqwest::StatusCode::UNAUTHORIZED) | Some(reqwest::StatusCode::FORBIDDEN)
    )
}

fn rejection_status_opt(e: &tokio_tungstenite::tungstenite::Error) -> Option<reqwest::StatusCode> {
    match e {
        tokio_tungstenite::tungstenite::Error::Http(resp) => Some(resp.status()),
        _ => None,
    }
}

fn rejection_status(e: &tokio_tungstenite::tungstenite::Error) -> reqwest::StatusCode {
    rejection_status_opt(e).unwrap_or(reqwest::StatusCode::INTERNAL_SERVER_ERROR)
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
/// Why a connection session ended. `run()` treats the non-terminal
/// variants as "reconnect with backoff"; the terminal ones end the
/// process-level run (operator asked for it, or dispatch demanded an
/// upgrade a retry cannot fix).
///
/// PACKET 23: before this, every disconnect — abrupt TLS abort
/// (`peer closed connection without sending TLS close_notify`),
/// ECONNRESET, EOF mid-frame, a clean server Close, stream end —
/// returned out of `run()` and `decent start` exited with code 1 (or
/// 0). Every `fly deploy` of dispatch killed every foreground node;
/// daemon nodes survived only because launchd restarted them. There
/// was NO reconnect path to unify with: the premise "clean closes
/// already reconnect" was false. Now `run()` itself loops: one
/// session's teardown (drains any in-flight job exactly as before)
/// is followed by a fresh connect, with the same backoff the
/// initial-connect loop uses, until shutdown, heartbeat-limit, or an
/// upgrade-required close ends the run.
/// Why the process-level connection loop stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionExit {
    Shutdown,
    HeartbeatLimit,
    UpgradeRequired,
    /// A nonterminal socket close when reconnect was explicitly disabled
    /// (principally focused tests and embedders owning their own retry loop).
    Disconnected,
    /// The socket is closed and no job or teardown is in flight. The caller
    /// may now run its trusted package-manager upgrade without an assignment
    /// racing into the swap window.
    AutoUpgrade {
        version: String,
    },
}

#[derive(Debug)]
enum Disconnect {
    /// SIGTERM/SIGINT or TUI quit — run() returns Ok.
    Shutdown,
    /// Smoke-test heartbeat_limit reached — run() returns Ok.
    HeartbeatLimit,
    /// dispatch raised its minimum protocol version; retrying is
    /// pointless until the operator upgrades. run() returns Ok.
    UpgradeRequired,
    /// An available update remained continuously idle for the configured
    /// quiet period. The socket has been closed without canceling a job.
    AutoUpgrade(String),
    /// Socket died without a close handshake (TLS abort, RST, EOF
    /// mid-frame) — the fly-deploy shape. Reconnect.
    Abnormal,
    /// Server sent a clean Close (or the stream simply ended).
    /// Reconnect: dispatch redeploys also produce clean closes, and
    /// an operator restarting dispatch should not silently take
    /// every foreground node down either.
    Clean,
}

/// Pure state machine for the quiet-idle handoff. `run_session` owns it in
/// the same event loop that owns `in_flight`, so an assignment cannot slip
/// between an external "looks idle" check and the socket close.
#[derive(Debug)]
struct AutoUpgradeGate {
    policy: Option<AutoUpgradePolicy>,
    target: Option<String>,
    idle_since: Option<Instant>,
    suppressed_until: Option<Instant>,
}

impl AutoUpgradeGate {
    fn new(policy: Option<AutoUpgradePolicy>) -> Self {
        Self::new_at(policy, Instant::now())
    }

    fn new_at(policy: Option<AutoUpgradePolicy>, now: Instant) -> Self {
        let suppressed_until = policy
            .as_ref()
            .and_then(|p| p.suppressed_for)
            .and_then(|duration| now.checked_add(duration));
        Self {
            policy,
            target: None,
            idle_since: None,
            suppressed_until,
        }
    }

    fn announce(&mut self, version: String) {
        // Dispatch may repeat the same notice. Only a NEW target resets the
        // accumulated idle window; repetition must not defer forever.
        if self.target.as_deref() != Some(version.as_str()) {
            self.target = Some(version);
            self.idle_since = None;
        }
    }

    fn mark_busy(&mut self) {
        self.idle_since = None;
    }

    fn observe(&mut self, enabled: bool, idle: bool, now: Instant) {
        let target_suppressed = self
            .policy
            .as_ref()
            .and_then(|p| p.suppressed_version.as_deref())
            == self.target.as_deref()
            && self.suppressed_until.is_some_and(|until| now < until);
        if self.policy.is_none() || self.target.is_none() || target_suppressed || !enabled || !idle
        {
            self.idle_since = None;
            return;
        }
        self.idle_since.get_or_insert(now);
    }

    fn ready(&self, now: Instant) -> Option<String> {
        let policy = self.policy.as_ref()?;
        let since = self.idle_since?;
        (now.saturating_duration_since(since) >= policy.quiet_period)
            .then(|| self.target.clone())?
    }
}

/// C-4 (audit T-15): does this terminal frame belong to the in-flight job?
/// Both the job id AND the attempt must match — dispatch requeues a failed
/// or refunded job as attempt+1 of the SAME job id, so a terminal frame for
/// attempt N (a draining teardown from an earlier cancel, or any late
/// frame) must never clear an in-flight attempt N+1: that would drop the
/// JoinHandle un-awaited (the strand packet 5 exists to prevent), show the
/// node idle while a render runs, and remove the busy rejection that
/// protects it. Attempt equality is `Option<u32>` equality: a frame WITHOUT
/// an attempt matches only an in-flight job WITHOUT an attempt.
fn frame_is_for(
    in_flight: Option<(&str, Option<u32>)>,
    job_id: &str,
    attempt: Option<u32>,
) -> bool {
    match in_flight {
        Some((in_flight_id, in_flight_attempt)) => {
            in_flight_id == job_id && in_flight_attempt == attempt
        }
        None => false,
    }
}

/// One connect-serve-disconnect session, ending in a [`Disconnect`].
///
/// All in-flight-job draining semantics live here and are unchanged from the
/// packet-5 era: whichever way the socket dies, the session does not return
/// until the render tree is dead and the workdir purged.
///
/// `run()` (below) calls this in a loop when `config.reconnect` is set.
async fn run_session(
    config: &ConnectionConfig,
    request: &tokio_tungstenite::tungstenite::handshake::client::Request,
    register: &RegisterMessage,
    obs: &Observability,
    shutdown: &mut oneshot::Receiver<()>,
    protected_keys: &mut Vec<String>,
) -> anyhow::Result<Disconnect> {
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
        s.auto_upgrade_enabled = obs.auto_upgrade_enabled();
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
            _ = &mut *shutdown => {
                tracing::info!("shutdown signal received before connect — exiting");
                obs.update_status(|s| s.connection = ConnectionState::Disconnected);
                return Ok(Disconnect::Shutdown);
            }
            result = connect_async(request.clone()) => result,
        };
        match connect {
            Ok((ws, _resp)) => break ws,
            // PACKET 40 (audit-api-ux): an HTTP-level rejection (401/403)
            // is NOT "dispatch unreachable" — retrying it 15 times sends
            // the operator to debug their network when the token is bad or
            // revoked. Fail fast, name the real cause.
            Err(e) if is_auth_rejection(&e) => {
                let msg = format!(
                    "Dispatch rejected this node's credentials (HTTP {}): the worker token                      is invalid, expired, or revoked. Re-run `decent login` with a fresh                      token. (Retrying will not help.)",
                    rejection_status(&e)
                );
                tracing::error!(status = %rejection_status(&e), "auth rejected at connect");
                obs.update_status(|s| {
                    s.connection = ConnectionState::Disconnected;
                    s.last_error = Some(msg.clone());
                });
                obs.log(LogLine::error(&msg));
                return Err(e).context(msg);
            }
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
                    _ = &mut *shutdown => {
                        tracing::info!("shutdown signal received while retrying — exiting");
                        obs.update_status(|s| s.connection = ConnectionState::Disconnected);
                        return Ok(Disconnect::Shutdown);
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
                // Initial-connect failure stays a hard error: a node that
                // cannot reach dispatch AT ALL should surface exit 1 to
                // launchd/systemd, whose restart policy owns cold-start
                // retries. Reconnect-on-disconnect (run(), below) is about a
                // node that WAS connected.
                return Err(e).with_context(|| msg);
            }
        }
    };
    tracing::info!(url = %config.dispatch_url, "connected to dispatch");
    obs.update_status(|s| s.connection = ConnectionState::Connected);
    obs.log(LogLine::info("Connected to dispatch"));

    // Startup sweeps (ITEM 3, D-11): SIGKILL/power-loss killed a previous
    // supervisor before WorkDir::Drop could run, orphaning job workdirs
    // (customer content) under the temp dir. Once we are connected — and
    // before any job can be assigned — remove abandoned ones. The sweep's
    // own safety rule (pid-liveness for supervisor dirs, age gate for
    // runner dirs) is what keeps a live sibling supervisor's workdir safe.
    // Both sweeps are blocking directory I/O: they run on the blocking
    // pool, not the async runtime (D-11), and are awaited to completion so
    // the ordering guarantee — swept before the register frame is sent —
    // is unchanged.
    let swept = match tokio::task::spawn_blocking(crate::sweep::sweep_stale_workdirs).await {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!(error = %e, "startup workdir sweep join failed");
            0
        }
    };
    if swept > 0 {
        obs.log(LogLine::info(format!(
            "Swept {swept} abandoned job workdir(s) left by a hard-killed supervisor"
        )));
    }

    // Cache LRU sweep (2.9, packet 17): enforce the size cap at startup,
    // before any jobAssign can arrive. On the FIRST connect nothing has been
    // seen, so the protected set is empty — but on a RECONNECT the caller
    // hands back the cache keys of the jobs this supervisor has already run
    // (D-10): an empty set here would let the sweep evict a payload/browser
    // this very node may be re-assigned the moment it re-registers (and, on
    // a machine with more than one supervisor sharing a worker root, one a
    // SIBLING is still using — the cache sweep has no pid-liveness rule of
    // its own; the protected set is the only thing standing between an LRU
    // pass and a live artifact). This is also the sweep that eventually
    // reclaims the pre-eviction residue (old test-* dirs and every
    // superseded payload/browser/bundle) on real nodes.
    let sweep_keys = protected_keys.clone();
    let sweep =
        tokio::task::spawn_blocking(move || crate::cache::sweep_node_caches(&sweep_keys)).await;
    match sweep {
        Ok(Ok(out)) if out.evicted > 0 || !protected_keys.is_empty() => {
            // Surface evictions on the status log (TUI log pane + daemon log)
            // — packet 20: an operator should see the cache being reclaimed
            // without reading tracing output. D-10: the protected count rides
            // along — after a job ran, a reconnect sweep reporting an EMPTY
            // protected set is exactly the bug this line exposes.
            obs.log(LogLine::info(format!(
                "Cache sweep: {} entries, {} -> {} bytes ({} evicted, {} protected)",
                out.entries,
                out.bytes_before,
                out.bytes_after,
                out.evicted,
                protected_keys.len()
            )));
        }
        Ok(Ok(_)) => {}
        Ok(Err(e)) => tracing::warn!(error = %e, "startup cache sweep failed"),
        Err(e) => tracing::warn!(error = %e, "startup cache sweep join failed"),
    }

    let (mut sink, mut stream) = ws.split();

    let send = |msg: WorkerMessage| -> Utf8Bytes {
        let frame = serde_json::to_string(&msg).expect("worker messages always serialize");
        tracing::info!(frame = %frame, "→ send");
        Utf8Bytes::from(frame)
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

    // PACKET 37: a failed send is a DEAD SOCKET, not a session error to
    // propagate — `?` here skipped drain_in_flight_jobs (stranding a
    // mid-grace render tree) and bypassed the packet-23 reconnect design.
    // This is the register frame, before any job exists, so there is
    // nothing to drain yet — the reconnect path is what matters.
    if sink
        .send(Message::Text(send(WorkerMessage::Register(
            register.clone(),
        ))))
        .await
        .is_err()
    {
        obs.update_status(|s| s.connection = ConnectionState::Disconnected);
        obs.log(LogLine::warn(
            "Register send failed — reconnecting".to_string(),
        ));
        return Ok(Disconnect::Abnormal);
    }

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
    // Short polling cadence, long eligibility window. The gate itself uses
    // the configured quiet period; 250ms merely bounds handoff latency and
    // keeps tests fast without adding another cross-task control channel.
    let mut auto_upgrade_tick = tokio::time::interval(Duration::from_millis(250));
    auto_upgrade_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut auto_upgrade_gate = AutoUpgradeGate::new(config.auto_upgrade.clone());
    let mut heartbeats_sent = 0u32;
    let (worker_tx, mut worker_rx) = tokio::sync::mpsc::unbounded_channel::<WorkerMessage>();
    let mut in_flight: Option<InFlightJob> = None;
    // Job id + attempt of the last dispatch-initiated cancel whose terminal
    // frame has not been observed yet. Recorded at cancel receipt — BEFORE
    // the runner is killed — so the render abort that follows is never
    // mistaken for a genuine failure. Cleared when that job's terminal frame
    // arrives. The attempt comes from the in-flight record being torn down:
    // `CancelMessage` carries no attempt on the wire, and a cancel can only
    // apply to the job actually in flight.
    let mut canceled_job: Option<(String, Option<u32>)> = None;

    // PACKET 5: a dispatch-canceled job whose terminate is still running.
    // The Cancel arm hands the job here instead of dropping its task handle —
    // `run()` awaits it on EVERY exit path so the TERM -> grace -> SIGKILL ->
    // browser-sweep -> purge sequence cannot be abandoned when the socket
    // dies mid-grace (dispatch redeploy / network blip).
    //
    // PACKET 37 (audit 5): a Vec, not a slot — assign→cancel→assign→cancel
    // used to OVERWRITE the first teardown and detach its handle. Bounded by
    // the busy-gate above (in_flight.is_some() rejects new assigns while a
    // job runs) plus the draining count below: at most a handful of
    // cancels per session can stack before the heartbeat advertises the
    // load. Hard cap for pathological peers: extra cancels beyond the cap
    // are still CANCELED and their handles awaited at drain — only the
    // per-cancel status bookkeeping is capped, never the completion
    // guarantee.
    let mut draining: Vec<InFlightJob> = Vec::new();
    // Purge failures already announced through obs.log (announce-once).
    let mut announced_purge_failures: Vec<std::path::PathBuf> = Vec::new();
    /// Teardowns are bounded by real job count; this cap only guards the
    /// Vec against a pathological cancel flood (each cancel requires a real
    /// prior jobAssign, so legitimate traffic stays far below it).
    const MAX_DRAINING_TEARDOWNS: usize = 16;

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
        draining: &mut Vec<InFlightJob>,
    ) {
        if let Some(mut job) = in_flight.take() {
            let _ = job.cancel.take().map(|tx| tx.send(()));
            // The job already had its chance to report; the socket is gone.
            let _ = job.handle.await;
        }
        // PACKET 37: await EVERY in-progress teardown (was a single slot —
        // a second cancel detached the first). Completion is guaranteed for
        // all of them; the Vec is bounded by MAX_DRAINING_TEARDOWNS.
        for job in draining.drain(..) {
            let _ = job.handle.await;
        }
    }

    loop {
        // PACKET 37 (audit 12): count draining teardowns too — a node that
        // reports idle while still tearing down gets double-assigned by
        // dispatch. Dispatch treats this as a load signal only (its
        // assignPendingJobs already requeues on timeout), so a transient
        // nonzero count is safe; permanent idleness is impossible because
        // teardowns always complete (drain awaits them).
        let current_job_count =
            u32::from(in_flight.is_some()) + u32::try_from(draining.len()).unwrap_or(u32::MAX);
        tokio::select! {
            // If the quiet-period timer and a jobAssign arrive together, close
            // first. No acceptance was sent, so dispatch simply retains/requeues
            // the job; choosing the frame first would start work at the exact
            // maintenance boundary.
            biased;
            // Graceful shutdown: cancel in-flight job, close socket, exit.
            _ = &mut *shutdown => {
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
                return Ok(Disconnect::Shutdown);
            }
            _ = auto_upgrade_tick.tick(), if config.auto_upgrade.is_some() => {
                let idle = in_flight.is_none()
                    && draining.iter().all(|job| job.handle.is_finished());
                let now = Instant::now();
                auto_upgrade_gate.observe(obs.auto_upgrade_enabled(), idle, now);
                if let Some(version) = auto_upgrade_gate.ready(now) {
                    tracing::info!(%version, "auto-upgrade quiet period reached — closing idle connection");
                    obs.log(LogLine::info(format!(
                        "Auto-upgrade to {version}: idle window reached — disconnecting safely"
                    )));
                    sink.send(Message::Close(None)).await.ok();
                    obs.update_status(|s| s.connection = ConnectionState::Disconnected);
                    // `idle` proved no live job; completed cancel teardowns may
                    // still have owned handles in the Vec, so join them before
                    // handing package-manager work to the caller.
                    drain_in_flight_jobs(&mut in_flight, &mut draining).await;
                    return Ok(Disconnect::AutoUpgrade(version));
                }
            }
            _ = heartbeat.tick() => {
                // PACKET 37 (audit 4): surface un-purged workdirs to the
                // OPERATOR (TUI log pane + daemon log via obs.log), not just
                // tracing. Announced once per path until it clears.
                {
                    let outstanding = crate::purge::outstanding_purge_failures();
                    for failure in &outstanding {
                        if !announced_purge_failures.contains(&failure.path) {
                            announced_purge_failures.push(failure.path.clone());
                            obs.log(LogLine::error(format!(
                                "PURGE INCOMPLETE: {} still holds render content ({}, {} attempts) — the sweep will retry",
                                failure.path.display(),
                                failure.error,
                                failure.attempts
                            )));
                            obs.update_status(|s| {
                                s.jobs_purge_pending = outstanding.len() as u32;
                            });
                        }
                    }
                    if outstanding.is_empty() && !announced_purge_failures.is_empty() {
                        announced_purge_failures.clear();
                        obs.update_status(|s| s.jobs_purge_pending = 0);
                        obs.log(LogLine::info(
                            "All previously failed purges have been reclaimed".to_string(),
                        ));
                    }
                }
                let msg = WorkerMessage::Heartbeat(HeartbeatMessage {
                    tenant: register.tenant.clone(),
                    current_job_count,
                });
                if sink.send(Message::Text(send(msg))).await.is_err() {
                    // PACKET 5: the socket died with a job still in flight —
                    // cancel and drain it BEFORE returning, or the render tree
                    // is stranded live on the operator machine.
                    obs.update_status(|s| s.connection = ConnectionState::Disconnected);
                    drain_in_flight_jobs(&mut in_flight, &mut draining).await;
                    return Ok(Disconnect::Abnormal);
                }
                heartbeats_sent += 1;
                if let Some(limit) = config.heartbeat_limit {
                    if heartbeats_sent >= limit {
                        tracing::info!(heartbeats = heartbeats_sent, "heartbeat limit reached — closing cleanly");
                        obs.log(LogLine::info("Heartbeat limit reached — closing"));
                        sink.send(Message::Close(None)).await.ok();
                        obs.update_status(|s| s.connection = ConnectionState::Disconnected);
                        drain_in_flight_jobs(&mut in_flight, &mut draining).await;
                        return Ok(Disconnect::HeartbeatLimit);
                    }
                }
            }
            Some(msg) = worker_rx.recv() => {
                // C-4: terminal frames carry (job_id, attempt); the attempt
                // rides along so the guard below can match the pair.
                let terminal = match &msg {
                    WorkerMessage::JobComplete(c) => {
                        Some((c.job_id.as_str(), c.attempt))
                    }
                    WorkerMessage::JobFailed(f) => Some((f.job_id.as_str(), f.attempt)),
                    _ => None,
                };
                // A terminal frame for a job dispatch already canceled is the
                // expected outcome of that cancel, not news — dispatch has
                // already marked the job canceled and refunded it. This holds
                // for BOTH terminal shapes:
                //
                // - jobFailed: the runner was killed mid-render.
                // - jobComplete: the runner finished its work (and, since the
                //   runner-core cancel guard, uploaded nothing) but dispatch
                //   canceled the job before the completion was reported. A
                //   completion racing in after cancel must not reach dispatch
                //   either — it would reference an output that was never
                //   settled (dispatch's settle update is scoped to assigned/
                //   rendering) and confuse the job's terminal state.
                //
                // The workdir purge happened in the runner regardless.
                let suppress_after_cancel = match &msg {
                    WorkerMessage::JobFailed(f) => frame_is_for(
                        canceled_job.as_ref().map(|(id, attempt)| (id.as_str(), *attempt)),
                        f.job_id.as_str(),
                        f.attempt,
                    ),
                    WorkerMessage::JobComplete(c) => frame_is_for(
                        canceled_job.as_ref().map(|(id, attempt)| (id.as_str(), *attempt)),
                        c.job_id.as_str(),
                        c.attempt,
                    ),
                    _ => false,
                };
                if let Some((id, attempt)) = terminal {
                    if frame_is_for(
                        in_flight.as_ref().map(|j| (j.job_id.as_str(), j.attempt)),
                        id,
                        attempt,
                    ) {
                        // Cache sweep (2.9): the job's artifacts were just
                        // used — protect them, evict older LRU entries down
                        // to the cap. Runs here, at termination, never on a
                        // timer during a render (packet 9's interference
                        // budget). D-11: blocking I/O — run it on the
                        // blocking pool and await the result (the terminal
                        // frame was already sent; the next heartbeat tick
                        // can wait). D-10: the keys are also REMEMBERED (in
                        // the caller's set) so the next reconnect's startup
                        // sweep protects them too.
                        *protected_keys =
                            in_flight.as_ref().map(|j| j.cache_keys.clone()).unwrap_or_default();
                        let sweep_keys = protected_keys.clone();
                        let sweep = tokio::task::spawn_blocking(move || {
                            crate::cache::sweep_node_caches(&sweep_keys)
                        })
                        .await;
                        match sweep {
                            Ok(Ok(out)) if out.evicted > 0 => {
                                // Surface evictions on the status log (TUI log
                                // pane + daemon log) — packet 20: an operator
                                // should see the cache being reclaimed without
                                // reading tracing output.
                                obs.log(LogLine::info(format!(
                                    "Cache sweep: {} entries, {} -> {} bytes ({} evicted)",
                                    out.entries, out.bytes_before, out.bytes_after, out.evicted
                                )));
                            }
                            Ok(Ok(_)) => {}
                            Ok(Err(e)) => {
                                tracing::warn!(error = %e, "cache sweep after job failed")
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "cache sweep after job join failed")
                            }
                        }
                        in_flight = None;
                    }
                    if canceled_job
                        .as_ref()
                        .map(|(canceled_id, canceled_attempt)| {
                            frame_is_for(
                                Some((canceled_id.as_str(), *canceled_attempt)),
                                id,
                                attempt,
                            )
                        })
                        .unwrap_or(false)
                    {
                        canceled_job = None;
                    }
                }
                if suppress_after_cancel {
                    let (job_id, what) = match &msg {
                        WorkerMessage::JobFailed(f) => (f.job_id.as_str(), "jobFailed"),
                        WorkerMessage::JobComplete(c) => (c.job_id.as_str(), "jobComplete"),
                        _ => unreachable!("suppress_after_cancel only set for terminal frames"),
                    };
                    tracing::info!(
                        job_id = job_id,
                        "render terminal frame after cancel — suppressing {what}"
                    );
                    // C-4: the status pane clears only when nothing is in
                    // flight. If dispatch already re-assigned this job id
                    // as the next attempt, `current_job` IS that attempt —
                    // the suppressed frame belongs to the torn-down one.
                    let idle = in_flight.is_none();
                    obs.update_status(|s| {
                        s.jobs_canceled += 1;
                        if idle && s.current_job.as_ref().is_some_and(|j| j.id == job_id) {
                            s.current_job = None;
                        }
                    });
                    obs.log(LogLine::info(format!(
                        "Job {job_id} render terminal frame after cancel — not reporting {what}"
                    )));
                } else {
                    emit(obs, &msg);
                    if sink.send(Message::Text(send(msg))).await.is_err() {
                        // PACKET 5: drain before returning — see heartbeat arm.
                        obs.update_status(|s| s.connection = ConnectionState::Disconnected);
                        drain_in_flight_jobs(&mut in_flight, &mut draining).await;
                        return Ok(Disconnect::Abnormal);
                    }
                }
            }
            frame = stream.next() => {
                match frame {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<ServerMessage>(text.as_str()) {
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
                                        // Reset synchronously in the same event-loop arm.
                                        // A very short job can start and finish between the
                                        // 250ms maintenance ticks; polling alone would miss it
                                        // and falsely count the interval as continuously idle.
                                        auto_upgrade_gate.mark_busy();
                                        let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
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
                                        // PACKET 37: a failed jobAccepted send is
                                        // a dead socket mid-session. `?` here
                                        // returned straight out of run_session
                                        // WITHOUT draining — and a job may already
                                        // be running below, mid-TERM-grace; the
                                        // dropped handle strands the render tree
                                        // (the exact defect packet 5 fixed for
                                        // every OTHER send site). Same treatment:
                                        // drain, then reconnect.
                                        if sink.send(Message::Text(send(WorkerMessage::JobAccepted(JobAcceptedMessage {
                                            tenant: assign.tenant.clone(),
                                            job_id: assign.job_id.clone(),
                                            attempt: assign.attempt,
                                        })))).await.is_err() {
                                            obs.update_status(|s| s.connection = ConnectionState::Disconnected);
                                            obs.log(LogLine::warn(
                                                "jobAccepted send failed — draining and reconnecting".to_string(),
                                            ));
                                            // No in-flight job yet on THIS path (the
                                            // spawn is below), but drain anyway for
                                            // uniformity and safety against reorders.
                                            drain_in_flight_jobs(&mut in_flight, &mut draining).await;
                                            return Ok(Disconnect::Abnormal);
                                        }
                                        // Transition to rendering phase once the runner starts.
                                        obs.update_status(|s| {
                                            if let Some(job) = &mut s.current_job {
                                                job.phase = JobPhase::Rendering;
                                            }
                                        });
                                        let assign_snapshot = assign.clone();
                                        let handle = tokio::spawn(run_job(
                                            *assign,
                                            cancel_rx,
                                            worker_tx.clone(),
                                            crate::runner::max_job_wall_time(),
                                            crate::runner::max_workdir_bytes(),
                                        ));
                                        // PACKET 5: keep the JoinHandle — every exit path awaits it.
                                        in_flight = Some(InFlightJob {
                                            job_id: job_id_owned,
                                            attempt: assign_snapshot.attempt,
                                            cancel: Some(cancel_tx),
                                            cache_keys: crate::runner::cache_keys_for(&assign_snapshot),
                                            handle,
                                        });
                                    }
                                    ServerMessage::Cancel(cancel)
                                        if in_flight.as_ref().map(|j| j.job_id.as_str())
                                            == Some(cancel.job_id.as_str()) =>
                                    {
                                        if let Some(mut job) = in_flight.take() {
                                            // C-4: record the pair (job, attempt)
                                            // being torn down — `CancelMessage`
                                            // carries no attempt on the wire, so
                                            // the attempt comes from the in-flight
                                            // record; only THAT attempt's terminal
                                            // frame is the cancel's expected outcome.
                                            canceled_job =
                                                Some((job.job_id.clone(), job.attempt));
                                            obs.update_status(|s| {
                                                if let Some(j) = &mut s.current_job {
                                                    j.phase = JobPhase::Canceled;
                                                }
                                            });
                                            let _ = job.cancel.take().map(|tx| tx.send(()));
                                            obs.log(LogLine::warn(format!("Job {} canceled by dispatch", cancel.job_id)));
                                            // PACKET 37 (4c): canceled jobs need the
                                            // post-job cache sweep too — in_flight is
                                            // None from here on, so the terminal-frame
                                            // path below would never run it. Run it
                                            // NOW with this job's cache keys protected
                                            // (same shape as the terminal-frame sweep).
                                            // D-10: remembered for the next reconnect
                                            // sweep as well. D-11: on the blocking pool.
                                            {
                                                *protected_keys = job.cache_keys.clone();
                                                let sweep_keys = protected_keys.clone();
                                                let sweep = tokio::task::spawn_blocking(
                                                    move || crate::cache::sweep_node_caches(&sweep_keys),
                                                )
                                                .await;
                                                match sweep {
                                                    Ok(Ok(out)) if out.evicted > 0 => {
                                                        obs.log(LogLine::info(format!(
                                                            "Cache sweep: {} entries, {} -> {} bytes ({} evicted)",
                                                            out.entries, out.bytes_before, out.bytes_after, out.evicted
                                                        )));
                                                    }
                                                    Ok(Ok(_)) => {}
                                                    Ok(Err(e)) => tracing::warn!(
                                                        error = %e,
                                                        "cache sweep after cancel failed"
                                                    ),
                                                    Err(e) => tracing::warn!(
                                                        error = %e,
                                                        "cache sweep after cancel join failed"
                                                    ),
                                                }
                                            }
                                            // PACKET 5 + 37: keep ownership of the task. The
                                            // terminate sequence runs INSIDE this task;
                                            // dropping the handle here would orphan it
                                            // mid-grace if the socket dies next. Pushed to
                                            // the Vec (was a single slot that a second
                                            // cancel would overwrite — audit 5).
                                            if draining.len() < MAX_DRAINING_TEARDOWNS {
                                                draining.push(job);
                                            } else {
                                                // Over the cap (pathological peer): still
                                                // await the handle — just inline here, the
                                                // completion guarantee is never skipped.
                                                tracing::warn!(
                                                    job_id = %cancel.job_id,
                                                    draining = draining.len(),
                                                    "draining teardowns over cap — awaiting inline"
                                                );
                                                let _ = job.handle.await;
                                            }
                                        }
                                    }
                                    ServerMessage::Cancel(_) => {}
                                    ServerMessage::UpdateAvailable(u) => {
                                        let version = u.supervisor_version.clone();
                                        obs.update_status(|s| {
                                            s.update_available = Some(version.clone());
                                        });
                                        auto_upgrade_gate.announce(version);
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
                                return Ok(Disconnect::UpgradeRequired);
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
                        return Ok(Disconnect::Clean);
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
                        // PACKET 23: this is the fly-deploy shape —
                        // "peer closed connection without sending TLS
                        // close_notify", RST, EOF mid-frame. Session over,
                        // run() reconnects.
                        return Ok(Disconnect::Abnormal);
                    }
                    None => {
                        tracing::info!("socket stream ended");
                        obs.log(LogLine::info("Socket stream ended"));
                        obs.update_status(|s| s.connection = ConnectionState::Disconnected);
                        drain_in_flight_jobs(&mut in_flight, &mut draining).await;
                        return Ok(Disconnect::Clean);
                    }
                }
            }
        }
    }
}

/// PACKET 28: jittered exponential backoff for the disconnect→reconnect
/// loop (packet 23's OWED hardening; flat 1s reconnect = thundering herd
/// when a dispatch redeploy disconnects the whole fleet at once).
///
/// Schedule: attempt n targets `min(max, base·2^(n−1))`; the actual delay
/// is uniformly random in [0, target] — FULL jitter (AWS "Exponential
/// Backoff and Jitter", full-jitter variant). Full rather than equal
/// jitter deliberately: a fleet reconnecting after a simultaneous
/// redeploy needs the retries SPREAD across the whole window [0, target],
/// not clustered near target the way equal jitter's [target/2, target]
/// does — the herd is densest at the low end right after a restart, and
/// full jitter thins it maximally. Cost: full jitter occasionally
/// reconnects near 0s; that is fine — dispatch accepting a connection is
/// the success signal, and the next failure re-enters the curve.
///
/// RNG: a xorshift64 seeded from the system clock (no `rand` dependency
/// for one uniform draw; xorshift64 is a well-studied fast PRNG with a
/// 2^64−1 period, plenty for reconnect timing). Tests inject a canned
/// `f64` source for determinism — no pigeonhole `nanos % n` jitter.
///
/// Reset rule: a session that stayed connected ≥ `max` is healthy, so the
/// next disconnect starts a FRESH exponential cycle (a node that held for
/// a full cap-window before dying is not in a crash loop).
struct ReconnectBackoff {
    base: Duration,
    max: Duration,
    /// Consecutive reconnect attempts without a healthy session.
    attempt: u32,
    /// Uniform source in [0, 1); production seeds xorshift64 from
    /// SystemTime; tests inject a canned value.
    rng: Box<dyn FnMut() -> f64 + Send>,
}

impl ReconnectBackoff {
    fn new(base: Duration, max: Duration) -> Self {
        let seed = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9E3779B97F4A7C15)
            | 1; // xorshift64 state must be nonzero
        let mut state = seed;
        ReconnectBackoff {
            base,
            max,
            attempt: 0,
            rng: Box::new(move || {
                // xorshift64*
                state ^= state >> 12;
                state ^= state << 25;
                state ^= state >> 27;
                ((state.wrapping_mul(0x2545F4914F6CDD1D)) >> 11) as f64 / (1u64 << 53) as f64
            }),
        }
    }

    #[cfg(test)]
    fn with_canned_rng(
        base: Duration,
        max: Duration,
        canned: impl FnMut() -> f64 + Send + 'static,
    ) -> Self {
        ReconnectBackoff {
            base,
            max,
            attempt: 0,
            rng: Box::new(canned),
        }
    }

    /// The exponential target for the upcoming attempt (pre-jitter),
    /// capped at `max`.
    fn target(&self) -> Duration {
        // base·2^(attempt) — saturating so a pathological attempt count
        // cannot overflow (and the cap binds first anyway).
        // Duration::saturating_mul takes u32; cap the shift so the
        // factor fits (2^32 ≈ 4.3e9 × any sane base already exceeds the
        // 60s cap long before attempt 32).
        let shift = self.attempt.min(31);
        let factor = 1u32.checked_shl(shift).unwrap_or(u32::MAX);
        let grown = self.base.saturating_mul(factor);
        grown.min(self.max)
    }

    /// Next delay: full jitter in [0, target], target ≤ max. The attempt
    /// counter advances here (this call IS the retry).
    fn next_delay(&mut self) -> Duration {
        let target = self.target();
        self.attempt = self.attempt.saturating_add(1);
        let u = (self.rng)().clamp(0.0, 1.0);
        target.mul_f64(u)
    }

    /// Record how long the last session stayed connected; ≥ max resets
    /// the curve (healthy node → fresh exponential next time).
    fn session_lasted(&mut self, connected_for: Duration) {
        if connected_for >= self.max {
            self.attempt = 0;
        }
    }
}

/// PACKET 23: the process-level entry point callers use.
///
/// Wraps [`run_session`] in the reconnect loop: any non-terminal
/// disconnect (abnormal TLS abort, RST, clean close, stream end) sleeps
/// `connect_retry_delay` and starts a fresh session, reusing the SAME
/// initial-connect attempt budget each cycle (a disconnect means the node
/// was reachable before; the budget resets because a redeployed dispatch
/// is expected back within seconds, not to be counted against a 15-attempt
/// cold-start allowance). Shutdown, heartbeat-limit and upgrade-required
/// end the run; `Err` from a session's initial connect (dispatch fully
/// unreachable for `max_connect_attempts`) ends it too, preserving the
/// exit-1 contract launchd/systemd restart policies rely on.
pub async fn run(
    config: &ConnectionConfig,
    register: &RegisterMessage,
    obs: &Observability,
    shutdown: oneshot::Receiver<()>,
) -> anyhow::Result<()> {
    run_until_exit(config, register, obs, shutdown)
        .await
        .map(|_| ())
}

/// Variant of [`run`] for process owners that need to act on a terminal
/// lifecycle handoff (currently the installed CLI daemon's auto-upgrader).
/// Existing embedders keep the historical `run() -> Result<()>` API.
pub async fn run_until_exit(
    config: &ConnectionConfig,
    register: &RegisterMessage,
    obs: &Observability,
    mut shutdown: oneshot::Receiver<()>,
) -> anyhow::Result<ConnectionExit> {
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
        s.auto_upgrade_enabled = obs.auto_upgrade_enabled();
        s.last_error = None;
        // Optimistic: assume up to date until dispatch says otherwise. Cleared
        // on every connect so a freshly-upgraded node stops showing a stale
        // "update available" once it matches the latest.
        s.update_available = None;
    });

    // PACKET 28: jittered exponential backoff replaces the flat
    // connect_retry_delay here (packet 23's OWED: a dispatch redeploy
    // disconnects the whole fleet at once; flat 1s reconnects = thundering
    // herd). The initial-connect retry loop INSIDE a session keeps the
    // flat delay — launchd/systemd own cold-start cadence.
    let mut backoff =
        ReconnectBackoff::new(config.reconnect_backoff_base, config.reconnect_backoff_max);

    // D-10: the cache keys of the jobs this supervisor has seen, remembered
    // ACROSS sessions. The startup sweep of every (re)connect protects them
    // instead of sweeping blind; empty until the first job's terminal/cancel
    // sweep fills it.
    let mut last_protected: Vec<String> = Vec::new();

    loop {
        let session_start = std::time::Instant::now();
        match run_session(
            config,
            &request,
            register,
            obs,
            &mut shutdown,
            &mut last_protected,
        )
        .await
        {
            Ok(Disconnect::Shutdown) => return Ok(ConnectionExit::Shutdown),
            Ok(Disconnect::HeartbeatLimit) => return Ok(ConnectionExit::HeartbeatLimit),
            Ok(Disconnect::UpgradeRequired) => return Ok(ConnectionExit::UpgradeRequired),
            Ok(Disconnect::AutoUpgrade(version)) => {
                return Ok(ConnectionExit::AutoUpgrade { version });
            }
            Ok(disconnect @ (Disconnect::Abnormal | Disconnect::Clean)) => {
                if !config.reconnect {
                    return Ok(ConnectionExit::Disconnected);
                }
                // A session that held ≥ the cap-window was healthy: start
                // a fresh exponential cycle. A short one keeps climbing.
                backoff.session_lasted(session_start.elapsed());
                let retry_delay = backoff.next_delay();
                tracing::warn!(
                    reason = ?disconnect,
                    retry_delay_ms = retry_delay.as_millis(),
                    backoff_attempt = backoff.attempt,
                    "disconnected from dispatch — reconnecting"
                );
                obs.log(LogLine::warn("Dispatch disconnected — reconnecting…"));
                obs.update_status(|s| s.connection = ConnectionState::Reconnecting);
                tokio::select! {
                    biased;
                    _ = &mut shutdown => {
                        tracing::info!("shutdown signal during reconnect backoff — exiting");
                        obs.update_status(|s| s.connection = ConnectionState::Disconnected);
                        return Ok(ConnectionExit::Shutdown);
                    }
                    _ = tokio::time::sleep(retry_delay) => {}
                }
            }
            Err(e) => return Err(e),
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
mod tests;
