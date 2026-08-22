/**
 * PACKET 25 (0.1.3): retry-once on the renderer-init delayRender hang.
 *
 * The packet-22 failure class: under GPU-adapter contention two Chrome
 * processes race navigator.gpu.requestAdapter(); the loser's promise
 * never settles, the delayRender window expires, and the job fails with
 * `A delayRender() "…" was called but not cleared after Nms`. The hung
 * promise dies with its Chrome, so one fresh attempt usually wins.
 *
 * These tests pin the contract:
 * - exactly ONE retry, only for delayRender-timeout failures;
 * - the retry is VISIBLE (the leased log line names it — Ray's
 *   traceability theme);
 * - a second delayRender failure surfaces the error (no infinite loop);
 * - non-delayRender failures never retry;
 * - cancel during the first attempt suppresses the retry;
 * - the retry restarts the progress curve (the supervisor sees attempt 2
 *   progress honestly);
 * - selectComposition hangs retry too (the loop covers select+render).
 */
import {afterEach, describe, expect, it, vi} from 'vitest';
import {writeFileSync} from 'node:fs';

import {BUNDLE_URL, jobAssign, makeBundleArchive, OUTPUT_PUT_URL, PROPS_URL} from './helpers.js';

vi.mock('node:os', async (importOriginal) => {
	const actual = await importOriginal<typeof import('node:os')>();
	const homedir = () => `${actual.tmpdir()}/runner-core-home-render-retry`;
	return {...actual, homedir, default: {...actual, homedir}};
});

const {renderJob, isDelayRenderTimeout, markJobCanceled, resetJobCanceledForTests} = await import('../render-job.js');

const bundle = makeBundleArchive();

afterEach(() => {
	vi.restoreAllMocks();
	resetJobCanceledForTests();
});

function stubNetwork() {
	const puts: string[] = [];
	vi.spyOn(globalThis, 'fetch').mockImplementation((async (input: RequestInfo | URL, init?: RequestInit) => {
		const url = String(input);
		if (init?.method === 'PUT') {
			puts.push(url);
			await new Response(init.body as never).arrayBuffer();
			return new Response('', {status: 200});
		}
		if (url === BUNDLE_URL) return new Response(new Uint8Array(bundle.bytes));
		if (url === PROPS_URL) return new Response(JSON.stringify({compositionId: 'c', inputProps: {}}), {status: 200});
		return new Response('', {status: 404});
	}) as typeof fetch);
	return puts;
}

const delayRenderError = () =>
	new Error('A delayRender() "Waiting for <ThreeCanvas/> to be created" was called but not cleared after 28000ms. See https://remotion.dev/docs/timeout for help. ');

/** Renderer whose renderMedia fails N times with the delayRender timeout, then writes output. */
function hangingRenderer(failures: number) {
	let calls = 0;
	return {
		selectCompositionCalls: 0,
		selectComposition: async () => {
			calls = calls; // count via closure below
			return {durationInFrames: 24, width: 64, height: 36};
		},
		renderMedia: async (options: {outputLocation: string; onProgress?: (p: {progress: number}) => void}) => {
			calls += 1;
			if (calls <= failures) {
				// report some progress first — the hang happens mid-init
				options.onProgress?.({progress: 0.1});
				throw delayRenderError();
			}
			options.onProgress?.({progress: 1});
			writeFileSync(options.outputLocation, Buffer.alloc(48 * 1024, 7));
		},
		renderCalls: () => calls,
	};
}

describe('isDelayRenderTimeout matcher', () => {
	it('matches the packet-22 production error verbatim', () => {
		expect(isDelayRenderTimeout(delayRenderError().message)).toBe(true);
	});
	it('matches any label — tenant copy must not gate the retry', () => {
		expect(isDelayRenderTimeout('A delayRender() "my thing" was called but not cleared after 10000ms')).toBe(true);
	});
	it('rejects other failures', () => {
		expect(isDelayRenderTimeout('output upload failed: HTTP 500')).toBe(false);
		expect(isDelayRenderTimeout('TypeError: cannot read properties of undefined')).toBe(false);
		expect(isDelayRenderTimeout('ENOSPC: no space left on device')).toBe(false);
	});
});

