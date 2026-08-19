#!/usr/bin/env bun
/**
 * Subprocess fixture for the liveness suite: a render that reports one early
 * progress event and then goes quiet, the way a heavy composition does between
 * 5% reporting deltas. The runner must keep proving it is alive on its own.
 */
import {readFileSync, writeFileSync} from 'node:fs';
import {runRunner} from '../../index.js';

const bundlePath = process.env.FIXTURE_BUNDLE_PATH as string;
const stallMs = Number(process.env.FIXTURE_STALL_MS ?? '400');

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
		options.onProgress({progress: 0.05});
		await new Promise((resolve) => setTimeout(resolve, stallMs));
		writeFileSync(options.outputLocation, Buffer.alloc(64, 3));
	},
});
