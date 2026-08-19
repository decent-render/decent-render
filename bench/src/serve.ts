/**
 * Minimal stand-in for R2 + dispatch's presigned URLs: serves the bundle
 * tarball and input props, and accepts the output PUT. Local only — the point
 * is to isolate the render path, not to test networking.
 */
import {readFileSync, writeFileSync} from 'node:fs';

export type ServeHandle = {
	port: number;
	stop: () => void;
	/** Wall-clock ms of each bundle GET, to separate download from render cost. */
	bundleFetches: {startedAt: number; finishedAt: number; bytes: number}[];
	outputBytes: () => number | null;
};

export function serve(options: {
	archivePath: string;
	compositionId: string;
	inputProps: Record<string, unknown>;
	outputPath: string;
}): ServeHandle {
	const archive = readFileSync(options.archivePath);
	const props = Buffer.from(
		JSON.stringify({compositionId: options.compositionId, inputProps: options.inputProps}),
	);
	const bundleFetches: ServeHandle['bundleFetches'] = [];
	let outputBytes: number | null = null;

	const server = Bun.serve({
		port: 0,
		async fetch(req) {
			const url = new URL(req.url);
			if (url.pathname === '/bundle.tar.gz') {
				const startedAt = performance.now();
				const record = {startedAt, finishedAt: startedAt, bytes: archive.byteLength};
				bundleFetches.push(record);
				const body = new Response(archive);
				record.finishedAt = performance.now();
				return body;
			}
			if (url.pathname === '/props.json') return new Response(props);
			if (url.pathname === '/output' && req.method === 'PUT') {
				const bytes = Buffer.from(await req.arrayBuffer());
				writeFileSync(options.outputPath, bytes);
				outputBytes = bytes.byteLength;
				return new Response('ok');
			}
			return new Response('not found', {status: 404});
		},
	});

	return {
		port: server.port,
		stop: () => server.stop(true),
		bundleFetches,
		outputBytes: () => outputBytes,
	};
}
