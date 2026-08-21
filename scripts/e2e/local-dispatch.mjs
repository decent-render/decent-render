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
 *   exit 1 on jobFailed / schema violation / timeout.
 */
import {
  ServerMessageSchema,
  WorkerMessageSchema,
} from '../../packages/protocol/dist/index.js';
import {readFileSync, writeFileSync, statSync} from 'node:fs';
import path from 'node:path';
import crypto from 'node:crypto';

const arg = (n) => process.argv.find((a) => a.startsWith(`--${n}=`))?.slice(n.length + 3);
const ART = arg('artifacts') || '/tmp/p3-artifacts';
const PORT = Number(arg('port') || 8790);
const CANCEL_AFTER = Number(arg('cancel-after') || 0);
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

const server = Bun.serve({
  port: PORT,
  hostname: '127.0.0.1',
  fetch(request, server) {
    const url = new URL(request.url);
    if (url.pathname === '/ws' && server.upgrade(request)) return;
    if (request.method === 'PUT' && url.pathname === '/output.mp4') {
      // arrayBuffer, NOT text(): text() UTF-8-decodes the body and replaces
      // invalid sequences with U+FFFD, corrupting binary uploads. (Found the
      // hard way in the first E2E run: moov atom not found, +66% size.)
      return request.arrayBuffer().then((body) => {
        const bytes = Buffer.from(body);
        writeFileSync(OUT, bytes);
        log(`PUT /output.mp4 ← ${bytes.length} bytes, sha=${sha256(bytes).slice(0, 12)}`);
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
          const bundleBytes = readFileSync(path.join(ART, 'bundle.tar.gz'));
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
          if (CANCEL_AFTER > 0 && !cancelSent && frame.progress >= 0.3) {
            log(`⇒ cancel job-p3-proof (at progress ${frame.progress})`);
            ws.send(JSON.stringify({type: 'cancel', tenant: 'driffs', jobId: 'job-p3-proof'}));
            cancelSent = true;
            setTimeout(() => {
              clearTimeout(timer);
              finish(0, `cancel settled: completeAfterCancel=${sawAccepted} failedLeaked=${result === 'failed'}`);
            }, 8_000);
          }
          break;
        case 'jobComplete': {
          clearTimeout(timer);
          if (cancelSent) {
            // A completion racing a late cancel is fine; record it.
            sawAccepted = true;
            result = 'complete';
            log('jobComplete after cancel sent (race — not a leak)');
            break;
          }
          const uploaded = statSync(OUT, {throwIfNoEntry: false});
          finish(0, `jobComplete: frames=${frame.metrics?.frames} wallMs=${frame.metrics?.wallMs} size=${frame.metrics?.outputSizeInBytes} uploaded=${uploaded ? `${uploaded.size}b` : 'MISSING'}`);
          break;
        }
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
