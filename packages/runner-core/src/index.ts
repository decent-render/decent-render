import {ServerMessageSchema} from '@decent-render/protocol';
import {existsSync, readFileSync} from 'node:fs';
import path from 'node:path';
import type {MinimalComposition, RendererApi} from './renderer-api.js';
import {renderJob, purgeActiveWorkDir} from './render-job.js';

/**
 * How often the runner proves it is alive when no progress is being reported.
 *
 * Only emitted during genuine silence — any progress report resets the timer —
 * so a normally-progressing render never sends one. Sized against the
 * supervisor's SILENCE_TIMEOUT (120s, supervisor-core/src/runner.rs:18): 30s
 * leaves a 4x margin, so three consecutive heartbeats can be lost or delayed
 * before a healthy job is killed.
 *
 * `DECENT_RUNNER_HEARTBEAT_MS` overrides it (floor 50ms) so tests can exercise
 * the path in milliseconds instead of waiting out the real interval.
 */
const DEFAULT_HEARTBEAT_INTERVAL_MS = 30_000;

/**
 * Hard ceiling on how long the SIGTERM/SIGINT handler may take before the
 * process force-exits (9). The handler's only synchronous work is an rmSync
 * of the workdir, which is normally sub-second; the ceiling exists so a
 * pathological dir (or a wedged event loop) can never leave a canceled job's
 * process lingering after the operator asked twice.
 */
const SIGNAL_EXIT_DEADLINE_MS = 5_000;

function heartbeatIntervalMs(): number {
  const override = Number(process.env.DECENT_RUNNER_HEARTBEAT_MS ?? '');
  return Number.isFinite(override) && override >= 50 ? override : DEFAULT_HEARTBEAT_INTERVAL_MS;
}

export {renderJob} from './render-job.js';
export {verifyRenderedOutput} from './verify-output.js';
export type {OutputProbe, VerifyOptions} from './verify-output.js';
export type {MinimalComposition, RendererApi} from './renderer-api.js';

/**
 * Where the browser is, in preference order.
 *
 * 1. `DECENT_BROWSER_EXECUTABLE` — the supervisor fetched a standalone,
 *    sha-verified browser artifact and resolved it to a local path. This is the
 *    production path: the browser is ~170MB and identical across Remotion
 *    versions pinning the same Chrome, so it is cached once per Chrome version
 *    instead of being re-shipped inside every payload.
 * 2. `chrome/executable` next to the runner binary — a payload that bundles its
 *    own browser. The manifest holds the path relative to the payload root, so
 *    the runner never guesses a platform-specific nested layout. Kept for
 *    payloads published before the split, and used by the bench harness.
 *
 * Returning null is a correctness hazard, not a fallback: Remotion would then
 * download ~1GB into the per-job workdir and lose it to the purge on every job
 * (measured 2026-08-19). Callers must treat null as a broken setup, not a
 * default.
 *
 * Exported so the tests exercise this function rather than a copy of it.
 */
export function resolveBrowserExecutable(
  payloadRoot: string,
  env: Record<string, string | undefined> = process.env,
): string | null {
  const injected = env.DECENT_BROWSER_EXECUTABLE;
  // An injected path that does not exist falls through to the payload rather
  // than failing: the supervisor already verified the artifact, so a stale or
  // hand-set variable should not take a working payload offline.
  if (injected && existsSync(injected)) return injected;
  const manifest = path.join(payloadRoot, 'chrome', 'executable');
  if (!existsSync(manifest)) return null;
  const relative = readFileSync(manifest, 'utf8').trim();
  if (!relative) return null;
  const resolved = path.join(payloadRoot, relative);
  return existsSync(resolved) ? resolved : null;
}

/**
 * Full runner main loop: reads a jobAssign frame from stdin, renders it with
 * the injected `@remotion/renderer` functions, emits protocol frames on
 * stdout, and exits. Each versioned runner app's entry point is just:
 *
 *   import {renderMedia, selectComposition} from '@remotion/renderer';
 *   import {runRunner} from '@decent-render/runner-core';
 *   await runRunner({renderMedia, selectComposition});
 *
 * The renderer import MUST stay in the app entry file so that
 * `bun build --compile` resolves it against the app's own pinned version.
 */
