import {readFileSync} from 'node:fs';
import path from 'node:path';
import {fileURLToPath} from 'node:url';

import {describe, expect, it} from 'vitest';

import {JobCanceledAckMessageSchema, WorkerMessageSchema} from '../index';

const here = path.dirname(fileURLToPath(import.meta.url));
const fixtures = JSON.parse(
	readFileSync(path.resolve(here, '../../fixtures/v2.json'), 'utf8'),
) as {
	cases: Array<{name: string; direction: 'worker' | 'server'; wire: unknown}>;
	reject: Array<{name: string; direction: 'worker' | 'server'; wire: unknown}>;
};

/**
 * CANCEL-ACK CENSUS (F4) — the ack frame, pinned.
 *
 * Baseline (previous commit): the census pinned that NO worker→server frame
 * acknowledged a cancel (FARM-1 §1-C's gap — the terminal jobFailed was
 * suppressed at the node with nothing sent in its place). This rewrite pins
 * the frame that closes it: `jobCanceledAck {tenant, jobId, attempt,
 * teardownMs?}` — attempt REQUIRED (the provenance key), teardownMs an
 * optional int ≥ 0 (ms from cancel receipt to process-tree dead + purge
 * done). Emitted by the supervisor AFTER the job task is joined, never on
 * cancel-frame receipt.
 */

/** The worker→server `type` literals, derived from the zod discriminated union. */
function workerFrameTypes(): string[] {
	return WorkerMessageSchema.options
		.map((option) => option.shape.type.value as string)
		.sort();
}

describe('cancel-ack census (F4: the frame)', () => {
	it('the worker→server census is exactly the eight known frame types', () => {
		// Exact-set, not contains: a frame type added OR removed must trip
		// this census, so every future wire change rewrites this list
		// deliberately.
		expect(workerFrameTypes()).toEqual([
			'heartbeat',
			'jobAccepted',
			'jobCanceledAck',
			'jobComplete',
			'jobFailed',
			'jobProgress',
			'jobRejected',
			'register',
		]);
	});

	it('the only cancel-acknowledging frame is jobCanceledAck', () => {
		const cancelish = workerFrameTypes().filter((type) =>
			/cancel|stop/i.test(type),
		);
		expect(cancelish).toEqual(['jobCanceledAck']);
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

describe('jobCanceledAck schema (F4)', () => {
	it('attempt is REQUIRED — the provenance key', () => {
		expect(() =>
			JobCanceledAckMessageSchema.parse({
				type: 'jobCanceledAck',
				tenant: 'driffs',
				jobId: 'spike-1',
				teardownMs: 812,
			}),
		).toThrow();
	});

	it('teardownMs is optional, integer, ≥ 0', () => {
		expect(() =>
			JobCanceledAckMessageSchema.parse({
				type: 'jobCanceledAck',
				tenant: 'driffs',
				jobId: 'spike-1',
				attempt: 1,
			}),
		).not.toThrow();
		expect(() =>
			JobCanceledAckMessageSchema.parse({
				type: 'jobCanceledAck',
				tenant: 'driffs',
				jobId: 'spike-1',
				attempt: 1,
				teardownMs: -1,
			}),
		).toThrow();
		expect(() =>
			JobCanceledAckMessageSchema.parse({
				type: 'jobCanceledAck',
				tenant: 'driffs',
				jobId: 'spike-1',
				attempt: 1,
				teardownMs: '812',
			}),
		).toThrow();
	});

	it('attempt must be a positive integer (0 is not a lease)', () => {
		expect(() =>
			JobCanceledAckMessageSchema.parse({
				type: 'jobCanceledAck',
				tenant: 'driffs',
				jobId: 'spike-1',
				attempt: 0,
			}),
		).toThrow();
	});

	it('old↔new tolerance: fixtures cover teardownMs both PRESENT and ABSENT, plus the reject set', () => {
		const acks = fixtures.cases.filter(
			(c) => (c.wire as {type?: string}).type === 'jobCanceledAck',
		);
		expect(
			acks.some((c) => 'teardownMs' in (c.wire as object)),
			'fixture must carry the teardownMs-PRESENT case',
		).toBe(true);
		expect(
			acks.some((c) => !('teardownMs' in (c.wire as object))),
			'fixture must carry the teardownMs-ABSENT case (old-tolerance shape)',
		).toBe(true);
		const ackRejects = fixtures.reject.filter(
			(c) => (c.wire as {type?: string}).type === 'jobCanceledAck',
		);
		expect(ackRejects.length).toBe(3);
	});

	it('a new-node ack parses through the FULL worker union (what dispatch will validate)', () => {
		const parsed = WorkerMessageSchema.parse({
			type: 'jobCanceledAck',
			tenant: 'driffs',
			jobId: 'spike-1',
			attempt: 2,
			teardownMs: 812,
		});
		expect(JSON.parse(JSON.stringify(parsed))).toEqual({
			type: 'jobCanceledAck',
			tenant: 'driffs',
			jobId: 'spike-1',
			attempt: 2,
			teardownMs: 812,
		});
	});
});
