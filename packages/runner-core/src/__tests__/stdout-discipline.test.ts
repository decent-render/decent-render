import {describe, expect, it} from 'vitest';
import {spawnSync} from 'node:child_process';
import {mkdtempSync, readFileSync, writeFileSync} from 'node:fs';
import {tmpdir} from 'node:os';
import path from 'node:path';
import {fileURLToPath} from 'node:url';

import {jobAssign, makeBundleArchive} from './helpers.js';

/**
 * The runner's contract with the supervisor is a stdout NDJSON stream of
 * protocol frames and NOTHING else — any renderer chatter (Remotion logs
 * liberally to stdout) must be diverted to stderr, or it corrupts the stream
 * the supervisor parses. These run the real `runRunner` under `bun` in a
 * subprocess, the same way the compiled payload binary runs it.
 */
const here = path.dirname(fileURLToPath(import.meta.url));
const fixtures = path.join(here, 'fixtures');

function runHarness(fixture: string, input: string, env: Record<string, string> = {}) {
	const home = mkdtempSync(path.join(tmpdir(), 'runner-core-subproc-home-'));
	const result = spawnSync('bun', [path.join(fixtures, fixture)], {
		input,
		encoding: 'utf8',
		env: {...process.env, HOME: home, ...env},
	});
	if (result.error) throw result.error;
	return {...result, home};
}

const stdoutLines = (stdout: string) => stdout.trim().split('\n').filter(Boolean);
const parseFrames = (stdout: string) => stdoutLines(stdout).map((line) => JSON.parse(line) as Record<string, unknown>);

describe('stdout/stderr protocol discipline', () => {
	it('emits exactly one error frame on stdout and exits 1 when stdin is not JSON', () => {
		const result = runHarness('harness-bad-frame.ts', 'not-json');
		expect(result.status).toBe(1);
		const frames = parseFrames(result.stdout);
		expect(frames).toHaveLength(1);
		expect(frames[0]!.type).toBe('error');
		expect(String(frames[0]!.message)).toMatch(/^JSON Parse error: /);
	});

	it('rejects a well-formed frame that is not a jobAssign', () => {
		const result = runHarness('harness-bad-frame.ts', JSON.stringify({type: 'ping', tenant: 'driffs'}));
		expect(result.status).toBe(1);
		const frames = parseFrames(result.stdout);
		expect(frames).toEqual([{type: 'error', message: 'Expected jobAssign frame, got ping'}]);
	});

	it('rejects a frame that fails protocol validation', () => {
		// purgeAfter is z.literal(true): the privacy rule is baked into the type.
		const result = runHarness('harness-bad-frame.ts', JSON.stringify({...jobAssign(), purgeAfter: false}));
		expect(result.status).toBe(1);
		const frames = parseFrames(result.stdout);
		expect(frames).toHaveLength(1);
		expect(frames[0]!.type).toBe('error');
	});

	it('diverts renderer process.stdout.write chatter to stderr and streams protocol frames on stdout', () => {
		const bundle = makeBundleArchive();
		const scratch = mkdtempSync(path.join(tmpdir(), 'runner-core-fixture-'));
		const bundlePath = path.join(scratch, 'bundle.tar.gz');
		const uploadReceipt = path.join(scratch, 'upload.json');
		writeFileSync(bundlePath, bundle.bytes);

		const result = runHarness('harness-render.ts', JSON.stringify(jobAssign({bundleSha256: bundle.sha256})), {
			FIXTURE_BUNDLE_PATH: bundlePath,
			FIXTURE_UPLOAD_RECEIPT: uploadReceipt,
		});

		expect(result.status).toBe(0);
		const frames = parseFrames(result.stdout.split('\n').filter((line) => line.startsWith('{')).join('\n'));
		// Progress is throttled to 5% steps, then a single done frame.
		expect(frames.map((f) => f.type)).toEqual(['progress', 'progress', 'progress', 'progress', 'done']);
		expect(frames.slice(0, 4).map((f) => f.progress)).toEqual([0.1, 0.3, 0.6, 1]);
		expect(frames.at(-1)).toEqual({
			type: 'done',
			outputSizeInBytes: 1234,
			wallTimeMs: expect.any(Number),
			metrics: {wallMs: expect.any(Number), frames: 24, outputSizeInBytes: 1234},
		});

		// THE GUARANTEE: runRunner swaps process.stdout.write before doing
		// anything, so a renderer writing to stdout lands on stderr instead.
		expect(result.stderr).toContain('chatter via raw process.stdout.write');
		expect(result.stdout).not.toContain('chatter via raw process.stdout.write');
		// runner-core's own logs go to stderr via console.error.
		expect(result.stderr).toContain('verified and extracted');

		expect(JSON.parse(readFileSync(uploadReceipt, 'utf8'))).toEqual({bytes: 1234, contentType: 'video/mp4'});
	});

	/**
	 * KNOWN GAP, characterized rather than fixed — this is the behavior of the
	 * payloads already published to operators, and this move is behavior-
	 * preserving by mandate.
	 *
	 * Under Bun, console.log writes straight to fd 1 and does NOT route through
	 * process.stdout.write, so the swap in runRunner cannot catch it: a renderer
	 * that uses console.log interleaves non-JSON lines into the protocol stream.
	 * The supervisor is tolerant (crates/supervisor-core/src/runner.rs logs
	 * `ignoring non-NDJSON runner stdout line` and continues), so this degrades
	 * observability rather than breaking jobs. Change this test deliberately if
	 * the redirect is ever hardened (e.g. by also patching console).
	 */
	it('does NOT currently catch console.log chatter under Bun (known gap)', () => {
		const bundle = makeBundleArchive();
		const scratch = mkdtempSync(path.join(tmpdir(), 'runner-core-fixture-'));
		const bundlePath = path.join(scratch, 'bundle.tar.gz');
		writeFileSync(bundlePath, bundle.bytes);

		const result = runHarness('harness-render.ts', JSON.stringify(jobAssign({bundleSha256: bundle.sha256})), {
			FIXTURE_BUNDLE_PATH: bundlePath,
			FIXTURE_UPLOAD_RECEIPT: path.join(scratch, 'upload.json'),
		});

		expect(result.status).toBe(0);
		expect(result.stdout).toContain('chatter via console.log');
		// It is the only non-frame line, and it does not stop the run.
		expect(stdoutLines(result.stdout).filter((line) => !line.startsWith('{'))).toEqual(['chatter via console.log']);
	});
});
