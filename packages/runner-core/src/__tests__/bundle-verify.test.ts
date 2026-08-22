import {afterEach, beforeEach, describe, expect, it, vi} from 'vitest';
import {createHash} from 'node:crypto';
import {existsSync, readdirSync, readFileSync, rmSync, writeFileSync} from 'node:fs';
import {tmpdir} from 'node:os';
import path from 'node:path';

import {BUNDLE_URL, jobAssign, makeBundleArchive, OUTPUT_PUT_URL, PROPS_URL} from './helpers.js';

// The bundle cache lives at `~/.decent-worker/bundles` and is resolved at
// module-load time, so homedir must be redirected BEFORE render-job is
// imported. (Setting process.env.HOME does not work: vitest runs each file in
// a worker thread whose process.env is a copy libuv's getenv never sees.)
vi.mock('node:os', async (importOriginal) => {
	const actual = await importOriginal<typeof import('node:os')>();
	const homedir = () => `${actual.tmpdir()}/runner-core-home-bundle-verify`;
	return {...actual, homedir, default: {...actual, homedir}};
});

const cacheDir = path.join(tmpdir(), 'runner-core-home-bundle-verify', '.decent-worker', 'bundles');
const {renderJob} = await import('../render-job.js');

const bundle = makeBundleArchive();

type FetchCall = {url: string; method: string};

function stubNetwork(options: {bundleBytes?: Buffer; bundleStatus?: number; uploadStatus?: number} = {}) {
	const calls: FetchCall[] = [];
	// Packet 15: the PUT body is now a STREAM (createReadStream under the
	// Node test runtime), so the capture drains it to bytes for the content
	// assertions — the wire bytes are identical either way.
	const uploaded: {body: Uint8Array; contentType: string | undefined; contentLength?: string}[] = [];
	vi.spyOn(globalThis, 'fetch').mockImplementation((async (input: RequestInfo | URL, init?: RequestInit) => {
		const url = String(input);
		calls.push({url, method: init?.method ?? 'GET'});
		if (init?.method === 'PUT') {
			const headers = (init.headers ?? {}) as Record<string, string>;
			const drained = await new Response(init.body as BodyInit).arrayBuffer();
			uploaded.push({body: new Uint8Array(drained), contentType: headers['content-type'], contentLength: headers['content-length']});
			return new Response('', {status: options.uploadStatus ?? 200});
		}
		if (url === BUNDLE_URL) {
			if (options.bundleStatus && options.bundleStatus !== 200) return new Response('nope', {status: options.bundleStatus});
			return new Response(new Uint8Array(options.bundleBytes ?? bundle.bytes));
		}
		if (url === PROPS_URL) {
			return new Response(JSON.stringify({compositionId: 'Main', inputProps: {greeting: 'hi'}}), {headers: {'content-type': 'application/json'}});
		}
		throw new Error(`unexpected fetch: ${url}`);
	}) as unknown as typeof fetch);
	return {calls, uploaded};
}

function fakeRenderer(onRender?: (outputLocation: string) => void) {
	const rendered: string[] = [];
	return {
		rendered,
		api: {
			selectComposition: vi.fn(async () => ({durationInFrames: 24})),
			renderMedia: vi.fn(async (options: {outputLocation: string; onProgress: (p: {progress: number}) => void}) => {
				rendered.push(options.outputLocation);
				options.onProgress({progress: 1});
				(onRender ?? ((location: string) => writeFileSync(location, Buffer.alloc(64, 3))))(options.outputLocation);
			}),
		},
	};
}

/** Job working directories are `job-<jobId>-*` under the OS temp dir. */
const strayWorkDirs = (jobId: string) => readdirSync(tmpdir()).filter((entry) => entry.startsWith(`job-${jobId}-`));

beforeEach(() => rmSync(cacheDir, {recursive: true, force: true}));
afterEach(() => vi.restoreAllMocks());

