import {afterAll, beforeAll, describe, expect, it} from 'vitest';
import {spawnSync} from 'node:child_process';
import {closeSync, existsSync, mkdtempSync, openSync, readdirSync, rmSync, truncateSync, writeFileSync} from 'node:fs';
import {homedir, tmpdir} from 'node:os';
import path from 'node:path';

import {maxOutputBytes, verifyRenderedOutput} from '../verify-output.js';

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
	// Tool-free fixtures: written BEFORE the system-ffmpeg early return so the
	// ungated `without ffprobe` describe below works on hosts with no ffmpeg
	// at all (CI runners). Caught by running the suite on Linux CI, where
	// these tests failed with "produced no output file" instead of exercising
	// the no-tools path they exist to pin.
	writeFileSync(clip('zero.mp4'), '');
	writeFileSync(clip('any-nonempty.mp4'), 'not a real video, but the no-tools path never decodes it');
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

	writeFileSync(clip('garbage.mp4'), 'this is definitely not an mp4 file, not even slightly');
});

afterAll(() => {
	if (dir) rmSync(dir, {recursive: true, force: true});
});

const base = {expectedFrames: 30, expectedWidth: 64, expectedHeight: 36, binariesDirectory: systemFfDir};

describe('the ffmpeg gate itself', () => {
	it('fails LOUDLY when REQUIRE_FF=1 and the system ffmpeg is missing', () => {
		// PACKET 39 (audit item 9): the verification suites used to
		// `describe.skipIf(!haveSystemFf)` silently — CI had no ffmpeg, so the
		// black-frame/dead-GPU guard reported green having executed ZERO
		// assertions. CI now installs ffmpeg and sets REQUIRE_FF=1; this test
		// makes the skip impossible: if the tooling ever disappears again
		// (a workflow edit, a broken apt mirror), the job FAILS here instead
		// of quietly disarming. Locally (no REQUIRE_FF) a missing ffmpeg
		// still skips, as before.
		if (process.env.REQUIRE_FF === '1') {
			expect(
				haveSystemFf,
				'REQUIRE_FF=1 but ffmpeg/ffprobe was not found — the output-verification gate would silently skip; refusing to run disarmed',
			).toBe(true);
		}
	});
});

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
			outputLocation: clip('any-nonempty.mp4'),
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

/** Apparent-size blob via a hole: statSync().size is huge, disk cost ~0. */
function sparse(file: string, bytes: number) {
	const handle = openSync(file, 'w');
	truncateSync(file, bytes);
	closeSync(handle);
}

describe('the output size cap', () => {
	it('floors at 2 GiB however small the composition claims', () => {
		// Sparse file: statSync sees APPARENT size without occupying CI disk.
		// A 2.5 GiB blob against 30×64×36 claims (derived 6,912 B) still gets
		// the 2 GiB floor — and trips it. Derived-only would cap a legit
		// 64×36 render (the E2E fixture shape) at ~7 KB and fail real work.
		sparse(clip('huge-floor.mp4'), 2.5 * 1024 ** 3);
		expect(() =>
			verifyRenderedOutput({outputLocation: clip('huge-floor.mp4'), expectedFrames: 30, expectedWidth: 64, expectedHeight: 36, binariesDirectory: null}),
		).toThrow(/exceeding the 2147483648 byte cap/);
	});

	it('derives a higher ceiling from big composition claims', () => {
		// 4 GiB blob: over the ceiling for a 10-minute 1080p30 claim
		// (3,732,480,000 B) → rejected…
		sparse(clip('huge-derived.mp4'), 4 * 1024 ** 3);
		expect(() =>
			verifyRenderedOutput({outputLocation: clip('huge-derived.mp4'), expectedFrames: 18_000, expectedWidth: 1920, expectedHeight: 1080, binariesDirectory: null}),
		).toThrow(/exceeding the 3732480000 byte cap/);
		// …but under it for a 20-minute 4K claim (35.8 GB) → proceeds down
		// the no-tools LOUD path instead. Proves the derived value really
		// replaces the floor, in both directions.
		const logs: string[] = [];
		const probe = verifyRenderedOutput({
			outputLocation: clip('huge-derived.mp4'),
			expectedFrames: 43_200,
			expectedWidth: 3840,
			expectedHeight: 2160,
			binariesDirectory: null,
			log: (m) => logs.push(String(m)),
		});
		expect(probe.codec).toBe('unverified');
		expect(logs.join('\n')).toMatch(/WITHOUT content verification/);
	});

	it('maxOutputBytes: generous math, floors at 2 GiB, tolerates missing claims', () => {
		// Small claims → the floor governs (and protects short renders).
		expect(maxOutputBytes(30, 640, 360)).toBe(2 * 1024 ** 3);
		expect(maxOutputBytes(30, 64, 36)).toBe(2 * 1024 ** 3);
		// Big legit claims → derived, ABOVE the floor: a 10-minute 1080p30
		// may emit 3.73 GB; a 20-minute 4K may emit 35.8 GB.
		expect(maxOutputBytes(18_000, 1920, 1080)).toBe(3_732_480_000);
		expect(maxOutputBytes(43_200, 3840, 2160)).toBe(35_831_808_000);
		// Claims missing or nonsense → the 2 GiB floor, never zero/NaN.
		expect(maxOutputBytes(0, 640, 360)).toBe(2 * 1024 ** 3);
		expect(maxOutputBytes(30, undefined, undefined)).toBe(2 * 1024 ** 3);
		expect(maxOutputBytes(Number.NaN, 640, 360)).toBe(2 * 1024 ** 3);
		expect(maxOutputBytes(30, 0, 360)).toBe(2 * 1024 ** 3);
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
// LOCAL-ONLY INTEGRATION TEST — NOT CI COVERAGE. This block additionally
// requires a real render payload cached in ~/.decent-worker/payloads, which
// only an actual operator machine has (CI installs ffmpeg but has no
// payload). It is skipped everywhere CI runs and pins the DEVELOPER-machine
// path (payload ffmpeg is stripped and dyld-sensitive — see the long comment
// above). Do not read its green-in-CI absence as coverage.
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
