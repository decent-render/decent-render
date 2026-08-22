import {afterEach, beforeEach, describe, expect, it, vi} from 'vitest';
import {readFileSync, rmSync, writeFileSync} from 'node:fs';
import {tmpdir} from 'node:os';
import path from 'node:path';
import {fileURLToPath} from 'node:url';

import {BUNDLE_URL, jobAssign, makeBundleArchive, PROPS_URL} from './helpers.js';

// See bundle-verify.test.ts: the cache dir is resolved from homedir() at
// module-load time, so it must be mocked before render-job is imported.
vi.mock('node:os', async (importOriginal) => {
	const actual = await importOriginal<typeof import('node:os')>();
	const homedir = () => `${actual.tmpdir()}/runner-core-home-renderer-api`;
	return {...actual, homedir, default: {...actual, homedir}};
});

const cacheDir = path.join(tmpdir(), 'runner-core-home-renderer-api', '.decent-worker', 'bundles');
const {renderJob} = await import('../render-job.js');

const here = path.dirname(fileURLToPath(import.meta.url));
const packageRoot = path.resolve(here, '../..');
const bundle = makeBundleArchive();

type SelectOptions = Parameters<Parameters<typeof renderJob>[1]['selectComposition']>[0];
type RenderOptions = Parameters<Parameters<typeof renderJob>[1]['renderMedia']>[0];

function harness() {
	vi.spyOn(globalThis, 'fetch').mockImplementation((async (input: RequestInfo | URL, init?: RequestInit) => {
		const url = String(input);
		if (init?.method === 'PUT') {
			// Packet 15: the PUT body is a stream under the Node runtime; a
			// consumer must consume it or the file read races the workdir purge.
			await new Response(init.body as never).arrayBuffer();
			return new Response('', {status: 200});
		}
		if (url === BUNDLE_URL) return new Response(new Uint8Array(bundle.bytes));
		if (url === PROPS_URL) return new Response(JSON.stringify({compositionId: 'Main', inputProps: {greeting: 'hi'}}), {headers: {'content-type': 'application/json'}});
		throw new Error(`unexpected fetch: ${url}`);
	}) as unknown as typeof fetch);

	const select: SelectOptions[] = [];
	const render: RenderOptions[] = [];
	const api = {
		selectComposition: async (options: SelectOptions) => {
			select.push(options);
			return {durationInFrames: 24};
		},
		renderMedia: async (options: RenderOptions) => {
			render.push(options);
			writeFileSync(options.outputLocation, Buffer.alloc(8, 1));
			return undefined;
		},
	};
	return {select, render, api};
}

beforeEach(() => rmSync(cacheDir, {recursive: true, force: true}));
afterEach(() => vi.restoreAllMocks());

describe('RendererApi injection contract', () => {
	it('renders through the injected functions only — runner-core never imports a renderer', () => {
		const manifest = JSON.parse(readFileSync(path.join(packageRoot, 'package.json'), 'utf8')) as {
			dependencies?: Record<string, string>;
			devDependencies?: Record<string, string>;
			peerDependencies?: Record<string, string>;
		};
		const declared = Object.keys({...manifest.dependencies, ...manifest.devDependencies, ...manifest.peerDependencies});
		expect(declared.filter((name) => name.startsWith('@remotion/') || name === 'remotion')).toEqual([]);

		// Comment lines are excluded on purpose: the docs quote the example
		// `import ... from '@remotion/renderer'` that callers must write.
		for (const file of ['index.ts', 'render-job.ts', 'renderer-api.ts']) {
			const code = readFileSync(path.join(packageRoot, 'src', file), 'utf8')
				.split('\n')
				.filter((line) => !/^\s*(\/\/|\/\*|\*)/.test(line))
				.join('\n');
			expect(code, `${file} must not import a pinned renderer`).not.toMatch(/from\s+'@remotion\//);
			expect(code, `${file} must not require a pinned renderer`).not.toMatch(/require\(\s*'@remotion\//);
		}
	});

	it('passes the GPU render settings the farm depends on to both injected calls', async () => {
		const {select, render, api} = harness();
		const binariesDirectory = '/opt/payload/remotion-binaries';
		await renderJob(jobAssign({bundleSha256: bundle.sha256}), api, {binariesDirectory, log: () => {}});

		const serveUrl = path.join(cacheDir, bundle.sha256);
		expect(select).toHaveLength(1);
		expect(select[0]).toEqual({
			serveUrl,
			id: 'Main',
			inputProps: {greeting: 'hi'},
			binariesDirectory,
			browserExecutable: null,
			chromeMode: 'chrome-for-testing',
			chromiumOptions: {gl: 'angle'},
		});

		expect(render).toHaveLength(1);
		const {onProgress, composition, outputLocation, ...rest} = render[0]!;
		expect(composition).toEqual({durationInFrames: 24});
		expect(typeof onProgress).toBe('function');
		// The output is written inside the per-job working directory that
		// renderJob purges in its `finally` block.
		expect(path.basename(outputLocation)).toBe('out.mp4');
		expect(path.basename(path.dirname(outputLocation))).toMatch(/^job-job-test-1-/);
		expect(rest).toEqual({
			serveUrl,
			inputProps: {greeting: 'hi'},
			binariesDirectory,
			browserExecutable: null,
			chromeMode: 'chrome-for-testing',
			chromiumOptions: {gl: 'angle'},
			codec: 'h264',
			colorSpace: 'bt709',
			concurrency: 1,
		});
	});

	it('defaults binariesDirectory to null when the caller does not supply one', async () => {
		const {select, api} = harness();
		await renderJob(jobAssign({bundleSha256: bundle.sha256}), api, {log: () => {}});
		expect(select[0]!.binariesDirectory).toBeNull();
	});

	it('maps vp8 jobs to a webm output and h264 jobs to an mp4 output', async () => {
		const vp8 = harness();
		await renderJob(jobAssign({bundleSha256: bundle.sha256, codec: 'vp8'}), vp8.api, {log: () => {}});
		expect(vp8.render[0]!.codec).toBe('vp8');
		expect(path.basename(vp8.render[0]!.outputLocation)).toBe('out.webm');
		vi.restoreAllMocks();

		const h264 = harness();
		await renderJob(jobAssign({bundleSha256: bundle.sha256, codec: 'h264'}), h264.api, {log: () => {}});
		expect(h264.render[0]!.codec).toBe('h264');
		expect(path.basename(h264.render[0]!.outputLocation)).toBe('out.mp4');
	});

	it('reports the composition duration returned by the injected selectComposition', async () => {
		const {api} = harness();
		const metrics = await renderJob(jobAssign({bundleSha256: bundle.sha256}), {...api, selectComposition: async () => ({durationInFrames: 900})}, {log: () => {}});
		expect(metrics.frames).toBe(900);
	});

	it('throttles progress to 5% steps and always emits completion', async () => {
		const {api} = harness();
		const seen: number[] = [];
		const throttling = {
			...api,
			renderMedia: async (options: RenderOptions) => {
				for (const progress of [0.01, 0.02, 0.06, 0.07, 0.2, 0.99, 1]) options.onProgress({progress});
				writeFileSync(options.outputLocation, Buffer.alloc(8, 1));
				return undefined;
			},
		};
		await renderJob(jobAssign({bundleSha256: bundle.sha256}), throttling, {log: () => {}, onProgress: (p) => seen.push(p)});
		expect(seen).toEqual([0.06, 0.2, 0.99, 1]);
	});
});
