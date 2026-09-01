/**
 * Liveness contract with the supervisor.
 *
 * The supervisor kills a job after SILENCE_TIMEOUT (120s) with no line on the
 * runner's stdout (supervisor-core/src/runner.rs:18). Progress is throttled to
 * 5% deltas, so a heavy composition can legitimately go quiet for longer than
 * that. Until 2026-08-19 the gap was covered by accident — Remotion's
 * Chrome-download logs leaked onto stdout and reset the timer. Shipping the
 * browser inside the payload removed those lines, so liveness must be explicit.
 *
 * These tests exist to stop anyone deleting the heartbeat as "noise".
 */
import {describe, expect, it} from 'vitest';
import {spawnSync} from 'node:child_process';
import {mkdtempSync, writeFileSync} from 'node:fs';
import {tmpdir} from 'node:os';
import path from 'node:path';
import {fileURLToPath} from 'node:url';

import {jobAssign, makeBundleArchive} from './helpers.js';
import {runnerEventSchema} from '../runner-stdout-schema.js';

const here = path.dirname(fileURLToPath(import.meta.url));
const fixtures = path.join(here, 'fixtures');

function runStall(env: Record<string, string>) {
	const bundle = makeBundleArchive();
	const dir = mkdtempSync(path.join(tmpdir(), 'runner-core-stall-'));
	const bundlePath = path.join(dir, 'bundle.tar.gz');
	writeFileSync(bundlePath, bundle.bytes);

	const result = spawnSync('bun', [path.join(fixtures, 'harness-stall.ts')], {
		input: JSON.stringify(jobAssign({bundleSha256: bundle.sha256})),
		encoding: 'utf8',
		env: {
			...process.env,
			HOME: mkdtempSync(path.join(tmpdir(), 'runner-core-stall-home-')),
			FIXTURE_BUNDLE_PATH: bundlePath,
			...env,
		},
	});
	if (result.error) throw result.error;

	const frames = result.stdout
		.split('\n')
		.filter(Boolean)
		.map((line) => {
			const frame = JSON.parse(line) as {type: string};
			// D-62: every emitted frame must satisfy the shared runner-stdout-v1
			// schema (packages/protocol/fixtures/runner-stdout-v1.json).
			runnerEventSchema.parse(frame);
			return frame;
		});
	return {frames, status: result.status, stderr: result.stderr};
}

describe('runner liveness', () => {
	it('emits heartbeat frames while a render is quiet', () => {
		// 600ms of silence at a 100ms interval — several heartbeats, no waiting
		// out the real 15s production interval.
		const {frames, status} = runStall({
			DECENT_RUNNER_HEARTBEAT_MS: '100',
			FIXTURE_STALL_MS: '600',
		});

		expect(status).toBe(0);
		expect(frames.filter((f) => f.type === 'heartbeat').length).toBeGreaterThanOrEqual(2);
		// Heartbeats must not replace the real signals.
		expect(frames.some((f) => f.type === 'progress')).toBe(true);
		expect(frames.at(-1)?.type).toBe('done');
	});

	it('stops heartbeating once the job finishes', () => {
		const {frames} = runStall({
			DECENT_RUNNER_HEARTBEAT_MS: '100',
			FIXTURE_STALL_MS: '300',
		});

		// Nothing may follow the terminal frame, or the supervisor would see
		// traffic for a job it has already settled.
		const done = frames.findIndex((f) => f.type === 'done');
		expect(done).toBeGreaterThanOrEqual(0);
		expect(frames.slice(done + 1)).toEqual([]);
	});

	it('does not heartbeat when progress is flowing', () => {
		// Interval far longer than the stall: progress alone keeps it alive.
		const {frames} = runStall({
			DECENT_RUNNER_HEARTBEAT_MS: '5000',
			FIXTURE_STALL_MS: '200',
		});

		expect(frames.filter((f) => f.type === 'heartbeat')).toEqual([]);
	});
});
