import type {JobAssignMessage, JobMetrics} from '@decent-render/protocol';
import {spawnSync} from 'node:child_process';
import {createHash} from 'node:crypto';
import {createReadStream} from 'node:fs';
import {existsSync, mkdirSync, mkdtempSync, rmSync, statSync, writeFileSync} from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import type {MinimalComposition, RendererApi} from './renderer-api.js';
import {verifyRenderedOutput} from './verify-output.js';

const bundleCacheDir = path.join(os.homedir(), '.decent-worker', 'bundles');
const defaultLog = (message: string) => process.stderr.write(`${message}\n`);

/**
 * The mkdtemp workdir of the job currently rendering in this process, if any.
 *
 * The runner is single-job by construction (the supervisor spawns one process
 * per jobAssign, concurrency 1), so one slot is exact, not a simplification.
 * `renderJob` registers the dir for its lifetime and purges it in its
 * `finally`; this registry exists so the SIGTERM/SIGINT handler in
 * `runRunner` can also purge it when the signal would otherwise kill the
 * process between the mkdtemp and the `finally` — the "cancel leaves
 * customer content behind" leak. Synchronous `rmSync` on purpose: the signal
 * handler must not await anything to make its exit guarantee.
 */
let activeWorkDir: string | null = null;

/**
 * Set the moment a SIGTERM/SIGINT is observed — i.e. the moment the
 * supervisor's cancel (a group TERM) has reached this process. Once set it is
 * never cleared: a canceled job must not upload, so there is no path back.
 *
 * The one deliberate exception is the signal handler itself, which checks the
 * re-entry guard (`handlingSignal`) rather than this flag, and which must be
 * able to purge even though the flag is already set. purgeActiveWorkDir()
 * likewise keeps working after a cancel — the purge is the point of the
 * handler.
 *
 * `renderJob` checks this immediately before the output PUT: uploading a
 * render after the job was canceled would put customer content into R2 that
 * dispatch will never settle (the settle update is scoped to assigned/
 * rendering states) and no query will ever reference — an orphan in object
 * storage that only a bucket-wide listing can find. Refusing the upload
 * keeps the object from ever existing; there is no second source of truth to
 * reconcile against the job table afterwards.
 *
 * Ordering contract: a cancel observed at ANY point up to and including the
 * event-loop tick immediately before the PUT suppresses the upload. The
 * check runs after `await new Promise(setImmediate)` (see renderJob), and a
 * signal delivered earlier — including during the fully-synchronous verify
 * section, which cannot run JS — is flushed by Bun at event-loop re-entry
 * BEFORE a setImmediate callback scheduled after the sync section (measured
 * 2026-08-22, Bun on macOS arm64: 40/40 probe runs). What remains is the
 * window after the check: a signal arriving while the PUT itself is on the
 * wire cannot stop bytes already sent. That window is the PUT duration; see
 * the packet-11 receipt for its measured width.
 */
let cancelObserved = false;

/** Visible for tests: the guard renderJob checks before the output PUT. */
export function jobCanceled(): boolean {
  return cancelObserved;
}

/**
 * Mark the job canceled. Called by runRunner's signal handler on SIGTERM/
 * SIGINT; exported so a payload entry point (or a test) can drive the same
 * transition without a real signal.
 */
export function markJobCanceled(): void {
  cancelObserved = true;
}

/**
 * TEST-ONLY: clear the cancel flag. Production code must never call this —
 * a canceled job stays canceled. It exists because the flag is module state
 * and vitest runs every test in one module instance, so the "no cancel was
 * observed" control test needs an explicit clean slate.
 */
export function resetJobCanceledForTests(): void {
  cancelObserved = false;
}

/**
 * Purge the active workdir, if one is registered. Safe to call at any time
 * (no-op when no job is rendering). Rethrows fs errors to the caller, which
 * decides whether they are fatal — on the normal path they propagate into
 * renderJob's own failure handling; on the signal path the handler swallows
 * them because exiting still matters more.
 */
export function purgeActiveWorkDir(): void {
  const dir = activeWorkDir;
  if (dir === null) return;
  activeWorkDir = null;
  rmSync(dir, {recursive: true, force: true});
}

type Options = {
  onProgress?: (progress: number) => void;
  binariesDirectory?: string | null;
  /** Browser shipped in the payload — see RendererApi.browserExecutable. */
  browserExecutable?: string | null;
  log?: (message: string) => void;
};

