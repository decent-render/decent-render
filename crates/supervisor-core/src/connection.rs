//! The one outbound WebSocket to the dispatch service.
//!
//! Mirrors the connection behavior of the TS reference worker
//! (driffs `scripts/spike-worker.ts`): connect with `?token=` on the URL,
//! send `register` immediately, heartbeat every 20 s, retry the initial
//! connect with a short delay (the dispatch may still be starting), and hand
//! every parsed server message to a caller-supplied handler.
//!
//! Job EXECUTION is out of scope for this crate version: on `jobAssign` the
//! loop logs the assignment and does nothing — it does NOT send `jobAccepted`,
//! so the job requeues when this worker disconnects.

use std::time::Duration;

use anyhow::{anyhow, Context};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

use crate::protocol::{HeartbeatMessage, RegisterMessage, ServerMessage, WorkerMessage};

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
pub async fn run<H: ServerMessageHandler>(
    config: &ConnectionConfig,
    register: &RegisterMessage,
    handler: &mut H,
) -> anyhow::Result<()> {
    let url = config.url_with_token();

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
                tokio::time::sleep(config.connect_retry_delay).await;
            }
            Err(e) => {
                return Err(e).with_context(|| {
                    format!(
                        "failed to connect to dispatch after {} attempts",
                        config.max_connect_attempts
                    )
                });
            }
        }
    };
    tracing::info!(url = %config.dispatch_url, "connected to dispatch");

    let (mut sink, mut stream) = ws.split();

    let send = |msg: WorkerMessage| {
        let frame = serde_json::to_string(&msg).expect("worker messages always serialize");
        tracing::info!(frame = %frame, "→ send");
        frame
    };

    sink.send(Message::Text(send(WorkerMessage::Register(
        register.clone(),
    ))))
    .await
    .context("failed to send register")?;

    // First heartbeat one full interval after register, then periodic.
    let mut heartbeat = tokio::time::interval_at(
        tokio::time::Instant::now() + config.heartbeat_interval,
        config.heartbeat_interval,
    );
    let mut heartbeats_sent = 0u32;
    // No job execution in this crate version — the count is always 0.
    let current_job_count = 0u32;

    loop {
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
                        sink.send(Message::Close(None)).await.ok();
                        return Ok(());
                    }
                }
            }
            frame = stream.next() => {
                match frame {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<ServerMessage>(&text) {
                            Ok(msg) => {
                                log_server_message(&msg);
                                handler.on_message(&msg);
                            }
                            Err(e) => tracing::warn!(error = %e, frame = %text, "unparseable frame from server"),
                        }
                    }
                    Some(Ok(Message::Close(close))) => {
                        tracing::info!(?close, "socket closed by server");
                        return Ok(());
                    }
                    // tungstenite answers Ping frames automatically.
                    Some(Ok(_)) => {}
                    Some(Err(e)) => return Err(anyhow!(e).context("websocket error")),
                    None => {
                        tracing::info!("socket stream ended");
                        return Ok(());
                    }
                }
            }
        }
    }
}

fn log_server_message(msg: &ServerMessage) {
    match msg {
        ServerMessage::JobAssign(assign) => {
            tracing::warn!(
                job_id = %assign.job_id,
                kind = ?assign.kind,
                frames = assign.duration_frames,
                "← jobAssign — job execution not implemented in this supervisor version; \
                 NOT accepting (job requeues on disconnect)"
            );
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

        let client =
            tokio::spawn(async move { run(&config, &register, &mut |_: &ServerMessage| {}).await });

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

        let seen: Arc<Mutex<Vec<ServerMessage>>> = Arc::default();
        let seen_clone = seen.clone();
        let client = tokio::spawn(async move {
            let mut handler =
                move |msg: &ServerMessage| seen_clone.lock().unwrap().push(msg.clone());
            run(&config, &register, &mut handler).await
        });

        let (mut ws, _uri) = accept_ws(&listener).await;
        let _register = next_text(&mut ws).await;

        ws.send(Message::Text(r#"{"type":"ping","tenant":"driffs"}"#.into()))
            .await
            .unwrap();
        ws.send(Message::Text(
            r#"{"type":"jobAssign","tenant":"driffs","jobId":"job-render-x","kind":"gpu","durationFrames":10,"fps":30,"codec":"h264","bundleSha256":"s","bundleGetUrl":"u","inputPropsGetUrl":"u","assetGetUrls":[],"outputPutUrl":"u","outputKey":"k","purgeAfter":true}"#.into(),
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
        let client =
            tokio::spawn(async move { run(&config, &register, &mut |_: &ServerMessage| {}).await });

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

    #[test]
    fn encodes_token_for_query() {
        assert_eq!(encode_uri_component("a.b-c_d~e"), "a.b-c_d~e");
        assert_eq!(encode_uri_component("a+b/c="), "a%2Bb%2Fc%3D");
    }
}
