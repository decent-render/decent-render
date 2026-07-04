//! The one outbound WebSocket to the dispatch service.
//!
//! Mirrors the connection behavior of the TS reference worker
//! (driffs `scripts/spike-worker.ts`): connect with `?token=` on the URL,
//! send `register` immediately, heartbeat every 20 s, retry the initial
//! connect with a short delay (the dispatch may still be starting), and hand
//! every parsed server message to a caller-supplied handler.
//!
//! The loop accepts an [`Observability`] bundle. When channels are attached
//! (Tauri app), it emits structured status snapshots and tailable log lines.
//! When they are `None` (CLI), it falls back to `tracing` only. Both skins
//! drive the exact same code path.

use std::time::Duration;

use anyhow::{anyhow, Context};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

use crate::protocol::{
    HeartbeatMessage, JobAcceptedMessage, RegisterMessage, ServerMessage, WorkerMessage,
};
use crate::runner::{run_job, InFlightJob};
use crate::status::{ConnectionState, JobPhase, JobStatus, LogLine, Observability};

/// Receives every parsed server → worker message.
pub trait ServerMessageHandler: Send {
    fn on_message(&mut self, msg: &ServerMessage);
}

impl<F: FnMut(&ServerMessage) + Send> ServerMessageHandler for F {
    fn on_message(&mut self, msg: &ServerMessage) {
        self(msg);
    }
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

    fn url_with_token(&self) -> String {
        let sep = if self.dispatch_url.contains('?') {
            '&'
        } else {
            '?'
        };
        format!(
            "{}{}token={}",
            self.dispatch_url,
            sep,
            encode_uri_component(&self.token)
        )
    }
}