describe('bundle integrity', () => {
	it('verifies the sha256, extracts, and caches the bundle under the worker cache dir', async () => {
		const net = stubNetwork();
		const renderer = fakeRenderer();
		const metrics = await renderJob(jobAssign({bundleSha256: bundle.sha256}), renderer.api, {log: () => {}});

		expect(metrics).toEqual({wallMs: expect.any(Number), frames: 24, outputSizeInBytes: 64});
		expect(existsSync(path.join(cacheDir, bundle.sha256, 'index.html'))).toBe(true);
		// serveUrl is the extracted cache directory, not the presigned URL.
		expect(renderer.api.selectComposition.mock.calls[0]![0].serveUrl).toBe(path.join(cacheDir, bundle.sha256));
		expect(net.uploaded).toHaveLength(1);
	});

	it('rejects a bundle whose bytes do not hash to the advertised sha256, and caches nothing', async () => {
		const net = stubNetwork({bundleBytes: Buffer.concat([bundle.bytes, Buffer.from('tamper')])});
		const renderer = fakeRenderer();
		const assign = jobAssign({bundleSha256: bundle.sha256});

		await expect(renderJob(assign, renderer.api, {log: () => {}})).rejects.toThrow(/^bundle sha mismatch: expected /);
		expect(existsSync(path.join(cacheDir, bundle.sha256))).toBe(false);
		// Nothing downstream of verification may run on unverified bytes.
		expect(renderer.api.selectComposition).not.toHaveBeenCalled();
		expect(renderer.api.renderMedia).not.toHaveBeenCalled();
		expect(net.uploaded).toHaveLength(0);
		expect(net.calls.map((c) => c.url)).toEqual([BUNDLE_URL]);
	});

	it('names both the advertised and the actual hash in the mismatch error', async () => {
		const tampered = Buffer.from('not a bundle');
		stubNetwork({bundleBytes: tampered});
		const advertised = 'c'.repeat(64);
		const actual = createHash('sha256').update(tampered).digest('hex');
		await expect(renderJob(jobAssign({bundleSha256: advertised}), fakeRenderer().api, {log: () => {}})).rejects.toThrow(
			`bundle sha mismatch: expected ${advertised}, got ${actual}`,
		);
	});

	it('reuses a verified cached bundle instead of re-downloading it', async () => {
		const first = stubNetwork();
		await renderJob(jobAssign({bundleSha256: bundle.sha256}), fakeRenderer().api, {log: () => {}});
		expect(first.calls.filter((c) => c.url === BUNDLE_URL)).toHaveLength(1);
		vi.restoreAllMocks();

		const second = stubNetwork();
		await renderJob(jobAssign({bundleSha256: bundle.sha256}), fakeRenderer().api, {log: () => {}});
		expect(second.calls.filter((c) => c.url === BUNDLE_URL)).toHaveLength(0);
	});

	it('surfaces a failed bundle download as an HTTP error', async () => {
		stubNetwork({bundleStatus: 404});
		await expect(renderJob(jobAssign({bundleSha256: bundle.sha256}), fakeRenderer().api, {log: () => {}})).rejects.toThrow('bundle download failed: HTTP 404');
	});

	it('surfaces a failed output upload', async () => {
		stubNetwork({uploadStatus: 500});
		await expect(renderJob(jobAssign({bundleSha256: bundle.sha256}), fakeRenderer().api, {log: () => {}})).rejects.toThrow('output upload failed: HTTP 500');
	});
});

describe('workdir purge', () => {
	it('deletes the job working directory after a successful render', async () => {
		stubNetwork();
		const assign = jobAssign({bundleSha256: bundle.sha256, jobId: 'purge-ok'});
		await renderJob(assign, fakeRenderer().api, {log: () => {}});
		expect(strayWorkDirs('purge-ok')).toEqual([]);
	});

	it('deletes the job working directory after a failed render', async () => {
		stubNetwork();
		const renderer = fakeRenderer();
		renderer.api.renderMedia.mockRejectedValueOnce(new Error('render exploded'));
		const assign = jobAssign({bundleSha256: bundle.sha256, jobId: 'purge-fail'});
		await expect(renderJob(assign, renderer.api, {log: () => {}})).rejects.toThrow('render exploded');
		expect(strayWorkDirs('purge-fail')).toEqual([]);
	});
});

describe('output handling', () => {
	it('uploads the rendered file bytes with the codec content type', async () => {
		const net = stubNetwork();
		const payload = Buffer.alloc(4096, 9);
		const renderer = fakeRenderer((location) => writeFileSync(location, payload));
		const metrics = await renderJob(jobAssign({bundleSha256: bundle.sha256, codec: 'vp8'}), renderer.api, {log: () => {}});

		expect(metrics.outputSizeInBytes).toBe(4096);
		expect(net.uploaded[0]!.contentType).toBe('video/webm');
		expect(Buffer.from(net.uploaded[0]!.body).equals(payload)).toBe(true);
		expect(net.calls.at(-1)).toEqual({url: OUTPUT_PUT_URL, method: 'PUT'});
		expect(readFileSync(path.join(cacheDir, bundle.sha256, 'index.html'), 'utf8')).toContain('decent-render test bundle');
	});
});
