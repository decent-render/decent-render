/**
 * Transport-level PUT proof (packet 15): the streamed output upload must
 * carry Content-Length equal to the file's stat size and must NOT use
 * Transfer-Encoding: chunked — S3-compatible presigned PUTs reject chunked
 * bodies, so this is a wire contract, not a style preference.
 *
 * These tests do NOT stub fetch. They start a real TCP server that reads
 * the raw request head (and drains the body by the advertised length), so
 * what is asserted is what an S3 endpoint would actually see on the wire.
 *
 * Runtime note: vitest runs under Node here (CI: bun run test → vitest run),
 * so the Node arm of the implementation is what these prove — the Bun arm
 * (Bun.file) was probed separately at the same raw-socket level (packet-15
 * receipt) and is additionally exercised end-to-end by scripts/e2e, where
 * the payload binary is a Bun build and the s3-stub server records the
 * request head it received.
 */
import {afterEach, describe, expect, it, vi} from 'vitest';
import net from 'node:net';
import {createHash} from 'node:crypto';
import {writeFileSync, mkdtempSync, rmSync, statSync} from 'node:fs';
import {tmpdir} from 'node:os';
import path from 'node:path';

import {BUNDLE_URL, jobAssign, makeBundleArchive, PROPS_URL} from './helpers.js';

vi.mock('node:os', async (importOriginal) => {
	const actual = await importOriginal<typeof import('node:os')>();
	const homedir = () => `${actual.tmpdir()}/runner-core-home-stream-put`;
	return {...actual, homedir, default: {...actual, homedir}};
});

const {renderJob} = await import('../render-job.js');

const bundle = makeBundleArchive();

afterEach(() => vi.restoreAllMocks());

/**
 * Stub ONLY the bundle/props fetches (they are not what these tests
 * exercise) while leaving the output PUT on the REAL wire: spy on fetch
 * and pass PUT requests through to the real implementation.
 */
function stubNonPutFetch() {
	const realFetch = globalThis.fetch;
	vi.spyOn(globalThis, 'fetch').mockImplementation((async (input: RequestInfo | URL, init?: RequestInit) => {
		if (init?.method === 'PUT') return realFetch(input, init);
		const url = String(input);
		if (url === BUNDLE_URL) return new Response(new Uint8Array(bundle.bytes));
		if (url === PROPS_URL) return new Response(JSON.stringify({compositionId: 'c', inputProps: {}}), {status: 200});
		return new Response('', {status: 404});
	}) as typeof fetch);
}

interface CapturedPut {
	head: string;
	body: Buffer;
}

/**
 * Real HTTP server: one PUT captured per connection. Reads the head, then
 * drains exactly content-length bytes (chunked requests are captured but
 * flagged by the caller's header assertions).
 */
async function startPutCapture(puts: CapturedPut[], status = 200): Promise<{url: URL; close: () => Promise<void>}> {
	const server = net.createServer((socket) => {
		let chunks: Buffer[] = [];
		let buffer = Buffer.alloc(0);
		let done = false;
		const respond = () => {
			if (done) return;
			done = true;
			socket.end(`HTTP/1.1 ${status} OK\r\ncontent-length: 0\r\n\r\n`);
		};
		socket.on('data', (chunk: Buffer) => {
			buffer = Buffer.concat([buffer, chunk]);
			if (done) return;
			const idx = buffer.indexOf('\r\n\r\n');
			if (idx === -1) return;
			const head = buffer.subarray(0, idx).toString('latin1');
			let bodyStart = idx + 4;
			const cl = /content-length:\s*(\d+)/i.exec(head);
			if (cl) {
				const need = Number(cl[1]);
				if (buffer.length - bodyStart >= need) {
					puts.push({head, body: buffer.subarray(bodyStart, bodyStart + need)});
					respond();
				}
			} else {
				// chunked (or no length): capture on last-chunk
				const tail = buffer.subarray(bodyStart);
				if (tail.includes(Buffer.from('0\r\n\r\n'))) {
					puts.push({head, body: tail});
					respond();
				}
			}
			chunks = [];
		});
		socket.on('error', () => {});
	});
	await new Promise<void>((r) => server.listen(0, '127.0.0.1', r));
	const address = server.address() as net.AddressInfo;
	return {
		url: new URL(`http://127.0.0.1:${address.port}/output`),
		close: () => new Promise<void>((r) => server.close(() => r())),
	};
}

