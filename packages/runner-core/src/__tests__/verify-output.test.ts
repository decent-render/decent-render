import {afterAll, beforeAll, describe, expect, it} from 'vitest';
import {spawnSync} from 'node:child_process';
import {existsSync, mkdtempSync, readdirSync, rmSync, writeFileSync} from 'node:fs';
import {homedir, tmpdir} from 'node:os';
import path from 'node:path';

import {verifyRenderedOutput} from '../verify-output.js';

/**
 * Fixtures are BUILT with the system ffmpeg (it has the lavfi sources needed
 * to synthesise clips) but the code under test is pointed at a binaries
 * directory the same shape as a render payload's. See the production-binary
 * describe block at the bottom for why that distinction matters.
 */
const systemFfmpeg = spawnSync('which', ['ffmpeg'], {encoding: 'utf8'}).stdout.trim();
const systemFfDir = systemFfmpeg === '' ? null : path.dirname(systemFfmpeg);
const haveSystemFf = systemFfDir !== null && existsSync(path.join(systemFfDir, 'ffprobe'));

/** A real payload's `remotion-binaries/`, if this machine has one cached. */
function findPayloadBinaries(): string | null {
	const payloads = path.join(homedir(), '.decent-worker', 'payloads');
	if (!existsSync(payloads)) return null;
	for (const entry of readdirSync(payloads)) {
		// sha-named dirs only — never the test-* litter.
		if (!/^[0-9a-f]{64}$/.test(entry)) continue;
		const binaries = path.join(payloads, entry, 'remotion-binaries');
		if (existsSync(path.join(binaries, 'ffprobe')) && existsSync(path.join(binaries, 'ffmpeg'))) return binaries;
	}
	return null;
}

let dir: string;
const clip = (name: string) => path.join(dir, name);

function synthesise(args: string[], output: string) {
	const result = spawnSync(systemFfmpeg, ['-v', 'error', '-y', ...args, output], {encoding: 'utf8'});
	if (result.status !== 0) throw new Error(`fixture build failed for ${output}: ${result.stderr}`);
}

beforeAll(() => {
	dir = mkdtempSync(path.join(tmpdir(), 'verify-output-'));
	if (!haveSystemFf) return;
	const encode = ['-frames:v', '30', '-c:v', 'libx264', '-pix_fmt', 'yuv420p'];
	// Moving content — what a healthy render looks like.
	synthesise(['-f', 'lavfi', '-i', 'testsrc=size=64x36:rate=30', ...encode], clip('good.mp4'));
	// The dead-GPU signature.
	synthesise(['-f', 'lavfi', '-i', 'color=c=black:s=64x36:r=30', ...encode], clip('black.mp4'));
	// Truncated render: real frames, wrong count.
	synthesise(['-f', 'lavfi', '-i', 'testsrc=size=64x36:rate=30', '-frames:v', '10', '-c:v', 'libx264', '-pix_fmt', 'yuv420p'], clip('short.mp4'));
	// Legitimately static, and NOT black — must survive verification.
	synthesise(['-f', 'lavfi', '-i', 'color=c=white:s=64x36:r=30', ...encode], clip('static.mp4'));
	// Wrong dimensions for the composition we will declare.
	synthesise(['-f', 'lavfi', '-i', 'testsrc=size=32x18:rate=30', ...encode], clip('small.mp4'));

	writeFileSync(clip('zero.mp4'), '');
	writeFileSync(clip('garbage.mp4'), 'this is definitely not an mp4 file, not even slightly');
});

afterAll(() => {
	if (dir) rmSync(dir, {recursive: true, force: true});
});

const base = {expectedFrames: 30, expectedWidth: 64, expectedHeight: 36, binariesDirectory: systemFfDir};