async function ensureBundle(sha256: string, getUrl: string, log: (message: string) => unknown = defaultLog): Promise<string> {
  const dir = path.join(bundleCacheDir, sha256);
  if (existsSync(path.join(dir, 'index.html'))) return dir;
  const response = await fetch(getUrl);
  if (!response.ok) throw new Error(`bundle download failed: HTTP ${response.status}`);
  const bytes = new Uint8Array(await response.arrayBuffer());
  const actual = createHash('sha256').update(bytes).digest('hex');
  if (actual !== sha256) throw new Error(`bundle sha mismatch: expected ${sha256}, got ${actual}`);
  const temp = mkdtempSync(path.join(os.tmpdir(), 'bundle-dl-'));
  const archive = path.join(temp, 'bundle.tar.gz');
  writeFileSync(archive, bytes);
  mkdirSync(dir, {recursive: true});
  const extracted = spawnSync('tar', ['-xzf', archive, '-C', dir]);
  rmSync(temp, {recursive: true, force: true});
  if (extracted.status !== 0) {
    rmSync(dir, {recursive: true, force: true});
    throw new Error(`tar extract failed: ${extracted.stderr.toString()}`);
  }
  log(`bundle ${sha256.slice(0, 12)} verified and extracted`);
  return dir;
}

/**
 * Deterministically release a streamed PUT body. If fetch never fully
 * consumed it (early throw, refused connection), the file read stream may
 * still be pending when renderJob's finally purges the workdir — an
 * unhandled ENOENT in the best case. Draining via Response (which both
 * runtimes expose) or cancelling via .cancel() when present closes the
 * stream before the purge runs. Best-effort by design: the upload has
 * already failed; this exists so it fails QUIETLY.
 */
async function discardBody(body: NonNullable<Parameters<typeof fetch>[1]>['body']): Promise<void> {
  if (body === null || body === undefined) return;
  const maybeStream = body as {cancel?: () => Promise<void>};
  if (typeof maybeStream.cancel === 'function') {
    await maybeStream.cancel();
    return;
  }
  await new Response(body as NonNullable<Parameters<typeof fetch>[1]>['body']).arrayBuffer();
}

/**
 * PACKET 25 (0.1.3): match a renderer/GPU-init hang that Remotion surfaces
 * as a delayRender timeout — `A delayRender() "…" was called but not
 * cleared after Nms`.
 *
 * Breadth decision: match ANY delayRender timeout, not a
 * ThreeCanvas-labeled one. The packet-22 incident was misdiagnosed for
 * hours partly BECAUSE the label ("<ThreeCanvas/>") is tenant-authored —
 * the failing riff used a WebGPU wrapper that reused the label. Matching
 * labels means chasing tenant copy; matching Remotion's runtime FORMAT
 * catches every GPU-init hang whatever the composition names it. The
 * cost of breadth — one extra attempt (~28s) on a deterministically
 * broken bundle — is bounded by exactly-one-retry and is what a human
 * operator would do anyway.
 */
export function isDelayRenderTimeout(message: string): boolean {
  return (
    message.includes('A delayRender()') &&
    message.includes('was called but not cleared after')
  );
}

