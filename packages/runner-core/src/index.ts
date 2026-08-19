import {ServerMessageSchema} from '@decent-render/protocol';
import {existsSync, readFileSync} from 'node:fs';
import path from 'node:path';
import type {MinimalComposition, RendererApi} from './renderer-api.js';
import {renderJob} from './render-job.js';

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

function heartbeatIntervalMs(): number {
  const override = Number(process.env.DECENT_RUNNER_HEARTBEAT_MS ?? '');
  return Number.isFinite(override) && override >= 50 ? override : DEFAULT_HEARTBEAT_INTERVAL_MS;
}

export {renderJob} from './render-job.js';
export type {MinimalComposition, RendererApi} from './renderer-api.js';

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

  /**
   * The browser shipped in the payload, next to this binary. The payload writes
   * `chrome/executable` containing the path to the browser relative to the
   * payload root — the publish step knows exactly what it downloaded, so the
   * runner never has to guess a platform-specific nested layout.
   *
   * Returning null is a correctness hazard, not a fallback: Remotion would then
   * download ~1GB into the per-job workdir and lose it to the purge on every
   * job (measured 2026-08-19). Callers should treat null as a broken payload.
   */
  const browserExecutable = () => {
    const payloadRoot = path.dirname(process.execPath);
    const manifest = path.join(payloadRoot, 'chrome', 'executable');
    if (!existsSync(manifest)) return null;
    const relative = readFileSync(manifest, 'utf8').trim();
    if (!relative) return null;
    const resolved = path.join(payloadRoot, relative);
    return existsSync(resolved) ? resolved : null;
  };

  try {
    const frame = ServerMessageSchema.parse(JSON.parse((await readStdin()).trim()));
    if (frame.type !== 'jobAssign') throw new Error(`Expected jobAssign frame, got ${frame.type}`);
    const chrome = browserExecutable();
    if (chrome === null) {
      console.error(
        'WARNING: no browser in payload (chrome/executable missing) — Remotion will download one into the per-job workdir and lose it to the purge on every job.',
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