describe.skipIf(!haveSystemFf)('verifyRenderedOutput', () => {
	it('accepts a healthy render and returns MEASURED values', () => {
		const probe = verifyRenderedOutput({...base, outputLocation: clip('good.mp4')});
		expect(probe.frames).toBe(30);
		expect(probe.width).toBe(64);
		expect(probe.height).toBe(36);
		expect(probe.codec).toBe('h264');
	});

	it('rejects an all-black render — the dead-GPU signature', () => {
		expect(() => verifyRenderedOutput({...base, outputLocation: clip('black.mp4')})).toThrow(/uniformly black/i);
	});

	it('rejects a render whose frame count contradicts the composition', () => {
		expect(() => verifyRenderedOutput({...base, outputLocation: clip('short.mp4')})).toThrow(/has 10 frames, composition declares 30/);
	});

	it('rejects a render whose dimensions contradict the composition', () => {
		expect(() => verifyRenderedOutput({...base, outputLocation: clip('small.mp4')})).toThrow(/is 32x18, composition declares 64x36/);
	});

	it('rejects a zero-byte output', () => {
		expect(() => verifyRenderedOutput({...base, outputLocation: clip('zero.mp4')})).toThrow(/zero-byte/);
	});

	it('rejects a file that is not decodable video', () => {
		expect(() => verifyRenderedOutput({...base, outputLocation: clip('garbage.mp4')})).toThrow(/does not decode|no video stream/);
	});

	it('rejects a missing output file', () => {
		expect(() => verifyRenderedOutput({...base, outputLocation: clip('never-written.mp4')})).toThrow(/produced no output file/);
	});

	it('ACCEPTS a legitimately static render, warning rather than failing', () => {
		// A held title card renders identical frames. Failing those to catch a
		// dead GPU would break real work; the black-frame check already covers
		// the fault, so this must pass.
		const logs: string[] = [];
		const probe = verifyRenderedOutput({...base, outputLocation: clip('static.mp4'), log: (m) => logs.push(String(m))});
		expect(probe.frames).toBe(30);
		expect(logs.join('\n')).toMatch(/identical/i);
	});

	it('skips dimension checking LOUDLY when the composition exposes none', () => {
		const logs: string[] = [];
		const probe = verifyRenderedOutput({
			outputLocation: clip('good.mp4'),
			expectedFrames: 30,
			binariesDirectory: systemFfDir,
			log: (m) => logs.push(String(m)),
		});
		expect(probe.frames).toBe(30);
		expect(logs.join('\n')).toMatch(/dimension check skipped/i);
	});
});

describe('verifyRenderedOutput without ffprobe', () => {
	it('warns LOUDLY and does not block the upload', () => {
		// Refusing to upload an otherwise good render because a diagnostic
		// tool is missing would be a worse failure than the one guarded
		// against — but it must never be silent.
		const logs: string[] = [];
		const probe = verifyRenderedOutput({
			outputLocation: clip('good.mp4'),
			expectedFrames: 30,
			binariesDirectory: null,
			log: (m) => logs.push(String(m)),
		});
		expect(probe.codec).toBe('unverified');
		expect(logs.join('\n')).toMatch(/WITHOUT content verification/);
	});

	it('still rejects a zero-byte output, which needs no tools to detect', () => {
		expect(() =>
			verifyRenderedOutput({outputLocation: clip('zero.mp4'), expectedFrames: 30, binariesDirectory: null}),
		).toThrow(/zero-byte/);
	});
});

/**
 * REGRESSION GUARD for the defect that only running this found: Remotion
 * ships a STRIPPED ffmpeg (50 filters — `scale`/`format` present, `select`
 * absent) whose binaries are dynamically linked against sibling dylibs
 * resolved relative to the CURRENT WORKING DIRECTORY. A verifier written and
 * tested against a developer's homebrew ffmpeg passes every test above and
 * then fails on every production node — first with a dyld "Library not
 * loaded", reported as "output does not decode".
 *
 * So: run the real thing against the real binaries.
 */
const payloadBinaries = findPayloadBinaries();
describe.skipIf(!haveSystemFf || payloadBinaries === null)("the render payload's own ffmpeg", () => {
	it('supports every operation the verifier depends on', () => {
		const probe = verifyRenderedOutput({
			...base,
			binariesDirectory: payloadBinaries,
			outputLocation: clip('good.mp4'),
		});
		expect(probe.frames).toBe(30);
		expect(probe.codec).toBe('h264');
	});

	it('detects black frames with the payload binaries too', () => {
		expect(() =>
			verifyRenderedOutput({...base, binariesDirectory: payloadBinaries, outputLocation: clip('black.mp4')}),
		).toThrow(/uniformly black/i);
	});
});
