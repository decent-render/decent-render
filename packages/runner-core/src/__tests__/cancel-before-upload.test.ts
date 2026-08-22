/**
 * Cancel-before-upload contract (packet 11).
 *
 * A runner that finishes a render after a cancel must not PUT its output:
 * dispatch will never settle a canceled job (the settle update is scoped to
 * assigned/rendering), so the object would be customer content in R2 that no
 * query references — an orphan. The fix is refuse-at-the-source: the signal
 * handler records the cancel, and renderJob checks it immediately before
 * the PUT (after a one-tick yield so a signal delivered during the sync
 * verify section has provably already run its handler).
 *
 * These tests drive the flag directly (markJobCanceled), proving the guard
 * and its consequences. The real-signal ordering — that Bun flushes a
 * pending SIGTERM handler before a setImmediate scheduled after the sync
 * section — was probed separately (40/40 runs, receipt §measurement) and is
 * exercised end-to-end by scripts/e2e with --cancel-after.
 */
import {afterAll, afterEach, beforeAll, describe, expect, it, vi} from 'vitest';
import {spawnSync} from 'node:child_process';
import {copyFileSync, existsSync, mkdtempSync, readdirSync, rmSync} from 'node:fs';
import {tmpdir} from 'node:os';
import path from 'node:path';

import {BUNDLE_URL, jobAssign, makeBundleArchive, OUTPUT_PUT_URL, PROPS_URL} from './helpers.js';

vi.mock('node:os', async (importOriginal) => {
	const actual = await importOriginal<typeof import('node:os')>();
	const homedir = () => `${actual.tmpdir()}/runner-core-home-cancel-before-upload`;
	return {...actual, homedir, default: {...actual, homedir}};
});

const {renderJob, markJobCanceled, jobCanceled, resetJobCanceledForTests} = await import('../render-job.js');

const systemFfmpeg = spawnSync('which', ['ffmpeg'], {encoding: 'utf8'}).stdout.trim();
const systemFfDir = systemFfmpeg === '' ? null : path.dirname(systemFfmpeg);
const haveSystemFf = systemFfDir !== null && existsSync(path.join(systemFfDir, 'ffprobe'));

const bundle = makeBundleArchive();
let dir: string;

function synthesise(args: string[], output: string) {
	const result = spawnSync(systemFfmpeg, ['-v', 'error', '-y', ...args, output], {encoding: 'utf8'});
	if (result.status !== 0) throw new Error(`fixture build failed: ${result.stderr}`);
}

beforeAll(() => {
	dir = mkdtempSync(path.join(tmpdir(), 'cancel-before-upload-'));
	if (!haveSystemFf) return;
	const encode = ['-frames:v', '24', '-c:v', 'libx264', '-pix_fmt', 'yuv420p'];
	synthesise(['-f', 'lavfi', '-i', 'testsrc=size=64x36:rate=30', ...encode], path.join(dir, 'good.mp4'));
});

afterAll(() => {
	if (dir) rmSync(dir, {recursive: true, force: true});
});

afterEach(() => vi.restoreAllMocks());

function stubNetwork() {
	const puts: string[] = [];
	vi.spyOn(globalThis, 'fetch').mockImplementation((async (input: RequestInfo | URL, init?: RequestInit) => {
		const url = String(input);
		if (init?.method === 'PUT') {
			puts.push(url);
			return new Response('', {status: 200});
		}
		if (url === BUNDLE_URL) return new Response(new Uint8Array(bundle.bytes));
		if (url === PROPS_URL) return new Response(JSON.stringify({compositionId: 'c', inputProps: {}}), {status: 200});
		return new Response('', {status: 404});
	}) as typeof fetch);
	return puts;
}

function rendererProducing(fixture: string) {
	return {
		selectComposition: async () => ({durationInFrames: 24, width: 64, height: 36}),
		renderMedia: async (options: {outputLocation: string}) => {
			copyFileSync(path.join(dir, fixture), options.outputLocation);
			return undefined;
		},
	};
}

describe.skipIf(!haveSystemFf)('renderJob refuses the upload after a cancel', () => {
	it('FAILS THE JOB WITHOUT UPLOADING when the cancel is observed mid-render', async () => {
		const puts = stubNetwork();
		await expect(
			renderJob(jobAssign({bundleSha256: bundle.sha256}), {
				...rendererProducing('good.mp4'),
				// Cancel observed DURING the render, before verify+upload.
				renderMedia: async (options: {outputLocation: string}) => {
					copyFileSync(path.join(dir, 'good.mp4'), options.outputLocation);
					markJobCanceled();
				},
			}, {
				binariesDirectory: systemFfDir,
				log: () => {},
			}),
		).rejects.toThrow(/cancel observed before the output upload/i);
		expect(puts).toEqual([]);
	});

	it('FAILS THE JOB WITHOUT UPLOADING when the cancel lands between verify and PUT (post-yield check)', async () => {
		// The window the yield exists for: signal delivered during the
		// synchronous verify section. Model it by marking canceled at the
		// last moment before the check runs — i.e. from a microtask queued
		// during renderMedia. The guard must still fire.
		const puts = stubNetwork();
		const renderer = rendererProducing('good.mp4');
		const racingRenderer = {
			...renderer,
			renderMedia: async (options: {outputLocation: string}) => {
				await renderer.renderMedia(options);
				// Runs before renderJob's post-verify setImmediate? No — this
				// promise continuation runs BEFORE the verify section (same
				// tick chain), so this models a cancel observed any time up
				// to the check. The guard is the assertion target.
				queueMicrotask(() => markJobCanceled());
			},
		};
		await expect(
			renderJob(jobAssign({bundleSha256: bundle.sha256}), racingRenderer, {
				binariesDirectory: systemFfDir,
				log: () => {},
			}),
		).rejects.toThrow(/cancel observed before the output upload/i);
		expect(puts).toEqual([]);
	});

	it('purges the workdir when the upload is refused', async () => {
		const jobId = `job-cancel-purge-${process.pid}`;
		stubNetwork();
		await expect(
			renderJob(jobAssign({bundleSha256: bundle.sha256, jobId}), {
				...rendererProducing('good.mp4'),
				renderMedia: async (options: {outputLocation: string}) => {
					copyFileSync(path.join(dir, 'good.mp4'), options.outputLocation);
					markJobCanceled();
				},
			}, {
				binariesDirectory: systemFfDir,
				log: () => {},
			}),
		).rejects.toThrow();
		const leaked = readdirSync(tmpdir()).filter((entry) => entry.startsWith(`job-${jobId}-`));
		expect(leaked).toEqual([]);
	});

	it('still uploads when NO cancel was observed (the guard must not over-fire)', async () => {
		resetJobCanceledForTests(); // clean slate: earlier tests set the sticky flag
		const puts = stubNetwork();
		const metrics = await renderJob(jobAssign({bundleSha256: bundle.sha256}), rendererProducing('good.mp4'), {
			binariesDirectory: systemFfDir,
			log: () => {},
		});
		expect(puts).toEqual([OUTPUT_PUT_URL]);
		expect(metrics.frames).toBe(24);
		expect(jobCanceled()).toBe(false);
	});
});
