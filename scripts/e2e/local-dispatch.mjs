/**
 * PACKET 3 local dispatch harness — speaks the REAL @decent-render/protocol
 * frames over Bun's native WebSocket, validating with the REAL zod schemas
 * from the published protocol package.
 *
 * Chosen over running apps/dispatch (justification, receipt §3): dispatch is
 * a Fly service wired to production Turso/R2 (credits, presign, worker auth)
 * which this packet must not touch; the supervisor's wire behaviour depends
 * only on the protocol frames, which this harness validates with the same
 * schemas the real dispatch uses.
 *
 * Serves over one port:
 *   GET /payload.tar.gz /browser.tar.gz /bundle.tar.gz /input-props.json
 *   PUT /output.mp4
 *   WS upgrade at /ws
 *
 * Usage: bun scripts/e2e/local-dispatch.mjs [--cancel-after=N] [--artifacts=DIR] [--port=N]
 *   exit 0 on jobComplete (success run) or after clean cancel handling,
 *   exit 1 on jobFailed / schema violation / timeout / jobComplete WITHOUT
 *   an uploaded video (C-7) / failed cancel-cleanup assertions.
 * Env: E2E_EXPECT_FAIL=missing-upload — no-supervisor self-proof that a
 *   missing video exits 1. E2E_WORKER_ROOT=<path> — the supervisor's state
 *   root, enabling the cancel path's workdir-gone assertion.
 */
import {
  ServerMessageSchema,
  WorkerMessageSchema,
} from '../../packages/protocol/dist/index.js';
import {readFileSync, writeFileSync, statSync, readdirSync} from 'node:fs';
import path from 'node:path';
import crypto from 'node:crypto';

const arg = (n) => process.argv.find((a) => a.startsWith(`--${n}=`))?.slice(n.length + 3);
const ART = arg('artifacts') || '/tmp/p3-artifacts';
const PORT = Number(arg('port') || 8790);
const CANCEL_AFTER = Number(arg('cancel-after') || 0);
// Packet 11: where on the progress curve the event-driven cancel fires.
// 0.3 (default) lands mid-render; 1.0 lands the instant the runner reports
// completion — right before its synchronous verify + pre-PUT cancel check,
// the worst case for "cancel observed, upload must still not happen".
const CANCEL_THRESHOLD = Number(arg('cancel-threshold') || 0.3);
// PACKET 5: hard-drop the WebSocket this many ms after sending cancel —
// models dispatch redeploys / network blips DURING the supervisor's
// CANCEL_GRACE window. The supervisor must still finish killing the tree.
const DROP_CONNECTION_AFTER = Number(arg('drop-connection-after') || 0);
// C-7: E2E_EXPECT_FAIL=missing-upload runs the harness WITHOUT a supervisor —
// it feeds itself a schema-valid jobComplete whose upload is FORCED missing
// and must exit 1 (the harness can never exit 0 without a video). A normal
// run sets nothing.
const E2E_EXPECT_FAIL = process.env.E2E_EXPECT_FAIL || '';
// C-7: where the supervisor under test keeps its state root (the launcher
// exports the same root it gave the supervisor via HOME/worker-root). Needed
// for the cancel path's "workdir is gone" assertion; when unset that one
// check is skipped (the harness cannot know where to look), the other two
// assertions still run.
const E2E_WORKER_ROOT = process.env.E2E_WORKER_ROOT || '';
const OUT = path.join(ART, 'uploaded-output.mp4');

const log = (...a) => console.error(`[dispatch]`, ...a);
const sha256 = (bytes) => crypto.createHash('sha256').update(bytes).digest('hex');

let sawRegister = false;
let sawAccepted = false;
let assignSent = false;
let cancelSent = false;
let result = null; // 'complete' | 'failed'
let exitCode = 0;

const timer = setTimeout(() => {
  log(`FAIL: timed out (register=${sawRegister} accepted=${sawAccepted} assigned=${assignSent} cancelSent=${cancelSent})`);
  process.exit(1);
}, 300_000);

function finish(code, summary) {
  console.log(`[dispatch] ${summary}`);
  process.exit(code);
}

// C-7: the harness must never exit 0 without a video on disk, and the
// cancel path must verify the supervisor actually cleaned up. Each failing
// assertion finishes 1.
function assertCleanAfterCancel() {
  // (1) no bytes were uploaded.
  const uploaded = statSync(OUT, {throwIfNoEntry: false});
  if (uploaded && uploaded.size > 0) {
    finish(1, `cancel path uploaded ${uploaded.size}b to ${OUT} — leak`);
  }
  // (2) the workdir is gone. Only checkable when the launcher told us where
  // the supervisor keeps its state root (E2E_WORKER_ROOT); skipped otherwise.
  if (E2E_WORKER_ROOT) {
    let entries = [];
    try {
      entries = readdirSync(path.join(E2E_WORKER_ROOT, 'workdirs'));
    } catch (e) {
      if (e.code !== 'ENOENT') throw e; // no workdirs dir at all = purged
    }
    const left = entries.filter((n) => n.includes('job-p3-proof'));
    if (left.length > 0) {
      finish(1, `cancel path left workdirs under ${E2E_WORKER_ROOT}: ${left.join(', ')} — leak`);
    }
  } else {
    log('workdir-gone check skipped: E2E_WORKER_ROOT unset');
  }
  // (3) no Chrome/runner process from this run remains. pgrep is read-only;
  // a false positive (an unrelated local node) fails CONSERVATIVELY.
  const pattern = E2E_WORKER_ROOT || '.decent-worker';
  const pg = Bun.spawnSync(['pgrep', '-f', pattern]);
  const pids = pg.stdout.toString().trim();
  if (pids) {
    finish(1, `cancel path left processes matching /${pattern}/: ${pids.split('\n').join(',')} — leak`);
  }
}

