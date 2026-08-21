import type {JobAssignMessage, JobMetrics} from '@decent-render/protocol';
import {spawnSync} from 'node:child_process';
import {createHash} from 'node:crypto';
import {existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync} from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import type {MinimalComposition, RendererApi} from './renderer-api.js';

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
    const output = readFileSync(outputLocation);
    const uploaded = await fetch(assign.outputPutUrl, {method: 'PUT', body: output, headers: {'content-type': assign.codec === 'vp8' ? 'video/webm' : 'video/mp4'}});
    if (!uploaded.ok) throw new Error(`output upload failed: HTTP ${uploaded.status}`);
    return {wallMs: Date.now() - started, frames: composition.durationInFrames, outputSizeInBytes: output.byteLength};
  } finally {
    activeWorkDir = null;
    rmSync(workDir, {recursive: true, force: true});
  }
}
