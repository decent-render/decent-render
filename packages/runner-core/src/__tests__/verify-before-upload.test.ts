import {afterAll, afterEach, beforeAll, describe, expect, it, vi} from 'vitest';
import {spawnSync} from 'node:child_process';
import {copyFileSync, existsSync, mkdtempSync, readdirSync, rmSync} from 'node:fs';
import {tmpdir} from 'node:os';
import path from 'node:path';

import {BUNDLE_URL, jobAssign, makeBundleArchive, OUTPUT_PUT_URL, PROPS_URL} from './helpers.js';

// Same reason as bundle-verify.test.ts: the bundle cache path is resolved at
// module-load time, so homedir must be redirected before render-job loads.
vi.mock('node:os', async (importOriginal) => {
	const actual = await importOriginal<typeof import('node:os')>();
	const homedir = () => `${actual.tmpdir()}/runner-core-home-verify-before-upload`;
	return {...actual, homedir, default: {...actual, homedir}};
});

const {renderJob} = await import('../render-job.js');

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
	dir = mkdtempSync(path.join(tmpdir(), 'verify-before-upload-'));
	if (!haveSystemFf) return;
	const encode = ['-frames:v', '24', '-c:v', 'libx264', '-pix_fmt', 'yuv420p'];
	synthesise(['-f', 'lavfi', '-i', 'testsrc=size=64x36:rate=30', ...encode], path.join(dir, 'good.mp4'));
	synthesise(['-f', 'lavfi', '-i', 'color=c=black:s=64x36:r=30', ...encode], path.join(dir, 'black.mp4'));
});

afterAll(() => {
	if (dir) rmSync(dir, {recursive: true, force: true});
});

afterEach(() => vi.restoreAllMocks());

/** Records every request so we can prove the PUT did or did not happen. */
function stubNetwork() {
	const puts: string[] = [];
	vi.spyOn(globalThis, 'fetch').mockImplementation((async (input: RequestInfo | URL, init?: RequestInit) => {
		const url = String(input);
		if (init?.method === 'PUT') {
			puts.push(url);
			// Packet 15: consume the streamed body (see stream-put.test.ts).
			await new Response(init.body as never).arrayBuffer();
			return new Response('', {status: 200});
		}
		if (url === BUNDLE_URL) return new Response(new Uint8Array(bundle.bytes));
		if (url === PROPS_URL) return new Response(JSON.stringify({compositionId: 'c', inputProps: {}}), {status: 200});
		return new Response('', {status: 404});
	}) as typeof fetch);
	return puts;
}

/**
 * A renderer that produces a REAL video file by copying a fixture into the
 * output location — which is what a healthy or a broken Remotion render
 * respectively leaves behind.
 */
function rendererProducing(fixture: string) {
	return {
		selectComposition: async () => ({durationInFrames: 24, width: 64, height: 36}),
		renderMedia: async (options: {outputLocation: string}) => {
			copyFileSync(path.join(dir, fixture), options.outputLocation);
			return undefined;
		},
	};
}

describe.skipIf(!haveSystemFf)('renderJob verifies before uploading', () => {
	it('uploads a healthy render and reports the MEASURED frame count', async () => {
		const puts = stubNetwork();
		const metrics = await renderJob(jobAssign({bundleSha256: bundle.sha256}), rendererProducing('good.mp4'), {
			binariesDirectory: systemFfDir,
			log: () => {},
		});
		expect(puts).toEqual([OUTPUT_PUT_URL]);
		expect(metrics.frames).toBe(24);
	});

	it('FAILS THE JOB WITHOUT UPLOADING when the render is black', async () => {
		// The whole point: nothing we already know is garbage should reach R2
		// and be settled as a success.
		const puts = stubNetwork();
		await expect(
			renderJob(jobAssign({bundleSha256: bundle.sha256}), rendererProducing('black.mp4'), {
				binariesDirectory: systemFfDir,
				log: () => {},
			}),
		).rejects.toThrow(/uniformly black/i);
		expect(puts).toEqual([]);
	});

	it('purges the workdir even when verification rejects the render', async () => {
		// Unique job id: `job-test-1` is the shared helper default, so scanning
		// tmpdir for it picks up empty workdirs other test files (and older
		// runs) left behind and fails for reasons that have nothing to do with
		// this assertion.
		const jobId = `job-verify-purge-${process.pid}`;
		stubNetwork();
		await expect(
			renderJob(jobAssign({bundleSha256: bundle.sha256, jobId}), rendererProducing('black.mp4'), {
				binariesDirectory: systemFfDir,
				log: () => {},
			}),
		).rejects.toThrow();
		const leaked = readdirSync(tmpdir()).filter((entry) => entry.startsWith(`job-${jobId}-`));
		expect(leaked).toEqual([]);
	});
});