// jobComplete landed. The VIDEO MUST BE ON DISK: a completion without an
// uploaded output is the exact lie this harness exists to catch (C-7) —
// exit 1, never 0.
function handleJobComplete(frame, forceMissingUpload = false) {
  clearTimeout(timer);
  if (cancelSent) {
    // A completion racing a late cancel is fine; record it.
    sawAccepted = true;
    result = 'complete';
    log('jobComplete after cancel sent (race — not a leak)');
    return;
  }
  const uploaded = forceMissingUpload ? undefined : statSync(OUT, {throwIfNoEntry: false});
  if (!uploaded || uploaded.size === 0) {
    finish(1, `jobComplete WITHOUT an uploaded video (${OUT} missing/empty) — refusing to exit 0`);
  }
  finish(0, `jobComplete: frames=${frame.metrics?.frames} wallMs=${frame.metrics?.wallMs} size=${frame.metrics?.outputSizeInBytes} uploaded=${uploaded.size}b`);
}

const server = Bun.serve({
  port: PORT,
  hostname: '127.0.0.1',
  fetch(request, server) {
    const url = new URL(request.url);
    if (url.pathname === '/ws' && server.upgrade(request)) return;
    if (request.method === 'PUT' && url.pathname === '/output.mp4') {
      // Packet 15 wire proof: capture the transport headers the runner's
      // streamed PUT actually carried — Content-Length must be present and
      // chunked must be absent (S3-compatible presigned PUTs reject chunked).
      const wireHeaders = [
        `content-length=${request.headers.get('content-length') ?? 'ABSENT'}`,
        `transfer-encoding=${request.headers.get('transfer-encoding') ?? 'none'}`,
      ].join(' ');
      // arrayBuffer, NOT text(): text() UTF-8-decodes the body and replaces
      // invalid sequences with U+FFFD, corrupting binary uploads. (Found the
      // hard way in the first E2E run: moov atom not found, +66% size.)
      return request.arrayBuffer().then((body) => {
        const bytes = Buffer.from(body);
        writeFileSync(OUT, bytes);
        log(`PUT /output.mp4 ← ${bytes.length} bytes, sha=${sha256(bytes).slice(0, 12)} [${wireHeaders}]`);
        return new Response(null, {status: 200});
      });
    }
    const files = {
      '/payload.tar.gz': 'payload.tar.gz',
      // Serve the pinned first-run browser (sha in jobAssign must match the
      // bytes actually served — the supervisor verifies, as run 6 proved).
      '/browser.tar.gz': 'browser-first.tar.gz',
      '/bundle.tar.gz': 'bundle.tar.gz',
    };
    if (request.method === 'GET' && url.pathname in files) {
      const bytes = readFileSync(path.join(ART, files[url.pathname]));
      log(`GET ${url.pathname} → ${bytes.length} bytes`);
      return new Response(bytes, {headers: {'content-type': 'application/gzip'}});
    }
    if (request.method === 'GET' && url.pathname === '/input-props.json') {
      const realProps = arg('real-props');
      if (realProps) {
        return new Response(readFileSync(realProps), {
          headers: {'content-type': 'application/json'},
        });
      }
      return Response.json({compositionId: 'p3comp', inputProps: {}});
    }
    return new Response('not found', {status: 404});
  },
  websocket: {
    open(ws) {
      log('worker connected');
    },
    close(ws) {
      log('worker disconnected');
    },
    message(ws, message) {
      if (typeof message !== 'string') return;
      log('⇐', message.slice(0, 400));
      let frame;
      try {
        frame = WorkerMessageSchema.parse(JSON.parse(message));
      } catch (e) {
        // A frame that fails the REAL schema is a hard harness failure: the
        // supervisor must speak valid protocol to pass this packet.
        log(`FAIL: worker frame failed real protocol schema: ${e.message}`);
        process.exit(1);
      }
      switch (frame.type) {
        case 'register': {
          if (frame.protocolVersion !== 2) {
            log(`FAIL: worker speaks protocol ${frame.protocolVersion}, need 2`);
            process.exit(1);
          }
          sawRegister = true;
          // PACKET 22 instrumentation: --real-bundle=<path> serves Ray's
          // actual bundle bytes; --real-props=<path> serves its props body.
          const realBundle = arg('real-bundle');
          const bundleBytes = realBundle
            ? readFileSync(realBundle)
            : readFileSync(path.join(ART, 'bundle.tar.gz'));
          const payloadBytes = readFileSync(path.join(ART, 'payload.tar.gz'));
          // Browser pin: reuse the first-run browser tarball (cached by the
          // supervisor) so cancel timing targets the render, not a 9s cold
          // browser download. Kept a copy when the payload was rebuilt:
          const browserBytes = readFileSync(path.join(ART, 'browser-first.tar.gz'));
          const assign = {
            type: 'jobAssign',
            tenant: 'driffs',
            jobId: 'job-p3-proof',
            attempt: 1,
            kind: 'standard',
            durationFrames: 30,
            fps: 30,
            codec: 'h264',
            bundleSha256: sha256(bundleBytes),
            bundleGetUrl: `http://127.0.0.1:${PORT}/bundle.tar.gz`,
            payloadSha256: sha256(payloadBytes),
            payloadGetUrl: `http://127.0.0.1:${PORT}/payload.tar.gz`,
            browserSha256: sha256(browserBytes),
            browserGetUrl: `http://127.0.0.1:${PORT}/browser.tar.gz`,
            inputPropsGetUrl: `http://127.0.0.1:${PORT}/input-props.json`,
            assetGetUrls: [],
            outputPutUrl: `http://127.0.0.1:${PORT}/output.mp4`,
            outputKey: 'renders/p3/out.mp4',
            purgeAfter: true,
          };
          // Validate our own jobAssign against the real schema before sending:
          // the harness must not send an invalid frame either.
          ServerMessageSchema.parse(assign);
          ws.send(JSON.stringify(assign));
          assignSent = true;
          log('⇒ jobAssign sent');
          if (CANCEL_AFTER > 0) {
            // Cancel is event-driven (on jobProgress ≥ 0.3) — see the
            // jobProgress case. Nothing to schedule here.
          }
          break;
        }
        case 'jobAccepted':
          sawAccepted = true;
          break;
        case 'jobProgress':
          // Event-driven cancel: fire on the first progress ≥ 0.3 so the
          // cancel always lands mid-render regardless of download timing.
          if (CANCEL_AFTER > 0 && !cancelSent && frame.progress >= CANCEL_THRESHOLD) {
            log(`⇒ cancel job-p3-proof (at progress ${frame.progress})`);
            ws.send(JSON.stringify({type: 'cancel', tenant: 'driffs', jobId: 'job-p3-proof'}));
            cancelSent = true;
            if (DROP_CONNECTION_AFTER > 0) {
              // RIP THE SOCKET mid-grace. The supervisor's drain must still
              // complete: TERM -> grace -> KILL -> sweep -> purge. The
              // harness exits only after observing the orphan accounting
              // window (below), so this drop is observable from outside.
              setTimeout(() => {
                log(`⇒ HARD-DROPPING WebSocket (drop-connection-after=${DROP_CONNECTION_AFTER}ms)`);
                ws.close(1001, 'dispatch redeploy');
                server.stop(true); // kill the TCP listener too — hard drop
              }, DROP_CONNECTION_AFTER);
            }
            setTimeout(() => {
              clearTimeout(timer);
              assertCleanAfterCancel();
              finish(0, `cancel settled + ws dropped: completeAfterCancel=${sawAccepted} failedLeaked=${result === 'failed'}`);
            }, 25_000);
          }
          break;
        case 'jobComplete':
          handleJobComplete(frame);
          break;
        case 'jobFailed': {
          clearTimeout(timer);
          log(`jobFailed: ${frame.reason}`);
          // In cancel mode a jobFailed after cancel is EXPECTED-but-suppressed
          // on the real dispatch; here the supervisor itself suppresses it,
          // so receiving one means the cancel path leaked a failure report.
          finish(1, `jobFailed: ${frame.reason}`);
          break;
        }
        default:
          break;
      }
    },
  },
});

log(`dispatch up: http+ws on 127.0.0.1:${PORT}  (cancel-after=${CANCEL_AFTER}ms)`);

if (E2E_EXPECT_FAIL === 'missing-upload') {
  // C-7 self-proof, no supervisor required: feed the jobComplete branch a
  // schema-valid completion whose upload is FORCED missing. The harness must
  // exit 1 — if this ever exits 0, the no-video guard is broken.
  setTimeout(() => {
    const synthetic = {
      type: 'jobComplete',
      tenant: 'driffs',
      jobId: 'job-p3-proof',
      outputKey: 'renders/p3/out.mp4',
      metrics: {frames: 30, wallMs: 1_000, outputSizeInBytes: 0},
    };
    WorkerMessageSchema.parse(synthetic); // the hook must not lie about shape
    log('E2E_EXPECT_FAIL=missing-upload: feeding synthetic jobComplete (upload forced missing)');
    handleJobComplete(synthetic, /*forceMissingUpload=*/ true);
  }, 2_000);
}
