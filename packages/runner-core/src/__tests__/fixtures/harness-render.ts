#!/usr/bin/env bun
/**
 * Subprocess fixture for the stdout/stderr discipline suite: a full runRunner
 * pass with a fake renderer and a stubbed network, run under Bun exactly the
 * way the compiled payload binary runs. Everything this fixture prints through
 * ordinary channels (console.log, a raw process.stdout.write) MUST end up on
 * stderr — stdout is reserved for protocol frames.
 */
import {readFileSync, writeFileSync} from 'node:fs';
import {runRunner} from '../../index.js';

const bundlePath = process.env.FIXTURE_BUNDLE_PATH as string;
const uploadReceipt = process.env.FIXTURE_UPLOAD_RECEIPT as string;

globalThis.fetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
	const url = String(input);
	if (init?.method === 'PUT') {
		const body = init.body as Uint8Array;
		writeFileSync(uploadReceipt, JSON.stringify({bytes: body.byteLength, contentType: (init.headers as Record<string, string>)['content-type']}));
		return new Response('', {status: 200});
	}
	if (url.includes('render-bundles')) return new Response(readFileSync(bundlePath) as unknown as BodyInit);
	if (url.includes('input-props')) {
		return new Response(JSON.stringify({compositionId: 'Main', inputProps: {greeting: 'hi'}}), {headers: {'content-type': 'application/json'}});
	}
	throw new Error(`unexpected fetch: ${url}`);
}) as typeof fetch;

await runRunner({
	selectComposition: async () => {
		console.log('chatter via console.log');
		return {durationInFrames: 24};
	},
	renderMedia: async (options) => {
		process.stdout.write('chatter via raw process.stdout.write\n');
		for (const progress of [0.01, 0.1, 0.3, 0.6, 1]) options.onProgress({progress});
		writeFileSync(options.outputLocation, Buffer.alloc(1234, 7));
	},
});
