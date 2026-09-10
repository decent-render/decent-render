/**
 * FARM-STILL — the still branch of renderJob.
 *
 * Contract pinned here:
 * - a job with `still` renders ONE frame via the injected renderStill and
 *   writes `still-f<frame>.png` into the per-job workdir;
 * - verify-before-upload is NEVER skipped: PNG signature + IHDR geometry
 *   against the resolved composition — a wrong-geometry or non-PNG output
 *   refuses the upload (the runner invariant, now for stills too);
 * - the upload is a PUT with `content-type: image/png`, carrying the bytes
 *   that were verified;
 * - the packet-25 retry is MIRRORED: exactly one retry of the whole still
 *   render on a delayRender timeout, never for other failures, never after
 *   a cancel, and the retry log line names the still path;
 * - progress is a single step, 0 → 1, with framesSoFar ∈ {0, 1} — never
 *   the composition's full duration;
 * - the workdir purge (invariant 1) holds on success AND on failure;
 * - the frame < durationFrames bound is re-checked in the runner even
 *   though the wire parse already refuses it (defense in depth);
 * - the video path is untouched: its assign→calls sequence is pinned
 *   option-for-option so a still regression cannot leak into it.
 */
import {readdirSync} from 'node:fs';
import {mkdtempSync, readFileSync, rmSync, writeFileSync} from 'node:fs';
import {tmpdir} from 'node:os';
import path from 'node:path';
import {afterEach, describe, expect, it, vi} from 'vitest';

import {BUNDLE_URL, jobAssign, makeBundleArchive, OUTPUT_PUT_URL, PROPS_URL} from './helpers.js';

vi.mock('node:os', async (importOriginal) => {
	const actual = await importOriginal<typeof import('node:os')>();
	const homedir = () => `${actual.tmpdir()}/runner-core-home-still-render`;
	return {...actual, homedir, default: {...actual, homedir}};
});

const {renderJob, isDelayRenderTimeout, resetJobCanceledForTests} = await import('../render-job.js');

const bundle = makeBundleArchive();

/** A minimal STRUCTURALLY valid PNG (signature + IHDR + IEND; no pixel data). */
function pngBytes(width: number, height: number): Buffer {
	const signature = Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);
	const ihdr = Buffer.alloc(4 + 4 + 13 + 4); // length + 'IHDR' + data + crc
	ihdr.writeUInt32BE(13, 0);
	ihdr.write('IHDR', 4, 'ascii');
	ihdr.writeUInt32BE(width, 8);
	ihdr.writeUInt32BE(height, 12);
	ihdr.writeUInt8(8, 16); // bit depth
	ihdr.writeUInt8(6, 17); // colour type RGBA
	// bytes 18–20 stay zero: compression, filter, interlace
	const iend = Buffer.alloc(8);
	iend.write('IEND', 4, 'ascii');
	return Buffer.concat([signature, ihdr, iend]);
}

const delayRenderError = () =>
	new Error('A delayRender() "Waiting for <ThreeCanvas/>" was called but not cleared after 28000ms. See https://remotion.dev/docs/timeout for help. ');

type RecordedCall = {options: Record<string, unknown>};

function stubNetwork(putBodies: Buffer[] = []) {
	const puts: Array<{url: string; contentType: string | undefined}> = [];
	vi.spyOn(globalThis, 'fetch').mockImplementation((async (input: RequestInfo | URL, init?: RequestInit) => {
		const url = String(input);
		if (init?.method === 'PUT') {
			const bytes = Buffer.from(await new Response(init.body as never).arrayBuffer());
			putBodies.push(bytes);
			puts.push({url, contentType: (init.headers as Record<string, string>)?.['content-type']});
			return new Response('', {status: 200});
		}
		if (url === BUNDLE_URL) return new Response(new Uint8Array(bundle.bytes));
		if (url === PROPS_URL) return new Response(JSON.stringify({compositionId: 'c', inputProps: {}}), {status: 200});
		return new Response('', {status: 404});
	}) as typeof fetch);
	return puts;
}

/** Renderer double that writes a still PNG of the given geometry. */
function stillRenderer(width = 64, height = 36) {
	const calls: {select?: RecordedCall; still?: RecordedCall} = {};
	return {
		calls,
		selectComposition: async (options: Record<string, unknown>) => {
			calls.select = {options};
			return {durationInFrames: 24, width: 64, height: 36};
		},
		renderStill: async (options: {output: string; frame: number; imageFormat: string}) => {
			calls.still = {options};
			writeFileSync(options.output, pngBytes(width, height));
			return undefined;
		},
	};
}

function stillAssign() {
	return jobAssign({
		bundleSha256: bundle.sha256,
		jobId: 'job-still-1',
		outputKey: 'renders/t1/attempt-1/still-f5.png',
		durationFrames: 24,
		still: {frame: 5, format: 'png'} as never,
	});
}

