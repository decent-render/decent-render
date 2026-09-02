// ── PACKET 40 (audit-api-ux): 401 ≠ unreachable ─────────────────────────────

/// A TCP listener that completes the TCP+HTTP exchange but answers the
/// websocket upgrade with an HTTP 401 — the shape dispatch returns for a
/// bad/revoked token.
async fn serve_401() -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = vec![0u8; 4096];
                let _ = socket.read(&mut buf).await; // the upgrade request
                let resp =
                    "HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
                let _ = socket.write_all(resp.as_bytes()).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    addr
}

#[tokio::test]
async fn an_http_401_at_connect_is_an_auth_failure_not_unreachable() {
    let addr = serve_401().await;
    let config = ConnectionConfig {
        max_connect_attempts: 20, // a full retry ladder — the test asserts we DON'T climb it
        connect_retry_delay: Duration::from_millis(200),
        ..ConnectionConfig::new(format!("ws://{addr}/ws"), "test-jwt.token")
    };
    let register = test_register();
    let obs = Observability::default();
    let started = std::time::Instant::now();
    let err = run(&config, &register, &obs, never_shutdown())
        .await
        .expect_err("401 must fail the run");
    // FAST: no retry ladder against an auth rejection. The ladder here is
    // 20 attempts × 200ms ≈ 4s+; the auth arm fails on attempt 1 (~0s).
    // (Verified by mutation: with the auth arm disabled this test took
    // 3.85s and failed these assertions.)
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "auth rejection must fail fast, took {:?}",
        started.elapsed()
    );
    let msg = format!("{err:#}");
    assert!(
        msg.contains("credentials"),
        "must name the credential cause (only the auth arm says credentials), got: {msg}"
    );
    assert!(msg.contains("401"), "must name the status, got: {msg}");
    assert!(
        !msg.to_lowercase().contains("unreachable"),
        "must NOT be reported as unreachable, got: {msg}"
    );
}

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