export async function runRunner<TComposition extends MinimalComposition>(renderer: RendererApi<TComposition>): Promise<never> {
  const protocolWrite = process.stdout.write.bind(process.stdout);
  const stderrWrite = process.stderr.write.bind(process.stderr);
  process.stdout.write = ((chunk: unknown, ...args: unknown[]) => stderrWrite(chunk as string | Uint8Array, ...(args as []))) as typeof process.stdout.write;
  const writeEvent = (event: Record<string, unknown>) => protocolWrite(`${JSON.stringify(event)}\n`);
  const readStdin = async () => {
    const chunks: Uint8Array[] = [];
    for await (const chunk of Bun.stdin.stream()) chunks.push(chunk);
    return Buffer.concat(chunks).toString('utf8');
  };
  const binariesDirectory = () => {
    const dir = path.join(path.dirname(process.execPath), 'remotion-binaries');
    return existsSync(path.join(dir, 'remotion')) ? dir : null;
  };

  // Cancel containment: the supervisor terminates the whole process group on
  // cancel, which delivers SIGTERM to this process. Without a handler, the
  // default disposition kills Bun instantly and renderJob's `finally` (which
  // purges the mkdtemp workdir — customer content) never runs. Handling the
  // signal makes the purge happen, then we exit non-zero as a canceled job
  // must. The supervisor ignores our exit status after a cancel (its
  // canceled_job marker suppresses the spurious jobFailed), so a clean
  // non-zero exit is correct here.
  //
  // Hardened against re-entry and hangs:
  // - a second signal force-exits immediately (the purge may be slow on a
  //   huge dir; the operator's second Ctrl-C must always win);
  // - purge errors are swallowed-and-logged — exiting is still more urgent
  //   than a perfect purge;
  // - a watchdog ARMED ONLY ON SIGNAL RECEIPT force-exits if the handler
  //   somehow exceeds SIGNAL_EXIT_DEADLINE_MS. Arming at startup would kill
  //   every render longer than the deadline, which is why it lives inside
  //   the handler.
  let handlingSignal = false;
  const exitFromSignal = (signal: NodeJS.Signals, code: number) => {
    if (handlingSignal) {
      process.exit(9);
    }
    handlingSignal = true;
    setTimeout(() => process.exit(9), SIGNAL_EXIT_DEADLINE_MS);
    try {
      purgeActiveWorkDir();
    } catch (error) {
      process.stderr.write(`workdir purge on ${signal} failed: ${String(error)}\n`);
    }
    process.exit(code);
  };
  process.on('SIGTERM', () => exitFromSignal('SIGTERM', 143));
  process.on('SIGINT', () => exitFromSignal('SIGINT', 130));

  try {
    const frame = ServerMessageSchema.parse(JSON.parse((await readStdin()).trim()));
    if (frame.type !== 'jobAssign') throw new Error(`Expected jobAssign frame, got ${frame.type}`);
    const chrome = resolveBrowserExecutable(path.dirname(process.execPath));
    if (chrome === null) {
      console.error(
        'WARNING: no browser resolved (DECENT_BROWSER_EXECUTABLE unset and no chrome/executable in payload) — Remotion will download one into the per-job workdir and lose it to the purge on every job.',
      );
    }
    // Liveness, independent of render progress.
    //
    // The supervisor kills a job after SILENCE_TIMEOUT (120s) without a line on
    // stdout (runner.rs:18). Progress is throttled to 5% deltas, so a heavy
    // composition can legitimately exceed that between reports. Until
    // 2026-08-19 this was masked by an accident: Remotion's Chrome-download logs
    // leaked onto stdout and reset the timer. Shipping the browser in the
    // payload removed those lines (verified: 0 leaked lines warm), so liveness
    // has to be explicit or heavy renders would start dying.
    let sawActivity = true;
    const heartbeat = setInterval(() => {
      if (!sawActivity) writeEvent({type: 'heartbeat'});
      sawActivity = false;
    }, heartbeatIntervalMs());

    const metrics = await renderJob(frame, renderer, {
      binariesDirectory: binariesDirectory(),
      browserExecutable: chrome,
      log: (message) => console.error(message),
      onProgress: (progress) => {
        sawActivity = true;
        writeEvent({type: 'progress', progress});
      },
    }).finally(() => clearInterval(heartbeat));
    writeEvent({type: 'done', outputSizeInBytes: metrics.outputSizeInBytes, wallTimeMs: metrics.wallMs, metrics});
    process.exit(0);
  } catch (error) {
    writeEvent({type: 'error', message: error instanceof Error ? error.message : String(error)});
    process.exit(1);
  }
}