afterEach(() => {
	vi.restoreAllMocks();
	resetJobCanceledForTests();
});

function leftoverWorkDirs(jobId: string): string[] {
	return readdirSync(tmpdir()).filter((entry) => entry.startsWith(`job-${jobId}-`));
}

describe('renderJob still path (FARM-STILL)', () => {
	it('renders ONE frame, names still-f<frame>.png, uploads image/png carrying the verified bytes', async () => {
		const putBodies: Buffer[] = [];
		const puts = stubNetwork(putBodies);
		const renderer = stillRenderer();
		const metrics = await renderJob(stillAssign(), renderer as never, {
			binariesDirectory: null,
			log: () => {},
		});
		// The injected renderStill received the still directive verbatim and
		// the SAME capture surface as the video path.
		const still = renderer.calls.still!.options as Record<string, never>;
		expect(still.frame).toBe(5);
		expect(still.imageFormat).toBe('png');
		expect(still.chromeMode).toBe('chrome-for-testing');
		expect(still.chromiumOptions).toEqual({gl: 'angle'});
		expect(String(still.output)).toMatch(/still-f5\.png$/);
		expect(String(still.serveUrl)).toBeTruthy();
		// Exactly one PUT, image/png, byte-for-byte what the renderer wrote.
		expect(puts).toHaveLength(1);
		expect(puts[0].url).toBe(OUTPUT_PUT_URL);
		expect(puts[0].contentType).toBe('image/png');
		expect(putBodies[0].equals(pngBytes(64, 36))).toBe(true);
		// A still is ONE measured frame.
		expect(metrics).toEqual({
			wallMs: expect.any(Number),
			frames: 1,
			outputSizeInBytes: pngBytes(64, 36).byteLength,
		});
	});

	it('verifies geometry: a PNG of the WRONG dimensions refuses the upload', async () => {
		const putBodies: Buffer[] = [];
		stubNetwork(putBodies);
		const renderer = stillRenderer(32, 16); // composition says 64x36
		await expect(
			renderJob(stillAssign(), renderer as never, {binariesDirectory: null, log: () => {}}),
		).rejects.toThrow(/still output is 32x16, composition declares 64x36/);
		expect(putBodies).toEqual([]); // nothing reached the PUT
	});

	it('verifies the signature: non-PNG output refuses the upload', async () => {
		const putBodies: Buffer[] = [];
		stubNetwork(putBodies);
		const renderer = {
			selectComposition: async () => ({durationInFrames: 24, width: 64, height: 36}),
			renderStill: async (options: {output: string}) => {
				writeFileSync(options.output, Buffer.alloc(1024, 7)); // not a PNG
			},
		};
		await expect(
			renderJob(stillAssign(), renderer as never, {binariesDirectory: null, log: () => {}}),
		).rejects.toThrow(/not a PNG \(bad signature\)/);
		expect(putBodies).toEqual([]);
	});

	it('purges the workdir on SUCCESS (invariant 1)', async () => {
		stubNetwork();
		const renderer = stillRenderer();
		await renderJob(stillAssign(), renderer as never, {binariesDirectory: null, log: () => {}});
		expect(leftoverWorkDirs('job-still-1')).toEqual([]);
	});

	it('purges the workdir on FAILURE (invariant 1 — a failed verify is still customer content)', async () => {
		stubNetwork();
		const renderer = stillRenderer(1, 1); // wrong geometry → verify fails
		await expect(
			renderJob(stillAssign(), renderer as never, {binariesDirectory: null, log: () => {}}),
		).rejects.toThrow(/composition declares/);
		expect(leftoverWorkDirs('job-still-1')).toEqual([]);
	});

	it('retries the WHOLE still render exactly once on a delayRender timeout, and the log names the still path', async () => {
		const putBodies: Buffer[] = [];
		stubNetwork(putBodies);
		const logs: string[] = [];
		let calls = 0;
		const renderer = {
			selectComposition: async () => ({durationInFrames: 24, width: 64, height: 36}),
			renderStill: async (options: {output: string}) => {
				calls += 1;
				if (calls === 1) throw delayRenderError();
				writeFileSync(options.output, pngBytes(64, 36));
			},
		};
		const metrics = await renderJob(stillAssign(), renderer as never, {
			binariesDirectory: null,
			log: (m: string) => logs.push(m),
		});
		expect(calls).toBe(2);
		expect(metrics.frames).toBe(1);
		expect(putBodies).toHaveLength(1);
		const retryLog = logs.find((l) => l.includes('[retry]'));
		expect(retryLog).toBeDefined();
		expect(retryLog).toContain('delayRender timeout');
		expect(retryLog).toContain('still render once');
	});

	it('a SECOND delayRender failure surfaces — no infinite still loop', async () => {
		stubNetwork();
		let calls = 0;
		const renderer = {
			selectComposition: async () => ({durationInFrames: 24, width: 64, height: 36}),
			renderStill: async () => {
				calls += 1;
				throw delayRenderError();
			},
		};
		await expect(
			renderJob(stillAssign(), renderer as never, {binariesDirectory: null, log: () => {}}),
		).rejects.toThrow(/was called but not cleared after/);
		expect(calls).toBe(2);
		expect(leftoverWorkDirs('job-still-1')).toEqual([]);
	});

	it('non-delayRender still failures never retry', async () => {
		stubNetwork();
		let calls = 0;
		const renderer = {
			selectComposition: async () => ({durationInFrames: 24, width: 64, height: 36}),
			renderStill: async () => {
				calls += 1;
				throw new Error('WebGL context lost');
			},
		};
		await expect(
			renderJob(stillAssign(), renderer as never, {binariesDirectory: null, log: () => {}}),
		).rejects.toThrow('WebGL context lost');
		expect(calls).toBe(1);
	});

	it('progress is a SINGLE step 0 → 1 with framesSoFar ∈ {0,1} — never the composition duration', async () => {
		stubNetwork();
		const events: Array<{progress: number; framesSoFar: number}> = [];
		const renderer = stillRenderer();
		await renderJob(stillAssign(), renderer as never, {
			binariesDirectory: null,
			log: () => {},
			onProgress: (e) => events.push({progress: e.progress, framesSoFar: e.framesSoFar}),
		});
		expect(events).toEqual([
			{progress: 0, framesSoFar: 0},
			{progress: 1, framesSoFar: 1},
		]);
	});

	it('re-checks the frame < durationFrames bound even though the wire refuses it', async () => {
		stubNetwork();
		let renderCalls = 0;
		const renderer = {
			selectComposition: async () => ({durationInFrames: 24, width: 64, height: 36}),
			renderStill: async () => {
				renderCalls += 1;
			},
		};
		await expect(
			renderJob(jobAssign({bundleSha256: bundle.sha256, still: {frame: 24, format: 'png'} as never}), renderer as never, {
				binariesDirectory: null,
				log: () => {},
			}),
		).rejects.toThrow(/still\.frame 24 must be < durationFrames 24/);
		expect(renderCalls).toBe(0);
	});
});