/** Renderer that writes a deterministic file of `bytes` bytes. */
function rendererWriting(bytes: number) {
	return {
		selectComposition: async () => ({durationInFrames: 24, width: 64, height: 36}),
		renderMedia: async (options: {outputLocation: string}) => {
			const payload = Buffer.alloc(bytes, 0xab);
			writeFileSync(options.outputLocation, payload);
		},
	};
}

describe('output PUT wire shape (streamed body)', () => {
	it('carries Content-Length equal to the stat size, byte-identical body, no chunked', async () => {
		// 4 MiB: far above any plausible single read buffer, so a passing
		// content-length equality means the size came from stat/stream
		// plumbing, not an in-memory buffer that happened to be right.
		const bytes = 4 * 1024 * 1024;
		stubNonPutFetch();
		const puts: CapturedPut[] = [];
		const server = await startPutCapture(puts);
		try {
			const metrics = await renderJob(
				jobAssign({bundleSha256: bundle.sha256, outputPutUrl: server.url.toString()}),
				rendererWriting(bytes) as never,
				{binariesDirectory: null, log: () => {}},
			);
			expect(puts).toHaveLength(1);
			const put = puts[0]!;
			// THE wire contract: Content-Length present and exact...
			const cl = /content-length:\s*(\d+)/i.exec(put.head);
			expect(cl).not.toBeNull();
			expect(Number(cl![1])).toBe(bytes);
			// ...and NOT chunked.
			expect(/transfer-encoding:\s*chunked/i.test(put.head)).toBe(false);
			// Byte-identical content.
			expect(put.body.length).toBe(bytes);
			const expected = Buffer.alloc(bytes, 0xab);
			expect(createHash('sha256').update(put.body).digest('hex'))
				.toBe(createHash('sha256').update(expected).digest('hex'));
			// The reported metric is the stat size (same number the wire saw).
			expect(metrics.outputSizeInBytes).toBe(bytes);
		} finally {
			await server.close();
		}
	});

	it('streams a small file with the same guarantees (no size-dependent branch)', async () => {
		const bytes = 4096;
		stubNonPutFetch();
		const puts: CapturedPut[] = [];
		const server = await startPutCapture(puts);
		try {
			await renderJob(
				jobAssign({bundleSha256: bundle.sha256, outputPutUrl: server.url.toString()}),
				rendererWriting(bytes) as never,
				{binariesDirectory: null, log: () => {}},
			);
			expect(puts).toHaveLength(1);
			const cl = /content-length:\s*(\d+)/i.exec(puts[0]!.head);
			expect(Number(cl![1])).toBe(bytes);
			expect(/transfer-encoding:\s*chunked/i.test(puts[0]!.head)).toBe(false);
			expect(puts[0]!.body.length).toBe(bytes);
		} finally {
			await server.close();
		}
	});

	it('failure parity: non-2xx surfaces the same error text as before streaming', async () => {
		stubNonPutFetch();
		const puts: CapturedPut[] = [];
		const server = await startPutCapture(puts, 500);
		try {
			await expect(
				renderJob(
					jobAssign({bundleSha256: bundle.sha256, outputPutUrl: server.url.toString()}),
					rendererWriting(4096) as never,
					{binariesDirectory: null, log: () => {}},
				),
			).rejects.toThrow('output upload failed: HTTP 500');
		} finally {
			await server.close();
		}
	});

	it('failure parity: connection refused surfaces a fetch error (and the workdir is purged)', async () => {
		// Reserve a port then close it: connection refused.
		const probe = net.createServer();
		await new Promise<void>((r) => probe.listen(0, '127.0.0.1', r));
		const port = (probe.address() as net.AddressInfo).port;
		await new Promise<void>((r) => probe.close(() => r()));
		stubNonPutFetch();
		const jobId = `job-stream-put-refused-${process.pid}`;
		await expect(
			renderJob(
				jobAssign({bundleSha256: bundle.sha256, jobId, outputPutUrl: `http://127.0.0.1:${port}/output`}),
				rendererWriting(4096) as never,
				{binariesDirectory: null, log: () => {}},
			),
		).rejects.toThrow();
		const leaked = (await import('node:fs')).readdirSync(tmpdir()).filter((e) => e.startsWith(`job-${jobId}-`));
		expect(leaked).toEqual([]);
	});
});