/// Config with no heartbeat limit — one session, ending at the first
/// disconnect (reconnect: false) so teardown-pinning tests observe
/// run() return. Reconnection itself is pinned by the two dedicated
/// reconnect tests (which set reconnect: true explicitly).
fn long_config(port: u16) -> ConnectionConfig {
    ConnectionConfig {
        heartbeat_interval: Duration::from_millis(50),
        max_connect_attempts: 20,
        connect_retry_delay: Duration::from_millis(50),
        heartbeat_limit: None,
        reconnect: false,
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

async fn accept_ws_capturing(listener: &TcpListener) -> (WebSocketStream<TcpStream>, Handshake) {
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
            Message::Text(t) => return t.as_str().to_owned(),
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
        let root = std::env::temp_dir().join(format!("decent-test-worker-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("test worker root");
        crate::runner::set_worker_root_for_tests(root.clone());
        root
    })
    .clone()
}

/// A job that never goes quiet must still be stoppable.
///
/// `SILENCE_TIMEOUT` only catches a runner that stops talking. A render
/// that keeps reporting progress resets that timer forever, so before the
/// wall-clock ceiling a pathological composition could hold a node
/// indefinitely — and dispatch would not intervene, because from its side
/// the job is progressing perfectly normally.
///
/// Drives `run_job` directly with a short limit rather than going through
/// the WebSocket: the limit is a parameter precisely so this test cannot
/// shorten the 10s cancel-grace tests running in parallel beside it.
#[cfg(unix)]
#[tokio::test]
async fn wall_clock_limit_kills_a_job_that_never_goes_silent() {
    let pid = std::process::id();
    let sha = format!("test-wallclock-{pid}");
    let job_id = format!("job-wallclock-{pid}");
    let pid_file = std::env::temp_dir().join(format!("{job_id}.runner-pid"));
    let _ = std::fs::remove_file(&pid_file);

    // Chatty forever: progress every 100ms. The silence timer can never
    // fire, so only the wall-clock ceiling can end this.
    let payload_script = format!(
        r#"#!/bin/sh
echo $$ > "{pidfile}"
while true; do
  echo '{{"type":"progress","progress":0.5}}'
  sleep 0.1
done
"#,
        pidfile = pid_file.display()
    );
    let payload_dir = seed_fake_payload(&sha, &payload_script);

    let assign: crate::protocol::JobAssignMessage =
        serde_json::from_str(&job_assign_json(&job_id, &sha)).unwrap();
    // Bound, not dropped: dropping the sender closes the channel, which
    // resolves the cancel receiver immediately and would end the job as a
    // cancel rather than a timeout.
    let (_cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

    let t0 = std::time::Instant::now();
    // 1500ms wall clock with a disk cap far out of reach: only the
    // wall-clock path may end this. (Exchanged one line for five — the
    // disk cap parameter must not silently neuter the original test.)
    // Gate on the runner REALLY running before the wall-clock can end
    // the job: its own pid file, written by itself. Under full-suite
    // parallel load, fork+exec+first-shell-line can exceed a tight wall
    // budget (proven packet 12: the payload's very first
    // `echo $$ > pidfile` never ran while run_job duly reported the
    // wall-clock limit) — so the test must not assert on a runner that
    // was never scheduled. run_job runs concurrently with the gate
    // because IT is what spawns the runner.
    let job = tokio::spawn(crate::runner::run_job(
        assign,
        cancel_rx,
        tx,
        // Generous budget so only a genuinely-unschedulable runner could
        // hit it before the gate below resolves: this test may not
        // assume the machine is idle.
        Duration::from_secs(4),
        u64::MAX / 2,
    ));
    tokio::time::timeout(Duration::from_secs(10), wait_for_pid_file(&pid_file))
        .await
        .expect("runner never started within 10s — spawn path broken, not a wall-clock race");
    job.await.expect("run_job task panicked");
    let ran_for = t0.elapsed().as_secs_f64();

    // The job must have been allowed to run past the gate before the
    // limit ended it — a spawn failure or instant death returns in
    // milliseconds and would fail this floor.
    assert!(
        ran_for >= 1.0,
        "job ended after {ran_for:.2}s — the wall-clock path was not exercised"
    );

    let mut reason = None;
    while let Ok(msg) = rx.try_recv() {
        if let WorkerMessage::JobFailed(failed) = msg {
            reason = Some(failed.reason);
        }
    }
    let reason = reason.expect("a timed-out job must report jobFailed");
    assert!(
        reason.contains("wall-clock limit"),
        "unexpected failure reason: {reason}"
    );

    let runner_pid: u32 = std::fs::read_to_string(&pid_file)
        .expect("runner recorded its pid")
        .trim()
        .parse()
        .expect("pid parses");
    assert!(
        unsafe { libc::kill(runner_pid as libc::pid_t, 0) } != 0,
        "runner survived the wall-clock limit"
    );
    assert!(
        job_workdirs(&job_id).is_empty(),
        "workdir survived the wall-clock limit"
    );

    let _ = std::fs::remove_file(&pid_file);
    std::fs::remove_dir_all(&payload_dir).ok();
}

/// A render that fills the disk takes down the WHOLE node, not just its
/// own job — every other job, the supervisor, anything else on the box.
/// The wall-clock cap bounds time; this bounds disk, sampled DURING the
/// render (a check at the end sees the damage already done).
///
/// Same shape as the wall-clock test above: drives `run_job` directly so
/// the small cap cannot leak into the parallel tests, against a runner
/// that stays chatty (progress every 100ms) so neither the silence timer
/// nor any spawn failure can be what ends it.
#[cfg(unix)]
#[tokio::test]
async fn workdir_disk_cap_kills_a_runaway_render() {
    let pid = std::process::id();
    let sha = format!("test-diskcap-{pid}");
    let job_id = format!("job-diskcap-{pid}");
    let pid_file = std::env::temp_dir().join(format!("{job_id}.runner-pid"));
    let _ = std::fs::remove_file(&pid_file);

    // Chatty forever AND writing: 4 MiB per 100ms into the workdir. The
    // silence timer can never fire; the workdir crosses a 4 MiB cap
    // within the first sample interval or two.
    let payload_script = format!(
        r#"#!/bin/sh
echo $$ > "{pidfile}"
i=0
while true; do
  dd if=/dev/zero of="blob-$i" bs=1048576 count=4 2>/dev/null
  echo '{{"type":"progress","progress":0.5}}'
  i=$((i+1))
  sleep 0.1
done
"#,
        pidfile = pid_file.display()
    );
    let payload_dir = seed_fake_payload(&sha, &payload_script);

    let assign: crate::protocol::JobAssignMessage =
        serde_json::from_str(&job_assign_json(&job_id, &sha)).unwrap();
    let (_cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

    let t0 = std::time::Instant::now();
    // 4 MiB cap (the production floor for the override) and a wall clock
    // generous enough that only the disk cap may end it.
    crate::runner::run_job(
        assign,
        cancel_rx,
        tx,
        Duration::from_secs(3600),
        4 * 1024 * 1024,
    )
    .await;
    let ran_for = t0.elapsed().as_secs_f64();

    // The job must have run long enough for the SAMPLER to fire at least
    // once past its first (empty-workdir) tick — not died instantly.
    assert!(
        ran_for >= 1.0,
        "job ended after {ran_for:.2}s — the disk-cap path was not exercised"
    );

    let mut reason = None;
    while let Ok(msg) = rx.try_recv() {
        if let WorkerMessage::JobFailed(failed) = msg {
            reason = Some(failed.reason);
        }
    }
    let reason = reason.expect("a disk-capped job must report jobFailed");
    assert!(
        reason.contains("disk cap"),
        "unexpected failure reason: {reason}"
    );

    let runner_pid: u32 = std::fs::read_to_string(&pid_file)
        .expect("runner recorded its pid")
        .trim()
        .parse()
        .expect("pid parses");
    assert!(
        unsafe { libc::kill(runner_pid as libc::pid_t, 0) } != 0,
        "runner survived the disk cap"
    );
    assert!(
        job_workdirs(&job_id).is_empty(),
        "workdir survived the disk cap"
    );

    let _ = std::fs::remove_file(&pid_file);
    std::fs::remove_dir_all(&payload_dir).ok();
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

/// Packet 17: a real job through the WS loop must run the post-
/// termination cache sweep — the LRU cache enforced after complete/
/// fail/cancel, protecting the finished job's own artifacts.
///
/// Seeds the redirected test worker root (NOT live ~/.decent-worker —
/// the 916a02e seam) with a stale browser entry and the payload the
/// job uses. With the default 20 GiB cap the sweep runs but evicts
/// nothing, so the honest wiring assertions are: the job completes,
/// both sweeps ran without deleting anything under-cap, and the job's
/// own payload survives (used moments ago). Eviction-ORDERING proofs
/// live in the cache unit tests; the env override cannot be set from
/// this parallel test binary — exactly why the cap is parameterized
/// (34c74f1 pattern).
#[cfg(unix)]
#[tokio::test]
async fn job_termination_runs_the_cache_sweep() {
    let root = test_worker_root();
    let pid = std::process::id();
    let sha = format!("test-cachesweep-{pid}");
    let job_id = format!("job-cachesweep-{pid}");

    // An ancient stale browser entry (marker epoch 2023) plus the
    // payload the job uses — both under the 20 GiB default cap.
    let stale = root.join("browsers").join(format!("stale-{pid}"));
    std::fs::create_dir_all(&stale).unwrap();
    std::fs::write(stale.join("executable"), "stale").unwrap();
    std::fs::write(stale.join(".last-use"), "1700000000").unwrap();
    std::fs::write(stale.join("filler"), vec![0u8; 1024]).unwrap();

    let payload_dir = seed_fake_payload(
        &sha,
        r#"#!/bin/sh
echo '{"type":"done","outputSizeInBytes":1,"wallTimeMs":1}'
"#,
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

    ws.send(Message::Text(Utf8Bytes::from(job_assign_json(
        &job_id, &sha,
    ))))
    .await
    .unwrap();

    // The startup sweep ran during connect (before the assign); the
    // post-termination sweep runs when the done frame is processed.
    tokio::time::timeout(
        Duration::from_secs(20),
        status_rx.wait_for(|s| s.jobs_completed == 1 && s.current_job.is_none()),
    )
    .await
    .expect("job did not complete in time")
    .expect("status channel closed");

    // Sweep wiring proof: the stale entry was ACCOUNTED (startup sweep
    // ran, saw it, kept it — under the default cap). If no sweep had
    // run at all this assertion is vacuous, so also assert the payload
    // the job used still exists (protected + used moments ago).
    assert!(stale.exists(), "under-cap entries survive both sweeps");
    assert!(
        root.join("payloads").join(&sha).exists(),
        "the completed job's payload survives (in-use protection + recency)"
    );

    ws.close(None).await.ok();
    while let Some(Ok(_)) = ws.next().await {}
    client.await.unwrap().expect("clean exit");
    let _ = std::fs::remove_dir_all(payload_dir);
    let _ = std::fs::remove_dir_all(&stale);
}

/// D-10 wiring: a RECONNECT session's startup cache sweep receives the
/// last job's cache keys — visible in the sweep's operator log line
/// ("N protected"). A first-connect session (no job seen yet) stays
/// silent, as before. The eviction SEMANTICS of a protected set are
/// pinned by cache.rs's sweep_caches unit tests; this pins the
/// connection-side wiring: after a job ran, the reconnect sweep is NOT
/// handed an empty set. (An end-to-end over-cap eviction variant of this
/// test was tried first and retired: tripping the 1 GiB floor cap from a
/// parallel test binary required an env window around the sweep, and the
/// window's sweeps raced the kill tests' fixtures — see packet 56
/// receipt, OWED.)
#[cfg(unix)]
#[tokio::test]
async fn reconnect_hands_the_startup_sweep_the_last_jobs_keys() {
    // Install the redirected worker root (first-write-wins): the sweeps
    // in this test must never see the real ~/.decent-worker.
    let _root = test_worker_root();
    let pid = std::process::id();
    let sha = format!("test-reconnkeys-{pid}");
    let job_id = format!("job-reconnkeys-{pid}");

    let payload_dir = seed_fake_payload(
        &sha,
        r#"#!/bin/sh
echo '{"type":"done","outputSizeInBytes":1,"wallTimeMs":1}'
"#,
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let config = ConnectionConfig {
        heartbeat_interval: Duration::from_millis(50),
        max_connect_attempts: 20,
        connect_retry_delay: Duration::from_millis(50),
        heartbeat_limit: None,
        reconnect: true,
        reconnect_backoff_base: Duration::from_millis(50),
        reconnect_backoff_max: Duration::from_secs(1),
        ..ConnectionConfig::new(format!("ws://127.0.0.1:{port}/ws"), "test-jwt.token")
    };
    let register = test_register();
    let (obs, mut status_rx, mut log_rx) =
        Observability::channels(crate::status::SupervisorStatus::default());
    obs.set_allow_real_jobs(true);

    let obs2 = obs.clone();
    let client =
        tokio::spawn(async move { run(&config, &register, &obs2, never_shutdown()).await });

    // Session 1: a real job runs to completion — the terminal sweep
    // remembers {payloads:<sha>, bundles:s} for the next session.
    let (mut ws, _) = accept_ws(&listener).await;
    let _register = next_text(&mut ws).await;
    ws.send(Message::Text(Utf8Bytes::from(job_assign_json(
        &job_id, &sha,
    ))))
    .await
    .unwrap();
    tokio::time::timeout(
        Duration::from_secs(20),
        status_rx.wait_for(|s| s.jobs_completed == 1 && s.current_job.is_none()),
    )
    .await
    .expect("job did not complete in time")
    .expect("status channel closed");

    // Force the reconnect: a clean server close, then session 2. The
    // startup sweep runs BEFORE the register frame, so a register on
    // session 2 proves the sweep already ran with whatever set the
    // loop remembered.
    ws.send(Message::Close(None)).await.ok();
    drop(ws);
    let (mut ws2, _) = tokio::time::timeout(Duration::from_secs(10), accept_ws(&listener))
        .await
        .expect("no reconnection within 10s");
    let second: serde_json::Value = serde_json::from_str(&next_text(&mut ws2).await).unwrap();
    assert_eq!(second["type"], "register");

    // THE SEMANTICS: the reconnect sweep must report the REMEMBERED
    // protected set — cache_keys_for(assign) is 2 keys (payloads +
    // bundles). Old code passed &[]: the line would never appear at
    // all (nothing evicted, nothing protected).
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut sweep_line = String::new();
    while std::time::Instant::now() < deadline {
        match log_rx.try_recv() {
            Ok(line) => {
                if line.message.contains("Cache sweep") {
                    sweep_line = line.message;
                    break;
                }
            }
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(_) => break,
        }
    }
    assert!(
        sweep_line.contains("2 protected"),
        "reconnect sweep must hand the remembered keys to the cache sweep; got: {sweep_line:?}"
    );

    ws2.close(None).await.ok();
    drop(ws2);
    client.abort();
    std::fs::remove_dir_all(payload_dir).ok();
}
/// Packet 18: the idle-sleep assertion is held EXACTLY for the job's
/// lifetime. Proven through keepawake's test-visible active-guard
/// counter (a pgrep census cannot work here: the parallel test binary's
/// sibling tests hold their own caffeinates parented to this same
/// process): the count must rise while the job renders, hold for the
/// job's duration, and return to baseline after completion AND after
/// failure.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn sleep_assertion_held_for_job_lifetime_only() {
    let pid = std::process::id();
    let baseline = crate::keepawake::active_guard_count_for_tests();

    // A job that renders for ~2s: long enough to observe the guard
    // held mid-job by polling the counter.
    let sha = format!("test-keepawake-{pid}");
    let payload_dir = seed_fake_payload(
        &sha,
        r#"#!/bin/sh
sleep 2
echo '{"type":"done","outputSizeInBytes":1,"wallTimeMs":1}'
"#,
    );

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let (_cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();
    let job = tokio::spawn(crate::runner::run_job(
        serde_json::from_str::<crate::protocol::JobAssignMessage>(&job_assign_json(
            &format!("job-keepawake-{pid}"),
            &sha,
        ))
        .unwrap(),
        cancel_rx,
        tx,
        std::time::Duration::from_secs(30),
        1024 * 1024 * 1024,
    ));

    // Mid-job: the guard is held — count strictly above baseline.
    let mut saw_held = false;
    for _ in 0..200 {
        if crate::keepawake::active_guard_count_for_tests() > baseline {
            saw_held = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(saw_held, "sleep assertion must be held while the job runs");

    job.await.expect("run_job task");
    let done = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .expect("completion frame within 5s")
        .expect("channel open");
    assert!(
        matches!(done, crate::protocol::WorkerMessage::JobComplete(_)),
        "expected jobComplete, got {done:?}"
    );

    // After completion: back to baseline (retry — Drop's kill+wait is
    // synchronous and fast, but the decrement lands before this read).
    let mut settled = false;
    for _ in 0..100 {
        if crate::keepawake::active_guard_count_for_tests() <= baseline {
            settled = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(settled, "sleep assertion must be released after completion");

    // Failure path: a runner that errors must also release it.
    let fail_sha = format!("test-keepawake-fail-{pid}");
    let fail_dir = seed_fake_payload(
        &fail_sha,
        r#"#!/bin/sh
echo '{"type":"error","message":"boom"}'
"#,
    );
    let (tx2, mut rx2) = tokio::sync::mpsc::unbounded_channel();
    let (_c2, cancel2) = tokio::sync::oneshot::channel::<()>();
    crate::runner::run_job(
        serde_json::from_str::<crate::protocol::JobAssignMessage>(&job_assign_json(
            &format!("job-keepawake-fail-{pid}"),
            &fail_sha,
        ))
        .unwrap(),
        cancel2,
        tx2,
        std::time::Duration::from_secs(30),
        1024 * 1024 * 1024,
    )
    .await;
    let failed = tokio::time::timeout(std::time::Duration::from_secs(5), rx2.recv())
        .await
        .expect("failure frame within 5s")
        .expect("channel open");
    assert!(
        matches!(failed, crate::protocol::WorkerMessage::JobFailed(_)),
        "expected jobFailed, got {failed:?}"
    );
    let mut settled_fail = false;
    for _ in 0..100 {
        if crate::keepawake::active_guard_count_for_tests() <= baseline {
            settled_fail = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        settled_fail,
        "sleep assertion must be released after failure"
    );

    let _ = std::fs::remove_dir_all(payload_dir);
    let _ = std::fs::remove_dir_all(fail_dir);
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

    ws.send(Message::Text(Utf8Bytes::from(job_assign_json(
        &job_id, &sha,
    ))))
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
    ws.send(Message::Text(Utf8Bytes::from(format!(
        r#"{{"type":"cancel","tenant":"driffs","jobId":"{job_id}"}}"#
    ))))
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
            frames.push(serde_json::from_str(t.as_str()).unwrap());
        }
    }
    client.await.unwrap().expect("clean exit");

    assert!(
        frames.iter().all(|v| v["type"] != "jobFailed"),
        "jobFailed must be suppressed after cancel, got {frames:?}"
    );
    std::fs::remove_dir_all(&payload_dir).ok();
}

/// (a2) Cancel lands AFTER the runner already reported `done` and closed
/// stdout — run_job has left its select loop and is in child.wait(), so
/// the job's eventual clean exit produces JobComplete, not JobFailed.
/// That completion is equally unwanted: dispatch has already marked the
/// job canceled (its settle update is scoped to assigned/rendering), so a
/// late jobComplete would reference an output nobody will ever settle.
/// The supervisor must suppress it exactly like the post-cancel
/// jobFailed.
///
/// Determinism: the script closes stdout immediately after `done`, so the
/// supervisor observes EOF within milliseconds of jobAccepted; the test
/// waits 500ms before sending cancel, far past that point, so the cancel
/// always lands in the child.wait() window — the JobComplete path, not
/// the (already suppressed) JobFailed one. The runner then exits 0 on its
/// own 2s schedule; nothing kills it mid-wait.
#[cfg(unix)]
#[tokio::test]
async fn cancel_after_runner_done_suppresses_job_complete() {
    let pid = std::process::id();
    let sha = format!("test-cancel-complete-{pid}");
    let job_id = format!("job-cancel-complete-{pid}");
    // Emit `done`, close stdout ( supervisor leaves its select loop into
    // child.wait()), linger, then exit cleanly — a runner that finished
    // its work while dispatch was already canceling the job.
    let payload_dir = seed_fake_payload(
        &sha,
        "#!/bin/sh\necho '{\"type\":\"done\",\"outputSizeInBytes\":123,\"wallTimeMs\":50}'\nexec 1>&-\nsleep 2\nexit 0\n",
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

    ws.send(Message::Text(Utf8Bytes::from(job_assign_json(
        &job_id, &sha,
    ))))
    .await
    .unwrap();

    // Collect frames until jobAccepted (heartbeats may interleave).
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

    // Let the runner's `done` + stdout EOF be processed first (see the
    // determinism note above), then cancel into the child.wait() window.
    tokio::time::sleep(Duration::from_millis(500)).await;
    ws.send(Message::Text(Utf8Bytes::from(format!(
        r#"{{"type":"cancel","tenant":"driffs","jobId":"{job_id}"}}"#
    ))))
    .await
    .unwrap();

    // The suppressed completion bumps jobs_canceled and clears
    // current_job — the deterministic "fully processed" signal. Under the
    // old suppress-failed-only code this times out instead (the
    // completion is emitted as a normal success).
    tokio::time::timeout(
        Duration::from_secs(15),
        status_rx.wait_for(|s| s.jobs_canceled == 1 && s.current_job.is_none()),
    )
    .await
    .expect("post-done completion after cancel was not processed in time")
    .expect("status channel closed");

    // Purge still happened (WorkDir dropped on run_job's success path).
    assert!(
        job_workdirs(&job_id).is_empty(),
        "workdir must be purged after the suppressed completion"
    );

    // Close and drain everything the client ever sent.
    ws.close(None).await.ok();
    while let Some(Ok(frame)) = ws.next().await {
        if let Message::Text(t) = frame {
            frames.push(serde_json::from_str(t.as_str()).unwrap());
        }
    }
    client.await.unwrap().expect("clean exit");

    assert!(
        frames.iter().all(|v| v["type"] != "jobComplete"),
        "jobComplete must be suppressed after cancel, got {frames:?}"
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

    ws.send(Message::Text(Utf8Bytes::from(job_assign_json(
        &job_id, &sha,
    ))))
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

    let client = tokio::spawn(async move { run(&config, &register, &obs, never_shutdown()).await });

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

    let client = tokio::spawn(async move { run(&config, &register, &obs, never_shutdown()).await });

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

    let client = tokio::spawn(async move { run(&config, &register, &obs, never_shutdown()).await });

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
    let client = tokio::spawn(async move { run(&config, &register, &obs, never_shutdown()).await });

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
    // Single session: it pins the STATE TRANSITIONS incl. the final
    // Disconnected, not reconnection (which has its own tests).
    config.reconnect = false;
    let register = test_register();

    let (obs, mut status_rx, _log_rx) =
        Observability::channels(crate::status::SupervisorStatus::default());

    let client = tokio::spawn(async move { run(&config, &register, &obs, never_shutdown()).await });

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

    let client = tokio::spawn(async move { run(&config, &register, &obs, never_shutdown()).await });

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
    let started = std::env::temp_dir().join(format!("decent-gc-im-{}.started", std::process::id()));
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

    ws.send(Message::Text(Utf8Bytes::from(job_assign_json(
        &job_id, &sha,
    ))))
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

    ws.send(Message::Text(Utf8Bytes::from(format!(
        r#"{{"type":"cancel","tenant":"driffs","jobId":"{job_id}"}}"#
    ))))
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

    ws.send(Message::Text(Utf8Bytes::from(job_assign_json(
        &job_id, &sha,
    ))))
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

    ws.send(Message::Text(Utf8Bytes::from(format!(
        r#"{{"type":"cancel","tenant":"driffs","jobId":"{job_id}"}}"#
    ))))
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
    std::fs::set_permissions(&browser_stand_in, std::fs::Permissions::from_mode(0o755)).unwrap();

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

    ws.send(Message::Text(Utf8Bytes::from(assign)))
        .await
        .unwrap();

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
        .split_whitespace() // D-8: `<pid> <start time>`
        .next()
        .unwrap_or("")
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

    ws.send(Message::Text(Utf8Bytes::from(format!(
        r#"{{"type":"cancel","tenant":"driffs","jobId":"{job_id}"}}"#
    ))))
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
    std::fs::set_permissions(&browser_stand_in, std::fs::Permissions::from_mode(0o755)).unwrap();

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

    ws.send(Message::Text(Utf8Bytes::from(assign)))
        .await
        .unwrap();

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
        .split_whitespace() // D-8: `<pid> <start time>`
        .next()
        .unwrap_or("")
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
    ws.send(Message::Text(Utf8Bytes::from(format!(
        r#"{{"type":"cancel","tenant":"driffs","jobId":"{job_id}"}}"#
    ))))
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
    // any socket-death error is acceptable — under parallel load either
    // arm can surface first (the read arm reports "websocket error", the
    // heartbeat/worker-frame send arms "failed to send …"), and both
    // drain before returning. The tree assertions below are the
    // load-bearing proof.
    match &outcome {
        Ok(()) => {}
        Err(e) => {
            let chain = format!("{e:#}");
            assert!(
                chain.contains("websocket error") || chain.contains("failed to send"),
                "unexpected error from run(): {chain}"
            );
        }
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

    ws.send(Message::Text(Utf8Bytes::from(job_assign_json(
        &job_id, &sha,
    ))))
    .await
    .unwrap();

    // Gate on the runner REALLY running — its own pid, written by itself.
    let runner_pid = wait_for_pid_file(&pid_file).await;
    assert!(
        unsafe { libc::kill(runner_pid as libc::pid_t, 0) } == 0,
        "runner must be alive before cancel"
    );

    ws.send(Message::Text(Utf8Bytes::from(format!(
        r#"{{"type":"cancel","tenant":"driffs","jobId":"{job_id}"}}"#
    ))))
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

/// PACKET 23 companion: a CLEAN server Close must also reconnect.
/// Pre-fix investigation answer to "does a normal close share the same
/// path?": YES — pre-fix both shapes returned out of run() and killed
/// the process (clean close exited 0, abort exited 1). Dispatch
/// redeploys and operator restarts produce clean closes too; the node
/// must come back from those as well.
#[tokio::test]
async fn clean_server_close_reconnects_instead_of_exiting() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let config = ConnectionConfig {
        heartbeat_interval: Duration::from_millis(50),
        max_connect_attempts: 20,
        connect_retry_delay: Duration::from_millis(50),
        heartbeat_limit: None,
        reconnect: true,
        reconnect_backoff_base: Duration::from_millis(50),
        reconnect_backoff_max: Duration::from_secs(1),
        ..ConnectionConfig::new(format!("ws://127.0.0.1:{port}/ws"), "test-jwt.token")
    };
    let register = test_register();
    let (obs, _status_rx, _log_rx) =
        Observability::channels(crate::status::SupervisorStatus::default());

    let obs2 = obs.clone();
    let client =
        tokio::spawn(async move { run(&config, &register, &obs2, never_shutdown()).await });

    // Session 1 → register → server closes CLEANLY (proper Close frame).
    let (mut ws, _) = accept_ws_capturing(&listener).await;
    let first: serde_json::Value = serde_json::from_str(&next_text(&mut ws).await).unwrap();
    assert_eq!(first["type"], "register");
    ws.send(Message::Close(None)).await.unwrap();
    drop(ws);

    // Session 2: the supervisor must come back and re-register.
    let (mut ws, _) = tokio::time::timeout(Duration::from_secs(10), accept_ws_capturing(&listener))
        .await
        .expect("no reconnection within 10s after a CLEAN close — run() returned instead");
    let second: serde_json::Value = serde_json::from_str(&next_text(&mut ws).await).unwrap();
    assert_eq!(
        second["type"], "register",
        "supervisor must re-register after a clean server close"
    );

    drop(ws);
    client.abort();
}

// ── PACKET 28: ReconnectBackoff unit tests ────────────────────────────

fn backoff_with(
    base_ms: u64,
    max_s: u64,
    canned: impl FnMut() -> f64 + Send + 'static,
) -> ReconnectBackoff {
    ReconnectBackoff::with_canned_rng(
        Duration::from_millis(base_ms),
        Duration::from_secs(max_s),
        canned,
    )
}

#[test]
fn backoff_bounds_every_delay_within_cap() {
    // For n = 1..=8 with base=1s/max=60s and an adversarial rng=0.999,
    // every delay must lie in [0, 60s].
    for _ in 0..8 {
        let mut b = backoff_with(1000, 60, || 0.999);
        for _ in 1..=8u32 {
            let d = b.next_delay();
            assert!(d <= Duration::from_secs(60), "delay {d:?} above cap");
        }
    }
}

#[test]
fn backoff_growth_mean_increases_and_attempts_separate() {
    // rng=0.999 → delay ≥ 0.9·target (target=base·2^(n-1), pre-cap):
    // attempt 1 ≈ 1s, attempt 4 ≈ 8s — clearly separated.
    let mut b = backoff_with(1000, 60, || 0.999);
    let d1 = b.next_delay();
    b.next_delay();
    b.next_delay(); // attempts 2, 3
    let d4 = b.next_delay();
    assert!(d1 >= Duration::from_millis(900), "attempt1 {d1:?}");
    assert!(d1 <= Duration::from_secs(1));
    assert!(
        d4 >= Duration::from_millis(7200),
        "attempt4 {d4:?} (≥0.9·8s)"
    );
    assert!(d4 > d1 * 4, "growth: {d4:?} vs 4×{d1:?}");

    // Mean grows with n (canned 0.5): expected = target/2.
    let mut b2 = backoff_with(1000, 60, || 0.5);
    let m1 = b2.next_delay();
    for _ in 0..2 {
        b2.next_delay();
    }
    let m4 = b2.next_delay();
    assert!(m4 > m1, "mean growth {m1:?} → {m4:?}");
}

#[test]
fn backoff_cap_binds_target_not_jitter() {
    // base=1s: attempt 7 targets 64s > cap 60s → target clamps to 60s;
    // rng=0.999 still ≤ 60s.
    let mut b = backoff_with(1000, 60, || 0.999);
    for _ in 0..6 {
        b.next_delay();
    }
    assert_eq!(b.target(), Duration::from_secs(60));
    let d = b.next_delay();
    assert!(d <= Duration::from_secs(60));
    assert!(d >= Duration::from_secs(59), "0.999·60s ≈ 60s: {d:?}");
    // and deeper attempts stay pinned
    assert_eq!(b.target(), Duration::from_secs(60));
}

#[test]
fn backoff_full_jitter_variance() {
    // Oscillating canned rng at the same attempt index → different
    // delays; extremes rng=0 → 0, rng→1 → target.
    // Oscillating rng: consecutive draws differ, so consecutive
    // delays differ (a≈0.99s, c≈20ms — same generator, same curve
    // position ±1, different jitter outcomes).
    let mut b = backoff_with(1000, 60, {
        let mut hi = false;
        move || {
            hi = !hi;
            if hi {
                0.99
            } else {
                0.01
            }
        }
    });
    let a = b.next_delay(); // attempt 1, rng 0.99 → ~0.99s
    let c = b.next_delay(); // attempt 2, rng 0.01 → 0.01·2s = 20ms
    assert!(a != c);
    assert!(a >= Duration::from_millis(980), "a={a:?}");
    assert!(c <= Duration::from_millis(21), "c={c:?}");

    let mut lo = backoff_with(1000, 60, || 0.0);
    assert_eq!(lo.next_delay(), Duration::ZERO);
    let mut hi = backoff_with(1000, 60, || 0.999999);
    let d = hi.next_delay();
    assert!(d >= Duration::from_millis(999), "near-target: {d:?}");
    assert!(d <= Duration::from_secs(1));
}

#[test]
fn backoff_reset_on_healthy_session_only() {
    // Short session: curve keeps climbing.
    let mut b = backoff_with(1000, 60, || 0.999);
    b.next_delay(); // attempt 1
    b.next_delay(); // attempt 2
    b.session_lasted(Duration::from_secs(5)); // « max
    assert_eq!(b.target(), Duration::from_secs(4), "attempt 3 target");
    // Healthy session (≥ max): resets to cycle-1 scale.
    b.session_lasted(Duration::from_secs(60));
    assert_eq!(b.target(), Duration::from_secs(1), "fresh cycle");
}

#[test]
fn backoff_saturates_instead_of_overflowing() {
    let mut b = backoff_with(1000, 60, || 0.5);
    for _ in 0..1000 {
        b.next_delay();
    }
    assert_eq!(b.target(), Duration::from_secs(60));
    let d = b.next_delay();
    assert!(d <= Duration::from_secs(60));
}

/// Set SO_LINGER(0) on the stream and drop it → the peer sees RST, not
/// FIN — no Close frame, no handshake: the wire shape of "peer closed
/// connection without sending TLS close_notify" and of any mid-frame
/// TCP death.
async fn abort_with_rst(stream: &TcpStream) {
    use std::os::fd::AsRawFd;
    let fd = stream.as_raw_fd();
    unsafe {
        let linger = libc::linger {
            l_onoff: 1,
            l_linger: 0,
        };
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_LINGER,
            &linger as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::linger>() as libc::socklen_t,
        );
    }
}

/// PACKET 23: hard TCP aborts (RST — the fly-deploy shape) must not end
/// the run. Pre-fix, the WS read error returned out of `run()` and
/// `decent start` exited with code 1; every dispatch deploy killed every
/// foreground node. Post-fix: session 1 aborts, the supervisor
/// reconnects and re-registers (session 2), that session aborts too, and
/// it STILL comes back (session 3) — sustained reconnection, not one
/// lucky retry.
#[tokio::test]
async fn hard_socket_abort_reconnects_instead_of_exiting() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let config = ConnectionConfig {
        heartbeat_interval: Duration::from_millis(50),
        max_connect_attempts: 20,
        connect_retry_delay: Duration::from_millis(50),
        heartbeat_limit: None,
        reconnect: true,
        reconnect_backoff_base: Duration::from_millis(50),
        reconnect_backoff_max: Duration::from_secs(1),
        ..ConnectionConfig::new(format!("ws://127.0.0.1:{port}/ws"), "test-jwt.token")
    };
    let register = test_register();
    let (obs, _status_rx, _log_rx) =
        Observability::channels(crate::status::SupervisorStatus::default());

    let obs2 = obs.clone();
    let client =
        tokio::spawn(async move { run(&config, &register, &obs2, never_shutdown()).await });

    async fn accept_registering_session(
        listener: &TcpListener,
    ) -> (WebSocketStream<TcpStream>, serde_json::Value) {
        let (ws, _) = tokio::time::timeout(
            Duration::from_secs(10),
            accept_ws_capturing(listener),
        )
        .await
        .expect(
            "no reconnection within 10s — run() exited (or hung) after the hard abort instead of reconnecting",
        );
        let mut ws = ws;
        let register: serde_json::Value = serde_json::from_str(&next_text(&mut ws).await).unwrap();
        (ws, register)
    }

    // Session 1 → register → HARD ABORT.
    let (ws, first) = accept_registering_session(&listener).await;
    assert_eq!(first["type"], "register");
    abort_with_rst(ws.get_ref()).await;
    drop(ws);

    // Session 2: must come back and re-register.
    let (ws, second) = accept_registering_session(&listener).await;
    assert_eq!(
        second["type"], "register",
        "supervisor must re-register after a hard socket abort"
    );
    abort_with_rst(ws.get_ref()).await;
    drop(ws);

    // Session 3: STILL coming back — reconnection is sustained.
    let (ws, third) = accept_registering_session(&listener).await;
    assert_eq!(
        third["type"], "register",
        "supervisor must survive a second consecutive hard abort"
    );

    // Sustained-register assertions above are the proof. Teardown: the
    // run would keep reconnecting (reconnect=true, never_shutdown), so
    // end the task explicitly.
    drop(ws);
    client.abort();
}

/// PACKET 12: send-error drain inventory (pinned by the two tests below).
///
/// Every `sink.send` in the run loop, and whether its Err path can hold
/// an in-flight job:
///
/// | site | failure handling | in-flight job possible? |
/// |---|---|---|
/// | register (pre-loop) | `?` return, no drain | NO — before the loop; no job exists |
/// | Close on shutdown | `.ok()` swallowed | NO — never an exit |
/// | Close on heartbeat limit | `.ok()` swallowed | NO — never an exit |
/// | heartbeat frame (~L342) | return Err WITH drain | YES |
/// | worker job frame (~L418) | return Err WITH drain | YES |
/// | jobAccepted (assign arm ~L473) | `?` WITHOUT drain | NO by construction — the job task is spawned AFTER this send succeeds, so no runner or workdir exists yet |
///
/// The two YES arms are exercised through socket death — the only way a
/// send fails on a real tungstenite stream — which also wakes the read
/// arm (Some(Err(..)) → identical drain). Deterministically the read arm
/// wins the select race, so the send arms are its structural shadows;
/// they exist (not deduplicated) because a backpressured sink can also
/// fail a send on flush without a peer read error. The tests pin the
/// guarantee BOTH share: a runner alive at the moment the socket dies is
/// terminated, its workdir purged, and `run()` does not return first.
///
/// (a) dies while a worker frame (progress) is imminent; (b) dies on the
/// heartbeat arm with a TERM-ignoring runner, forcing the drain to ride
/// the full 10s grace — the wall-clock floor that proves the grace path
/// was really exercised rather than a fast return.
#[cfg(unix)]
#[tokio::test]
async fn send_failure_with_in_flight_job_still_drains_and_purges() {
    let pid = std::process::id();
    let sha = format!("test-sendfail-drain-{pid}");
    let job_id = format!("job-sendfail-drain-{pid}");
    let pid_file = std::env::temp_dir().join(format!("{job_id}.runner-pid"));
    let _ = std::fs::remove_file(&pid_file);

    // TERM-honoring runner that reports progress and keeps running: the
    // supervisor holds an in-flight job and is about to forward progress
    // (a worker-frame send) when the socket dies.
    let payload_script = format!(
        r#"#!/bin/sh
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

    ws.send(Message::Text(Utf8Bytes::from(job_assign_json(
        &job_id, &sha,
    ))))
    .await
    .unwrap();

    // Gate on the runner REALLY running — its own pid, written by itself.
    let runner_pid = wait_for_pid_file(&pid_file).await;
    assert!(
        unsafe { libc::kill(runner_pid as libc::pid_t, 0) } == 0,
        "runner must be alive before the socket dies"
    );

    // NO cancel frame: the scenario is the socket dying while the job is
    // simply in flight — the read arm errors out (and the pending
    // progress-forward / heartbeat sends fail against the same dead
    // socket), and the drain must cancel the job from there. A cancel
    // first would kill this TERM-honoring runner before the socket
    // death, leaving nothing to drain (caught by the alive assert in the
    // first version of this test).
    drop(ws);
    drop(listener);

    // run() must not return until the drain finishes. A TERM-honoring
    // runner exits fast, so there is no timing floor here — the load-
    // bearing assertions are alive-at-failure (above) and dead+purged
    // (below); the grace-riding floor is test (b)'s job.
    let outcome = tokio::time::timeout(Duration::from_secs(30), &mut client)
        .await
        .expect("run() hung — drain did not complete in 30s")
        .expect("client task panicked");
    // The socket really died; either a websocket error or a clean end is
    // honest. What would be dishonest is returning before the drain.
    match &outcome {
        Ok(()) => {}
        Err(e) => assert!(
            format!("{e:#}").contains("websocket error")
                || format!("{e:#}").contains("failed to send"),
            "unexpected error from run(): {e:#}"
        ),
    }

    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        unsafe { libc::kill(runner_pid as libc::pid_t, 0) } != 0,
        "runner survived a send failure with an in-flight job — drain was abandoned"
    );
    assert!(
        job_workdirs(&job_id).is_empty(),
        "workdir survived a send failure with an in-flight job — purge invariant violated"
    );

    let _ = std::fs::remove_file(&pid_file);
    std::fs::remove_dir_all(&payload_dir).ok();
}

/// (b) The heartbeat send arm with a wedged runner: the drain must ride
/// the full 10s CANCEL_GRACE to SIGKILL ACROSS the dead socket. Pre-fix
/// packet-5 shape (return-on-error without drain) would return ~0s here
/// and leave the wedged runner alive forever.
#[cfg(unix)]
#[tokio::test]
async fn send_failure_mid_grace_rides_the_grace_to_sigkill() {
    let pid = std::process::id();
    let sha = format!("test-sendfail-grace-{pid}");
    let job_id = format!("job-sendfail-grace-{pid}");
    let pid_file = std::env::temp_dir().join(format!("{job_id}.runner-pid"));
    let _ = std::fs::remove_file(&pid_file);

    // Wedged runner: traps TERM, so only the 10s grace + SIGKILL can end
    // it. Heartbeat interval in long_config is 50ms, so heartbeats (the
    // ~L342 arm) are pounding the dying socket throughout.
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

    ws.send(Message::Text(Utf8Bytes::from(job_assign_json(
        &job_id, &sha,
    ))))
    .await
    .unwrap();

    let runner_pid = wait_for_pid_file(&pid_file).await;
    assert!(
        unsafe { libc::kill(runner_pid as libc::pid_t, 0) } == 0,
        "runner must be alive before cancel"
    );

    ws.send(Message::Text(Utf8Bytes::from(format!(
        r#"{{"type":"cancel","tenant":"driffs","jobId":"{job_id}"}}"#
    ))))
    .await
    .unwrap();

    // Enter the grace window (TERM sent and ignored), then kill the
    // socket. Heartbeats every 50ms fail against it from here on.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(
        unsafe { libc::kill(runner_pid as libc::pid_t, 0) } == 0,
        "runner died before the socket failure — not mid-grace, so the grace-riding proof below would be vacuous"
    );
    drop(ws);
    drop(listener);

    let t0 = std::time::Instant::now();
    let outcome = tokio::time::timeout(Duration::from_secs(45), &mut client)
        .await
        .expect("run() hung — drain did not complete in 45s")
        .expect("client task panicked");
    let ran_for = t0.elapsed().as_secs_f64();
    assert!(
        ran_for >= 7.0,
        "run() returned in {ran_for:.1}s — the grace path was not ridden (a dropped job returns ~0s; 10s grace minus the 1.5s pre-drop cancel lead is ~8.5s)"
    );
    match &outcome {
        Ok(()) => {}
        Err(e) => assert!(
            format!("{e:#}").contains("websocket error")
                || format!("{e:#}").contains("failed to send"),
            "unexpected error from run(): {e:#}"
        ),
    }

    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        unsafe { libc::kill(runner_pid as libc::pid_t, 0) } != 0,
        "wedged runner survived a mid-grace send failure — SIGKILL escalation was abandoned"
    );
    assert!(
        job_workdirs(&job_id).is_empty(),
        "workdir survived a mid-grace send failure — purge invariant violated"
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

    ws.send(Message::Text(Utf8Bytes::from(job_assign_json(
        &job_id, &sha,
    ))))
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
