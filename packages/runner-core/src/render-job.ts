import type {JobAssignMessage, JobMetrics} from '@decent-render/protocol';
import {spawnSync} from 'node:child_process';
import {createHash} from 'node:crypto';
import {existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync} from 'node:fs';
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
    const composition = await renderer.selectComposition({serveUrl, id: compositionId, inputProps, ...renderOptions});
    let lastReported = 0;
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
    // Verify BEFORE the upload, and fail the job rather than uploading
    // something we already know is broken: nothing we can detect as garbage
    // should ever reach R2 and be reported as a success.
    const probe = verifyRenderedOutput({
      outputLocation,
      expectedFrames: composition.durationInFrames,
      expectedWidth: composition.width,
      expectedHeight: composition.height,
      binariesDirectory: options.binariesDirectory ?? null,
      log,
    });
    const output = readFileSync(outputLocation);
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
    const uploaded = await fetch(assign.outputPutUrl, {method: 'PUT', body: output, headers: {'content-type': assign.codec === 'vp8' ? 'video/webm' : 'video/mp4'}});
    if (!uploaded.ok) throw new Error(`output upload failed: HTTP ${uploaded.status}`);
    // `frames` is the MEASURED count from the file, not the composition's
    // declared duration — the whole point of verifying is that the two can
    // disagree, and the claim is what used to be reported.
    return {wallMs: Date.now() - started, frames: probe.frames, outputSizeInBytes: output.byteLength};
  } finally {
    activeWorkDir = null;
    rmSync(workDir, {recursive: true, force: true});
  }
}
