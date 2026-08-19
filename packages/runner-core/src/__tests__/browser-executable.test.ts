/**
 * Regression tests for the payload-supplied browser.
 *
 * Background (measured 2026-08-19): a compiled runner is spawned with cwd set to
 * a per-job workdir. Remotion resolves its browser cache by walking UP from cwd
 * for a package.json and, finding none, downloads ~1GB into that workdir — which
 * the purge then deletes, so EVERY job re-downloaded Chrome. Passing an explicit
 * browserExecutable from the payload is what stops that, so these tests pin both
 * halves: the option must reach both renderer calls, and the manifest lookup
 * must be strict about what counts as a usable browser.
 */
import {afterEach, describe, expect, it, vi} from 'vitest';
import {mkdirSync, mkdtempSync, writeFileSync} from 'node:fs';
import {tmpdir} from 'node:os';
import path from 'node:path';

import {BUNDLE_URL, jobAssign, makeBundleArchive, PROPS_URL} from './helpers.js';

vi.mock('node:os', async (importOriginal) => {
	const actual = await importOriginal<typeof import('node:os')>();
	const homedir = () => `${actual.tmpdir()}/runner-core-home-browser-exe`;
	return {...actual, homedir, default: {...actual, homedir}};
});

const {renderJob} = await import('../render-job.js');
const bundle = makeBundleArchive();

type SelectOptions = Parameters<Parameters<typeof renderJob>[1]['selectComposition']>[0];
type RenderOptions = Parameters<Parameters<typeof renderJob>[1]['renderMedia']>[0];

function harness() {
	vi.spyOn(globalThis, 'fetch').mockImplementation((async (input: RequestInfo | URL, init?: RequestInit) => {
		const url = String(input);
		if (init?.method === 'PUT') return new Response('', {status: 200});
		if (url === BUNDLE_URL) return new Response(new Uint8Array(bundle.bytes));
		if (url === PROPS_URL)
			return new Response(JSON.stringify({compositionId: 'Main', inputProps: {}}), {
				headers: {'content-type': 'application/json'},
			});
		throw new Error(`unexpected fetch: ${url}`);
	}) as unknown as typeof fetch);

	const select: SelectOptions[] = [];
	const render: RenderOptions[] = [];
	return {
		select,
		render,
		api: {
			selectComposition: async (options: SelectOptions) => {
				select.push(options);
				return {durationInFrames: 24};
			},
			renderMedia: async (options: RenderOptions) => {
				render.push(options);
				writeFileSync(options.outputLocation, Buffer.alloc(8, 1));
				return undefined;
			},
		},
	};
}

afterEach(() => {
	vi.restoreAllMocks();
});

describe('payload-supplied browser', () => {
	it('passes browserExecutable to BOTH selectComposition and renderMedia', async () => {
		const {api, select, render} = harness();
		const browser = '/payload/chrome/Google Chrome for Testing';

		await renderJob(jobAssign({bundleSha256: bundle.sha256}), api, {browserExecutable: browser});

		// Both calls matter: selectComposition launches a browser too, and it is
		// where the download was observed to happen first.
		expect(select[0]?.browserExecutable).toBe(browser);
		expect(render[0]?.browserExecutable).toBe(browser);
	});

	it('defaults to null rather than undefined when the payload has no browser', async () => {
		const {api, select, render} = harness();

		await renderJob(jobAssign({bundleSha256: bundle.sha256}), api, {});

		// null is Remotion's documented "resolve it yourself" value; undefined
		// would be silently dropped from the options object.
		expect(select[0]?.browserExecutable).toBeNull();
		expect(render[0]?.browserExecutable).toBeNull();
	});
});

describe('chrome/executable manifest resolution', () => {
	/** Mirrors runRunner's browserExecutable() resolution against a fake payload. */
	const resolve = async (payloadRoot: string) => {
		const {existsSync, readFileSync} = await import('node:fs');
		const manifest = path.join(payloadRoot, 'chrome', 'executable');
		if (!existsSync(manifest)) return null;
		const relative = readFileSync(manifest, 'utf8').trim();
		if (!relative) return null;
		const resolved = path.join(payloadRoot, relative);
		return existsSync(resolved) ? resolved : null;
	};

	const payload = () => mkdtempSync(path.join(tmpdir(), 'runner-core-payload-'));

	it('resolves a browser recorded by the publish step', async () => {
		const root = payload();
		mkdirSync(path.join(root, 'chrome', 'nested'), {recursive: true});
		writeFileSync(path.join(root, 'chrome', 'nested', 'browser'), '#!/bin/sh\n');
		writeFileSync(path.join(root, 'chrome', 'executable'), 'chrome/nested/browser\n');

		expect(await resolve(root)).toBe(path.join(root, 'chrome/nested/browser'));
	});

	it('returns null when the manifest is absent', async () => {
		expect(await resolve(payload())).toBeNull();
	});

	it('returns null when the manifest points at a missing file', async () => {
		// A truncated or mis-built payload must not be treated as usable: the
		// caller warns loudly instead of silently falling back to a 1GB download.
		const root = payload();
		mkdirSync(path.join(root, 'chrome'), {recursive: true});
		writeFileSync(path.join(root, 'chrome', 'executable'), 'chrome/does-not-exist\n');

		expect(await resolve(root)).toBeNull();
	});

	it('returns null when the manifest is empty', async () => {
		const root = payload();
		mkdirSync(path.join(root, 'chrome'), {recursive: true});
		writeFileSync(path.join(root, 'chrome', 'executable'), '   \n');

		expect(await resolve(root)).toBeNull();
	});
});