export async function renderJob<TComposition extends MinimalComposition>(
  assign: JobAssignMessage,
  renderer: RendererApi<TComposition>,
  options: Options = {},
): Promise<JobMetrics & {outputSizeInBytes: number}> {
  const started = Date.now();
  const log = options.log ?? defaultLog;
  const serveUrl = await ensureBundle(assign.bundleSha256, assign.bundleGetUrl, log);
  const propsResponse = await fetch(assign.inputPropsGetUrl);
  if (!propsResponse.ok) throw new Error(`input props fetch failed: HTTP ${propsResponse.status}`);
  const {compositionId, inputProps} = await propsResponse.json() as {compositionId: string; inputProps: Record<string, unknown>};
  const workDir = mkdtempSync(path.join(os.tmpdir(), `job-${assign.jobId}-`));
  activeWorkDir = workDir;
  try {
    const outputLocation = path.join(workDir, assign.codec === 'vp8' ? 'out.webm' : 'out.mp4');
    const renderOptions = {
      binariesDirectory: options.binariesDirectory ?? null,
      browserExecutable: options.browserExecutable ?? null,
      chromeMode: 'chrome-for-testing',
      chromiumOptions: {gl: 'angle'},
    } as const;
    // PACKET 25 (0.1.3): retry the WHOLE render (select + renderMedia —
    // both hang under the packet-22 GPU-adapter contention) exactly ONCE
    // when the first attempt dies with a delayRender timeout. The hung
    // navigator.gpu.requestAdapter() promise dies with the Chrome that
    // owns it; a fresh Chrome usually wins the adapter (packet-22 PROVEN:
    // concurrent repro, stagger trials). Bounded: no second retry, no
    // retry for other failures, cancel aborts before the retry starts,
    // and wallMs keeps counting across attempts (the caller's ceiling
    // sees the true cost).
    let composition: TComposition | undefined;
    let lastReported = 0;
    const attemptRender = async (attempt: 1 | 2): Promise<void> => {
      composition = await renderer.selectComposition({serveUrl, id: compositionId, inputProps, ...renderOptions});
      lastReported = 0; // the retry restarts the progress curve from 0
      await renderer.renderMedia({
        serveUrl,
        composition,
        inputProps,
        codec: assign.codec === 'vp8' ? 'vp8' : 'h264',
        colorSpace: 'bt709',
        outputLocation,
        concurrency: 1,
        ...renderOptions,
        onProgress: ({progress}) => {
          if (progress - lastReported >= 0.05 || progress === 1) {
            lastReported = progress;
            options.onProgress?.(progress);
          }
        },
      });
    };
    try {
      await attemptRender(1);
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      if (!isDelayRenderTimeout(message)) throw error;
      if (jobCanceled()) throw error; // a canceled job must not retry
      // Visible by design (Ray's traceability theme): the leased log line
      // names the retry, the trigger, and the attempt number.
      log(
        `[retry] attempt 1 failed with a delayRender timeout (renderer/GPU init hang, packet-22 class) — ` +
          `retrying the render once (attempt 2 of 2): ${message}`,
      );
      await attemptRender(2);
    }
    // Verify BEFORE the upload, and fail the job rather than uploading
    // something we already know is broken: nothing we can detect as garbage
    // should ever reach R2 and be reported as a success.
    const done = composition as TComposition; // attemptRender assigns before returning
    const probe = verifyRenderedOutput({
      outputLocation,
      expectedFrames: done.durationInFrames,
      expectedWidth: done.width,
      expectedHeight: done.height,
      binariesDirectory: options.binariesDirectory ?? null,
      log,
    });
    const outputSize = statSync(outputLocation).size;
    // Refuse the upload if a cancel has been observed. The `await` yield is
    // load-bearing: verifyRenderedOutput above is fully synchronous, so a
    // SIGTERM delivered during it cannot run the handler until JS execution
    // resumes. Bun flushes a pending signal handler at event-loop re-entry
    // BEFORE a setImmediate scheduled after the sync section (measured,
    // 40/40) — so yielding one tick here means the handler (which sets the
    // cancel flag and exits the process) has provably already run if the
    // signal arrived before this point. Without the yield the same thing
    // happened in practice, but only by luck of the runtime's internal
    // ordering, not by contract.
    await new Promise<void>((resolve) => setImmediate(resolve));
    if (jobCanceled()) {
      throw new Error(
        'cancel observed before the output upload — refusing to upload a canceled job (the workdir purge in this finally block is the cleanup)',
      );
    }
    // STREAM the file as the PUT body — never buffer it. The size cap allows
    // up to 2 GiB of output, and readFileSync would commit that much memory
    // per job (packet 9's OWED). S3-compatible presigned PUTs reject chunked
    // transfer encoding, so the body must carry Content-Length:
    //   - Bun (the payload runtime): Bun.file sets Content-Length from the
    //     file size (probed at the raw-socket level, packet 15).
    //   - Node (the vitest runtime): a stream body needs duplex:'half' and
    //     an EXPLICIT content-length header — without it undici rejects, and
    //     Bun would strip it to chunked anyway (probed both ways).
    // BodyInit is not in this tsconfig's libs; derive it from fetch's own
    // RequestInit so the type tracks whatever the runtime defines.
    let putBody: NonNullable<Parameters<typeof fetch>[1]>['body'];
    let putHeaders: Record<string, string> = {'content-type': assign.codec === 'vp8' ? 'video/webm' : 'video/mp4'};
    if (typeof Bun !== 'undefined' && typeof Bun.file === 'function') {
      putBody = Bun.file(outputLocation);
    } else {
      putBody = createReadStream(outputLocation) as unknown as ReadableStream;
      putHeaders = {...putHeaders, 'content-length': String(outputSize), duplex: 'half'} as Record<string, string>;
    }
    let uploaded: Response;
    try {
      uploaded = await fetch(assign.outputPutUrl, {
      method: 'PUT',
      body: putBody,
      // The duplex hint is ignored by Bun and required by Node's undici for
      // stream bodies; typing it through a cast keeps one call site.
      ...(putBody instanceof ReadableStream || typeof (putBody as {pipe?: unknown}).pipe === 'function'
        ? {duplex: 'half' as const}
        : {}),
      headers: putHeaders,
      } as RequestInit);
    } catch (err) {
      await discardBody(putBody).catch(() => {});
      throw err;
    }
    if (!uploaded.ok) {
      // A streamed body may still be opening/reading when fetch rejects or
      // returns early; make sure nothing can touch the workdir after the
      // finally-purge below. (Not theoretical: under a refused connection
      // the stream-open races the purge and surfaces as an unhandled ENOENT
      // — caught by the ffmpeg-hidden CI-parity run, packet 15.)
      await discardBody(putBody).catch(() => {});
      throw new Error(`output upload failed: HTTP ${uploaded.status}`);
    }
    // `frames` is the MEASURED count from the file, not the composition's
    // declared duration — the whole point of verifying is that the two can
    // disagree, and the claim is what used to be reported.
    return {wallMs: Date.now() - started, frames: probe.frames, outputSizeInBytes: outputSize};
  } finally {
    activeWorkDir = null;
    rmSync(workDir, {recursive: true, force: true});
  }
}
