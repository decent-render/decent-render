import {readFileSync} from 'node:fs';
import path from 'node:path';
import {fileURLToPath} from 'node:url';

import {describe, expect, it} from 'vitest';

import {WorkerMessageSchema} from '../index';

const here = path.dirname(fileURLToPath(import.meta.url));
const fixtures = JSON.parse(
	readFileSync(path.resolve(here, '../../fixtures/v2.json'), 'utf8'),
) as {
	cases: Array<{name: string; direction: 'worker' | 'server'; wire: unknown}>;
};

/**
 * CANCEL-ACK CENSUS (F4 baseline) — the ack gap, pinned.
 *
 * FARM-1 §1-C walked the cancel path hop by hop and found: the terminal
 * `jobFailed("Render canceled by dispatch")` the runner emits is SUPPRESSED
 * at the node (connection.rs `suppress_after_cancel`), so **no worker→server
 * frame of any kind acknowledges a dispatch-initiated cancel**. The only
 * implicit signals are the 20 s heartbeat's job COUNT (not job-scoped) and
 * a node-local status counter. "Time from cancel request → node actually
 * stopped" is therefore unmeasurable at dispatch/driffs today.
 *
 * This census pins that gap from the protocol side: the worker→server
 * frame-type set is derived from the schema union itself (not hand-listed),
 * asserted to contain NO cancel-acknowledging frame, and cross-checked
 * against the shared fixtures. When F4 adds `jobCanceledAck` this test is
 * REWRITTEN to pin the new frame's presence + provenance key (`attempt`
 * REQUIRED) — the red→green pair is the point: the baseline proves the gap
 * existed, the rewrite proves it closed, and neither is a silent edit.
 */

/** The worker→server `type` literals, derived from the zod discriminated union. */
function workerFrameTypes(): string[] {
	return WorkerMessageSchema.options
		.map((option) => option.shape.type.value as string)
		.sort();
}

describe('cancel-ack census (F4 baseline: the gap)', () => {
	it('the worker→server census is exactly the seven known frame types', () => {
		// Exact-set, not contains: a frame type added OR removed must trip
		// this census, so the rewrite in the F4 commit is a deliberate act.
		expect(workerFrameTypes()).toEqual([
			'heartbeat',
			'jobAccepted',
			'jobComplete',
			'jobFailed',
			'jobProgress',
			'jobRejected',
			'register',
		]);
	});

	it('NO worker→server frame acknowledges a cancel (the F4 gap, pinned)', () => {
		// The gap pin. Any frame whose type names a cancel acknowledgement
		// (`jobCanceledAck`, `cancelAcked`, `jobStopped`, …) flips this red —
		// that is the moment F4 lands, and the census rewrite must accompany
		// it in the same commit.
		const cancelish = workerFrameTypes().filter((type) =>
			/cancel|stop/i.test(type),
		);
		expect(
			cancelish,
			'a cancel-acknowledging frame exists — rewrite this census per F4 (attempt REQUIRED, teardownMs optional int ≥ 0)',
		).toEqual([]);
	});

	it('the shared fixtures carry no worker frame outside the census', () => {
		const census = new Set(workerFrameTypes());
		const workerFixtureTypes = fixtures.cases
			.filter((c) => c.direction === 'worker')
			.map((c) => (c.wire as {type: string}).type);
		expect(workerFixtureTypes.length).toBeGreaterThan(0);
		for (const type of workerFixtureTypes) {
			expect(census.has(type), `fixture type ${type} missing from the schema union`).toBe(
				true,
			);
		}
	});
});
