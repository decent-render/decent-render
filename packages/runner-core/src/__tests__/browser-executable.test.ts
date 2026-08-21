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
const {resolveBrowserExecutable} = await import('../index.js');
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

describe('browser resolution', () => {
	const payload = () => mkdtempSync(path.join(tmpdir(), 'runner-core-payload-'));

	/** A payload with a browser bundled inside it, as published before the split. */
	const payloadWithBundledBrowser = () => {
		const root = payload();
		mkdirSync(path.join(root, 'chrome', 'nested'), {recursive: true});
		writeFileSync(path.join(root, 'chrome', 'nested', 'browser'), '#!/bin/sh\n');
		writeFileSync(path.join(root, 'chrome', 'executable'), 'chrome/nested/browser\n');
		return root;
	};

	/** A standalone browser artifact, as fetched and resolved by the supervisor. */
	const standaloneBrowser = () => {
		const dir = mkdtempSync(path.join(tmpdir(), 'runner-core-browser-'));
		const exe = path.join(dir, 'chrome');
		writeFileSync(exe, '#!/bin/sh\n');
		return exe;
	};

	it('prefers the supervisor-injected browser over one bundled in the payload', () => {
		const injected = standaloneBrowser();
		const root = payloadWithBundledBrowser();

		expect(resolveBrowserExecutable(root, {DECENT_BROWSER_EXECUTABLE: injected})).toBe(injected);
	});

	it('uses the injected browser when the payload bundles none', () => {
		const injected = standaloneBrowser();

		expect(resolveBrowserExecutable(payload(), {DECENT_BROWSER_EXECUTABLE: injected})).toBe(injected);
	});

	it('falls back to the payload when the injected path does not exist', () => {
		// A stale or hand-set variable must not take a working payload offline;
		// the supervisor already verified whatever it actually fetched.
		const root = payloadWithBundledBrowser();

		expect(resolveBrowserExecutable(root, {DECENT_BROWSER_EXECUTABLE: '/nope/chrome'})).toBe(
			path.join(root, 'chrome/nested/browser'),
		);
	});

	it('resolves a browser recorded by the publish step', () => {
		const root = payloadWithBundledBrowser();

		expect(resolveBrowserExecutable(root, {})).toBe(path.join(root, 'chrome/nested/browser'));
	});

	it('returns null when neither source has a browser', () => {
		expect(resolveBrowserExecutable(payload(), {})).toBeNull();
	});

	it('returns null when the manifest points at a missing file', () => {
		// A truncated or mis-built payload must not be treated as usable: the
		// caller warns loudly instead of silently falling back to a 1GB download.
		const root = payload();
		mkdirSync(path.join(root, 'chrome'), {recursive: true});
		writeFileSync(path.join(root, 'chrome', 'executable'), 'chrome/does-not-exist\n');

		expect(resolveBrowserExecutable(root, {})).toBeNull();
	});

	it('returns null when the manifest is empty', () => {
		const root = payload();
		mkdirSync(path.join(root, 'chrome'), {recursive: true});
		writeFileSync(path.join(root, 'chrome', 'executable'), '   \n');

		expect(resolveBrowserExecutable(root, {})).toBeNull();
	});
});

describe('supervisor exec wrapper (browser containment)', () => {
	/**
	 * Defect A containment: the supervisor hands Remotion a wrapper script
	 * (.decent-browser-wrapper) that records the spawned pid BEFORE exec'ing
	 * the real browser. Remotion spawns it detached (own group), so the
	 * recorded pid is the browser tree's group leader. This test proves the
	 * resolution layer passes the wrapper through unchanged — the property
	 * the whole containment depends on.
	 */
	it('passes the supervisor exec wrapper through as the browser executable', () => {
		const workdir = mkdtempSync(path.join(tmpdir(), 'runner-core-wrapper-'));
		const wrapper = path.join(workdir, '.decent-browser-wrapper');
		writeFileSync(wrapper, '#!/bin/sh\necho $$ >> pids\nexec /usr/bin/true "$@"\n');
		const barePayload = mkdtempSync(path.join(tmpdir(), 'runner-core-payload-'));

		expect(resolveBrowserExecutable(barePayload, {DECENT_BROWSER_EXECUTABLE: wrapper})).toBe(
			wrapper,
		);
	});
});
