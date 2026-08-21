#!/usr/bin/env bun
/**
 * Subprocess fixture for the signal-cleanup suite: the runner renders with a
 * fake renderer that (a) reports a file inside the per-job workdir so the
 * test can see the dir exists, (b) stalls "forever" so only a signal can end
 * the process — the exact situation a supervisor cancel creates. The parent
 * test SIGTERMs this process and asserts the workdir was purged and the exit
 * was non-zero.
 */
import {readFileSync, writeFileSync} from 'node:fs';
import path from 'node:path';
import {runRunner} from '../../index.js';

const bundlePath = process.env.FIXTURE_BUNDLE_PATH as string;
const workdirProbe = process.env.FIXTURE_WORKDIR_PROBE as string;

globalThis.fetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
	const url = String(input);
	if (init?.method === 'PUT') return new Response('', {status: 200});
	if (url.includes('render-bundles')) return new Response(readFileSync(bundlePath) as unknown as BodyInit);
	if (url.includes('input-props')) {
		return new Response(JSON.stringify({compositionId: 'Main', inputProps: {}}), {
			headers: {'content-type': 'application/json'},
		});
	}
	throw new Error(`unexpected fetch: ${url}`);
}) as typeof fetch;

await runRunner({
	selectComposition: async () => ({durationInFrames: 24}),
	renderMedia: async (options) => {
		// Signal to the parent that the workdir exists and is mid-render.
		// Report the DIR, not outputLocation (which this fake never writes).
		writeFileSync(workdirProbe, path.dirname(options.outputLocation));
		// "Render" until a signal arrives. The handler must purge the dir.
		await new Promise(() => {});
	},
});