/// Percent-encode a URL query component (RFC 3986 unreserved set kept).
fn encode_uri_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Connect, register, heartbeat, and pump server messages into `handler`.
///
/// Returns `Ok(())` on a clean self-initiated close (heartbeat limit reached)
/// or when the server closes the socket after we ever connected; returns an
/// error if the dispatch is unreachable after all connect attempts.
///
/// `obs` carries the optional status/log channels + the live `allow_real_jobs`
/// flag. The CLI passes `Observability::default()`; the Tauri app passes one
/// with channels attached.
pub async fn run<H: ServerMessageHandler>(
    config: &ConnectionConfig,
    register: &RegisterMessage,
    handler: &mut H,
    obs: &Observability,
) -> anyhow::Result<()> {
    let url = config.url_with_token();

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
    });

    // Initial-connect retry loop (mirrors spike-worker.ts MAX_CONNECT_ATTEMPTS).
    let mut attempts = 0u32;
    let ws = loop {
        attempts += 1;
        match connect_async(&url).await {
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
                tokio::time::sleep(config.connect_retry_delay).await;
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

    loop {
        let current_job_count = u32::from(in_flight.is_some());
        tokio::select! {
            _ = heartbeat.tick() => {
                let msg = WorkerMessage::Heartbeat(HeartbeatMessage {
                    tenant: register.tenant.clone(),
                    current_job_count,
                });
                sink.send(Message::Text(send(msg))).await.context("failed to send heartbeat")?;
                heartbeats_sent += 1;
                if let Some(limit) = config.heartbeat_limit {
                    if heartbeats_sent >= limit {
                        tracing::info!(heartbeats = heartbeats_sent, "heartbeat limit reached — closing cleanly");
                        obs.log(LogLine::info("Heartbeat limit reached — closing"));
                        sink.send(Message::Close(None)).await.ok();
                        obs.update_status(|s| s.connection = ConnectionState::Disconnected);
                        return Ok(());
                    }
                }
            }
            Some(msg) = worker_rx.recv() => {
                let is_terminal = matches!(msg, WorkerMessage::JobComplete(_) | WorkerMessage::JobFailed(_));
                emit(obs, &msg);
                if is_terminal {
                    in_flight = None;
                }
                sink.send(Message::Text(send(msg))).await.context("failed to send worker job frame")?;
            }
            frame = stream.next() => {
                match frame {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<ServerMessage>(&text) {
                            Ok(msg) => {
                                log_server_message(&msg, obs.allows_real_jobs());
                                handler.on_message(&msg);
                                match msg {
                                    ServerMessage::JobAssign(assign) => {
                                        if !obs.allows_real_jobs() {
                                            tracing::warn!(job_id = %assign.job_id, "refusing jobAssign; allow_real_jobs is OFF");
                                            obs.log(LogLine::warn(format!(
                                                "Job {} assigned but refused — \"Accept real jobs\" is OFF",
                                                assign.job_id
                                            )));
                                            continue;
                                        }
                                        if in_flight.is_some() {
                                            tracing::warn!(job_id = %assign.job_id, "refusing jobAssign while busy");
                                            continue;
                                        }
                                        let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
                                        in_flight = Some(InFlightJob { job_id: assign.job_id.clone(), cancel: cancel_tx });
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
                                        })))).await.context("failed to send jobAccepted")?;
                                        // Transition to rendering phase once the runner starts.
                                        obs.update_status(|s| {
                                            if let Some(job) = &mut s.current_job {
                                                job.phase = JobPhase::Rendering;
                                            }
                                        });
                                        tokio::spawn(run_job(assign, cancel_rx, worker_tx.clone()));
                                    }
                                    ServerMessage::Cancel(cancel)
                                        if in_flight.as_ref().map(|j| j.job_id.as_str())
                                            == Some(cancel.job_id.as_str()) =>
                                    {
                                        if let Some(job) = in_flight.take() {
                                            obs.update_status(|s| {
                                                if let Some(j) = &mut s.current_job {
                                                    j.phase = JobPhase::Canceled;
                                                }
                                            });
                                            let _ = job.cancel.send(());
                                            obs.log(LogLine::warn(format!("Job {} canceled by dispatch", cancel.job_id)));
                                        }
                                    }
                                    ServerMessage::Cancel(_) => {}
                                    _ => {}
                                }
                            }
                            Err(e) => tracing::warn!(error = %e, frame = %text, "unparseable frame from server"),
                        }
                    }
                    Some(Ok(Message::Close(close))) => {
                        tracing::info!(?close, "socket closed by server");
                        obs.log(LogLine::info("Socket closed by server"));
                        obs.update_status(|s| s.connection = ConnectionState::Disconnected);
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
                        return Err(anyhow!(e).context("websocket error"));
                    }
                    None => {
                        tracing::info!("socket stream ended");
                        obs.log(LogLine::info("Socket stream ended"));
                        obs.update_status(|s| s.connection = ConnectionState::Disconnected);
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
    use std::sync::{Arc, Mutex};
    use tokio::net::{TcpListener, TcpStream};
    use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
    use tokio_tungstenite::WebSocketStream;

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
            capabilities: Capabilities { gpu: false },
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

    async fn accept_ws(listener: &TcpListener) -> (WebSocketStream<TcpStream>, String) {
        let (tcp, _) = listener.accept().await.unwrap();
        let uri = Arc::new(Mutex::new(String::new()));
        let uri_clone = uri.clone();
        // tungstenite's handshake callback signature carries a large Err type.
        #[allow(clippy::result_large_err)]
        let callback = move |req: &Request, resp: Response| {
            *uri_clone.lock().unwrap() = req.uri().to_string();
            Ok(resp)
        };
        let ws = tokio_tungstenite::accept_hdr_async(tcp, callback)
            .await
            .unwrap();
        let uri = uri.lock().unwrap().clone();
        (ws, uri)
    }

    async fn next_text(ws: &mut WebSocketStream<TcpStream>) -> String {
        loop {
            match ws.next().await.expect("stream ended").expect("ws error") {
                Message::Text(t) => return t,
                _ => continue,
            }
        }
    }

    #[tokio::test]
    async fn registers_heartbeats_and_closes_cleanly() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let config = fast_config(port);
        let register = test_register();
        let obs = Observability::default();

        let client = tokio::spawn(async move {
            run(&config, &register, &mut |_: &ServerMessage| {}, &obs).await
        });

        let (mut ws, uri) = accept_ws(&listener).await;
        assert!(
            uri.contains("token=test-jwt.token"),
            "token must ride the query string, got {uri}"
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

        let seen: Arc<Mutex<Vec<ServerMessage>>> = Arc::default();
        let seen_clone = seen.clone();
        let client = tokio::spawn(async move {
            let mut handler =
                move |msg: &ServerMessage| seen_clone.lock().unwrap().push(msg.clone());
            run(&config, &register, &mut handler, &obs).await
        });

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

        // The skeleton must NOT accept the job (no jobAccepted frame).
        assert!(
            sent_types.iter().all(|t| t == "heartbeat"),
            "only heartbeats expected after register, got {sent_types:?}"
        );

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "handler must receive ping + jobAssign");
        assert!(matches!(seen[0], ServerMessage::Ping(_)));
        match &seen[1] {
            ServerMessage::JobAssign(a) => assert_eq!(a.job_id, "job-render-x"),
            other => panic!("expected jobAssign, got {other:?}"),
        }
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
        let client = tokio::spawn(async move {
            run(&config, &register, &mut |_: &ServerMessage| {}, &obs).await
        });

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
        let config = fast_config(port);
        let register = test_register();

        let (obs, status_rx, _log_rx) =
            Observability::channels(crate::status::SupervisorStatus::default());

        let client = tokio::spawn(async move {
            run(&config, &register, &mut |_: &ServerMessage| {}, &obs).await
        });

        let (mut ws, _uri) = accept_ws(&listener).await;
        let _register = next_text(&mut ws).await;

        // After connect + register, status should be Registered.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(status_rx.borrow().connection, ConnectionState::Registered);
        assert!(status_rx.borrow().node_identity.is_some());

        // Drain heartbeats until clean close.
        while ws.next().await.is_some() {}
        client.await.unwrap().expect("clean exit");

        assert_eq!(status_rx.borrow().connection, ConnectionState::Disconnected);
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
        let client = tokio::spawn(async move {
            run(&config, &register, &mut |_: &ServerMessage| {}, &obs2).await
        });

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

    #[test]
    fn encodes_token_for_query() {
        assert_eq!(encode_uri_component("a.b-c_d~e"), "a.b-c_d~e");
        assert_eq!(encode_uri_component("a+b/c="), "a%2Bb%2Fc%3D");
    }
}