describe('renderJob retry-once on delayRender timeout', () => {
	it('retries EXACTLY ONCE and succeeds; the retry log line names the trigger', async () => {
		const puts = stubNetwork();
		const logs: string[] = [];
		const renderer = hangingRenderer(1);
		const metrics = await renderJob(jobAssign({bundleSha256: bundle.sha256}), renderer as never, {
			binariesDirectory: null,
			log: (m: string) => logs.push(m),
		});
		expect(renderer.renderCalls()).toBe(2);
		expect(metrics.frames).toBe(24);
		expect(puts).toEqual([OUTPUT_PUT_URL]);
		const retryLog = logs.find((l) => l.includes('[retry]'));
		expect(retryLog).toBeDefined();
		expect(retryLog).toContain('delayRender timeout');
		expect(retryLog).toContain('attempt 2 of 2');
	});

	it('a SECOND delayRender failure surfaces the error frame — no infinite loop', async () => {
		stubNetwork();
		const renderer = hangingRenderer(2);
		await expect(
			renderJob(jobAssign({bundleSha256: bundle.sha256}), renderer as never, {
				binariesDirectory: null,
				log: () => {},
			}),
		).rejects.toThrow(/was called but not cleared after/);
		expect(renderer.renderCalls()).toBe(2);
	});

	it('non-delayRender failures never retry', async () => {
		stubNetwork();
		let calls = 0;
		const renderer = {
			selectComposition: async () => ({durationInFrames: 24, width: 64, height: 36}),
			renderMedia: async () => {
				calls += 1;
				throw new Error('some other render failure');
			},
		};
		await expect(
			renderJob(jobAssign({bundleSha256: bundle.sha256}), renderer as never, {
				binariesDirectory: null,
				log: () => {},
			}),
		).rejects.toThrow('some other render failure');
		expect(calls).toBe(1);
	});

	it('cancel during the first attempt → NO retry (cancel must abort)', async () => {
		stubNetwork();
		let calls = 0;
		const renderer = {
			selectComposition: async () => ({durationInFrames: 24, width: 64, height: 36}),
			renderMedia: async (options: {outputLocation: string}) => {
				calls += 1;
				writeFileSync(options.outputLocation, Buffer.alloc(48 * 1024, 7));
				markJobCanceled();
				throw delayRenderError();
			},
		};
		await expect(
			renderJob(jobAssign({bundleSha256: bundle.sha256}), renderer as never, {
				binariesDirectory: null,
				log: () => {},
			}),
		).rejects.toThrow(/was called but not cleared after/);
		// eslint-disable-next-line @typescript-eslint/no-unused-expressions
		expect(calls).toBe(1); // a canceled job must not consume a retry
	});

	it('a delayRender failure in selectComposition retries too (the loop covers select+render)', async () => {
		stubNetwork();
		let selectCalls = 0;
		const renderer = {
			selectComposition: async () => {
				selectCalls += 1;
				if (selectCalls === 1) throw delayRenderError();
				return {durationInFrames: 24, width: 64, height: 36};
			},
			renderMedia: async (options: {outputLocation: string}) => {
				writeFileSync(options.outputLocation, Buffer.alloc(48 * 1024, 7));
			},
		};
		const metrics = await renderJob(jobAssign({bundleSha256: bundle.sha256}), renderer as never, {
			binariesDirectory: null,
			log: () => {},
		});
		expect(selectCalls).toBe(2);
		expect(metrics.frames).toBe(24);
	});

	it('the retry restarts the progress curve from 0 (attempt 2 is honest)', async () => {
		stubNetwork();
		const progressReports: number[] = [];
		let calls = 0;
		const renderer = {
			selectComposition: async () => ({durationInFrames: 24, width: 64, height: 36}),
			renderMedia: async (options: {outputLocation: string; onProgress?: (p: {progress: number}) => void}) => {
				calls += 1;
				if (calls === 1) {
					options.onProgress?.({progress: 0.6}); // attempt 1 climbs...
					throw delayRenderError();
				}
				options.onProgress?.({progress: 0.2}); // attempt 2 restarts low
				writeFileSync(options.outputLocation, Buffer.alloc(48 * 1024, 7));
			},
		};
		await renderJob(jobAssign({bundleSha256: bundle.sha256}), renderer as never, {
			binariesDirectory: null,
			log: () => {},
			onProgress: (p) => progressReports.push(p),
		});
		// 0.6 (attempt 1) then 0.2 (attempt 2 restart) — both surface.
		expect(progressReports).toContain(0.6);
		expect(progressReports).toContain(0.2);
	});
});