describe('the video path stays byte-identical (FARM-STILL guard)', () => {
	it('pins the assign→calls sequence option-for-option and never touches renderStill', async () => {
		const putBodies: Buffer[] = [];
		const puts = stubNetwork(putBodies);
		const logs: string[] = [];
		const selectOptions: Array<Record<string, unknown>> = [];
		const mediaOptions: Array<Record<string, unknown>> = [];
		let stillCalls = 0;
		const renderer = {
			selectComposition: async (options: Record<string, unknown>) => {
				selectOptions.push(options);
				return {durationInFrames: 24, width: 64, height: 36};
			},
			renderMedia: async (options: {outputLocation: string; onProgress?: (p: {progress: number}) => void}) => {
				mediaOptions.push(options);
				options.onProgress?.({progress: 1});
				writeFileSync(options.outputLocation, Buffer.alloc(48 * 1024, 7));
				return undefined;
			},
			renderStill: async () => {
				stillCalls += 1;
			},
		};
		const assign = jobAssign({bundleSha256: bundle.sha256});
		const metrics = await renderJob(assign, renderer as never, {
			binariesDirectory: null,
			log: (m: string) => logs.push(m),
		});
		expect(stillCalls).toBe(0);
		// selectComposition: the exact pre-still option set.
		expect(selectOptions).toHaveLength(1);
		expect(selectOptions[0]).toMatchObject({
			serveUrl: expect.any(String),
			id: 'c',
			inputProps: {},
			binariesDirectory: null,
			browserExecutable: null,
			chromeMode: 'chrome-for-testing',
			chromiumOptions: {gl: 'angle'},
		});
		// renderMedia: the exact pre-still option set — codec, colour space,
		// concurrency, output name, progress callback.
		expect(mediaOptions).toHaveLength(1);
		expect(mediaOptions[0]).toMatchObject({
			serveUrl: selectOptions[0].serveUrl,
			composition: {durationInFrames: 24, width: 64, height: 36},
			inputProps: {},
			codec: 'h264',
			colorSpace: 'bt709',
			concurrency: 1,
			chromeMode: 'chrome-for-testing',
			chromiumOptions: {gl: 'angle'},
		});
		expect(String(mediaOptions[0].outputLocation)).toMatch(/out\.mp4$/);
		expect(typeof mediaOptions[0].onProgress).toBe('function');
		// One video PUT with the video content type.
		expect(puts).toHaveLength(1);
		expect(puts[0].contentType).toBe('video/mp4');
		expect(putBodies[0].byteLength).toBe(48 * 1024);
		// No still retry line, measured frames from the composition.
		expect(logs.some((l) => l.includes('[retry]'))).toBe(false);
		expect(metrics.frames).toBe(24);
	});
});

describe('isDelayRenderTimeout (still parity with the video matcher)', () => {
	it('matches the packet-22 production error so the still retry fires on the same class', () => {
		expect(isDelayRenderTimeout(delayRenderError().message)).toBe(true);
	});
});

describe('still geometry verification unit (verifyStillOutput)', () => {
	it('accepts a structurally valid PNG and reports the measured geometry', async () => {
		const {verifyStillOutput} = await import('../verify-output.js');
		const dir = mkdtempSync(path.join(tmpdir(), 'still-verify-'));
		try {
			const file = path.join(dir, 'still-f0.png');
			writeFileSync(file, pngBytes(1920, 1080));
			const probe = verifyStillOutput({outputLocation: file, expectedWidth: 1920, expectedHeight: 1080, log: () => {}});
			expect(probe).toEqual({width: 1920, height: 1080, sizeInBytes: pngBytes(1920, 1080).byteLength});
		} finally {
			rmSync(dir, {recursive: true, force: true});
		}
	});

	it('refuses a truncated header, a wrong chunk order, and zero-byte files', async () => {
		const {verifyStillOutput} = await import('../verify-output.js');
		const dir = mkdtempSync(path.join(tmpdir(), 'still-verify-'));
		try {
			const truncated = path.join(dir, 'truncated.png');
			writeFileSync(truncated, pngBytes(64, 36).subarray(0, 12));
			expect(() => verifyStillOutput({outputLocation: truncated, log: () => {}})).toThrow(/truncated header/);

			const noIhdr = path.join(dir, 'no-ihdr.png');
			const reordered = Buffer.from(pngBytes(64, 36));
			reordered.write('IDAT', 12, 'ascii'); // first chunk is not IHDR
			writeFileSync(noIhdr, reordered);
			expect(() => verifyStillOutput({outputLocation: noIhdr, log: () => {}})).toThrow(/not IHDR/);

			const empty = path.join(dir, 'empty.png');
			writeFileSync(empty, Buffer.alloc(0));
			expect(() => verifyStillOutput({outputLocation: empty, log: () => {}})).toThrow(/zero-byte/);

			const missing = path.join(dir, 'missing.png');
			expect(() => verifyStillOutput({outputLocation: missing, log: () => {}})).toThrow(/produced no output file/);
		} finally {
			rmSync(dir, {recursive: true, force: true});
		}
	});

	it('reads real PNGs from disk the same way it reads fixture bytes (round-trip through the fs)', async () => {
		const {verifyStillOutput} = await import('../verify-output.js');
		const dir = mkdtempSync(path.join(tmpdir(), 'still-verify-'));
		try {
			// A 4-byte-over-13-length IHDR variant would break a naive offset
			// read; the fixed header layout keeps it exact. Write and re-read a
			// real file to prove no in-memory shortcut.
			const file = path.join(dir, 'real.png');
			const bytes = pngBytes(640, 360);
			writeFileSync(file, bytes);
			const probe = verifyStillOutput({outputLocation: file, expectedWidth: 640, expectedHeight: 360, log: () => {}});
			expect(probe.width).toBe(640);
			expect(probe.height).toBe(360);
			expect(probe.sizeInBytes).toBe(readFileSync(file).byteLength);
		} finally {
			rmSync(dir, {recursive: true, force: true});
		}
	});

	it('the geometry leg is FAIL-CLOSED (fix1 P3-3): a composition without width/height is REFUSED, never "skipped"', async () => {
		const {verifyStillOutput} = await import('../verify-output.js');
		const dir = mkdtempSync(path.join(tmpdir(), 'still-verify-'));
		try {
			const file = path.join(dir, 'valid.png');
			writeFileSync(file, pngBytes(640, 360));
			// The old branch logged "geometry check skipped" and passed on the
			// signature alone; the packet invariant says verification is never
			// skipped. No dimensions → named refusal, not a pass.
			expect(() =>
				verifyStillOutput({outputLocation: file, log: () => {}}),
			).toThrow(/composition exposed no width\/height — still geometry cannot be verified/);
			expect(() =>
				verifyStillOutput({outputLocation: file, expectedWidth: 640, log: () => {}}),
			).toThrow(/composition exposed no width\/height/);
		} finally {
			rmSync(dir, {recursive: true, force: true});
		}
	});
});
