import {readFileSync} from 'node:fs';
import path from 'node:path';
import {fileURLToPath} from 'node:url';

import {describe, expect, it} from 'vitest';

import {ServerMessageSchema, WorkerMessageSchema, PROTOCOL_VERSION} from '../index';

const here = path.dirname(fileURLToPath(import.meta.url));
const cases = JSON.parse(
	readFileSync(path.resolve(here, '../../fixtures/v2.json'), 'utf8'),
) as {
	protocolVersion: number;
	cases: Array<{name: string; direction: 'worker' | 'server'; wire: unknown}>;
	reject: Array<{
		name: string;
		direction: 'worker' | 'server';
		reason: string;
		wire: unknown;
	}>;
};

/**
 * Recursively collect every leaf path (a.b, a[0].c, a[] for empty arrays) —
 * the deep "field set" of a value. Two values with the same field set expose
 * the same shape, regardless of key order.
 */
function deepKeyPaths(value: unknown, prefix = ''): string[] {
	if (value === null || typeof value !== 'object') {
		return prefix ? [prefix] : [];
	}

	if (Array.isArray(value)) {
		if (value.length === 0) return [`${prefix}[]`];
		return value.flatMap((v, i) => deepKeyPaths(v, `${prefix}[${i}]`));
	}

	const obj = value as Record<string, unknown>;
	return Object.keys(obj)
		.sort()
		.flatMap((k) => deepKeyPaths(obj[k], prefix ? `${prefix}.${k}` : k));
}

describe('protocol v2 — Rust⇄TS golden-fixture conformance', () => {
	// Teeth (C-2): a suite that iterates an empty fixture set passes vacuously.
	// Both sides assert the sets are populated so a broken fixture path or a
	// gutted file cannot turn into a green run.
	it('the positive fixture set is non-empty', () => {
		expect(cases.cases.length).toBeGreaterThan(0);
	});

	it('PROTOCOL_VERSION stays pinned at 2 — additive fields, never a bump (F2/I10 §1.4)', () => {
		// The version pin is a REGISTER-TIME gate, not a capability signal:
		// `protocolVersion: z.literal(PROTOCOL_VERSION)` (and Rust's
		// `pinned_protocol_version`) make a node announcing any other version
		// fail the register parse entirely — an old node speaking v2 to a v3
		// dispatch becomes an UNREGISTERED socket, never assigned, never told
		// why (inspection-I10 §1.4). A bump therefore orphans every existing
		// node at register. Additive-optional fields like jobProgress's
		// elapsedMs/framesSoFar (F2, accrued-cost protocol) are wire-tolerant
		// in BOTH directions — unknown fields are stripped by the receiving
		// zod object and ignored by Rust's serde (no deny_unknown_fields) —
		// which is exactly why this change needs NO bump. If this test fails,
		// you changed the wire in a way that breaks old nodes; either revert to
		// an additive shape or make the fleet-orphaning an explicit, human-
		// gated decision instead of a test edit.
		expect(PROTOCOL_VERSION).toBe(2);
		expect(cases.protocolVersion).toBe(2);
	});

	it('old↔new tolerance: an old-node jobProgress (no accrued fields) parses under the NEW schema', () => {
		const oldNodeFrame = {
			type: 'jobProgress',
			tenant: 'driffs',
			jobId: 'spike-1',
			attempt: 1,
			progress: 0.5,
		};
		const parsed = WorkerMessageSchema.parse(oldNodeFrame);
		if (parsed.type !== 'jobProgress') throw new Error('expected jobProgress');
		// Fields stay ABSENT (not zero, not null) — dispatch leaves accrued
		// null for such nodes by design.
		expect(parsed.elapsedMs).toBeUndefined();
		expect(parsed.framesSoFar).toBeUndefined();
	});

	it('old↔new tolerance: a new-node jobProgress keeps its accrued measurements through the round-trip', () => {
		const newNodeFrame = {
			type: 'jobProgress',
			tenant: 'driffs',
			jobId: 'spike-1',
			attempt: 1,
			progress: 0.35,
			elapsedMs: 8453,
			framesSoFar: 105,
		};
		const parsed = WorkerMessageSchema.parse(newNodeFrame);
		expect(JSON.parse(JSON.stringify(parsed))).toEqual(newNodeFrame);
	});

	it('the negative fixture set is non-empty', () => {
		expect(cases.reject.length).toBeGreaterThan(0);
	});

	for (const c of cases.cases) {
		it(`${c.direction} → ${c.name}: parses + round-trips with no field drift`, () => {
			const schema =
				c.direction === 'worker' ? WorkerMessageSchema : ServerMessageSchema;

			// 1. TS must ACCEPT what the shared fixture (locked from Rust) carries.
			//    If TS requires a field Rust never sends, parse throws here.
			const parsed = schema.parse(c.wire);

			// 2. Round-trip through JSON (the wire) and assert no field was dropped.
			//    If the fixture carries a field TS's schema lacks, zod strips it on
			//    parse -> the reserialized field set is missing it -> FAIL. This is
			//    exactly the outputSizeInBytes drift class. Field-SET equality (not
			//    byte) so it isn't brittle to key order or whitespace.
			const reserialized = JSON.parse(JSON.stringify(parsed));
			expect(deepKeyPaths(reserialized).sort()).toEqual(
				deepKeyPaths(c.wire).sort(),
			);
		});
	}

	/**
	 * Fixture cases are found by wire `type`, never by their display name — the
	 * names carry human explanation and get reworded, and a test that silently
	 * stops finding its subject is worse than no test.
	 */
	const casesOfType = (type: string) =>
		cases.cases.filter(
			(c) => (c.wire as {type?: string}).type === type,
		);
	const firstOfType = (type: string) => {
		const [found] = casesOfType(type);
		expect(found, `no ${type} fixture`).toBeDefined();
		return found!.wire as Record<string, unknown>;
	};

	it('fixtures cover the outputSizeInBytes drift scar both ways', () => {
		// Scoped to jobComplete on purpose: other cases now use ABSENT/PRESENT in
		// their names too, so an unscoped search would keep passing after the
		// jobComplete pair — the actual scar — was deleted.
		const names = casesOfType('jobComplete').map((c) => c.name);
		expect(names.some((n) => n.includes('ABSENT'))).toBe(true);
		expect(names.some((n) => n.includes('PRESENT'))).toBe(true);
	});

	it('fixtures cover the browser artifact both split out and bundled', () => {
		const assigns = casesOfType('jobAssign');
		expect(assigns.some((c) => 'browserSha256' in (c.wire as object))).toBe(true);
		expect(assigns.some((c) => !('browserSha256' in (c.wire as object)))).toBe(true);
	});

	it('old↔new tolerance: an old-dispatch jobAssign (no still directive) parses under the NEW schema', () => {
		const wire = firstOfType('jobAssign');
		expect('still' in wire).toBe(false);
		const parsed = ServerMessageSchema.parse(wire);
		if (parsed.type !== 'jobAssign') throw new Error('expected jobAssign');
		// The field stays ABSENT (not null) — the runner branches on presence.
		expect(parsed.still).toBeUndefined();
	});

	it('old↔new tolerance: a still jobAssign keeps its directive through the round-trip (FARM-STILL)', () => {
		const still = casesOfType('jobAssign').find((c) => 'still' in (c.wire as object));
		expect(still, 'no still jobAssign fixture').toBeDefined();
		const parsed = ServerMessageSchema.parse(still!.wire);
		if (parsed.type !== 'jobAssign') throw new Error('expected jobAssign');
		expect(JSON.parse(JSON.stringify(parsed))).toEqual(still!.wire);
	});

	it('fixtures cover the still directive both PRESENT and ABSENT (FARM-STILL)', () => {
		const assigns = casesOfType('jobAssign');
		expect(assigns.some((c) => 'still' in (c.wire as object))).toBe(true);
		expect(assigns.some((c) => !('still' in (c.wire as object)))).toBe(true);
	});

	it('the still cross-field bound refuses frame ≥ durationFrames at parse (FARM-STILL)', () => {
		// Mirrored by a reject fixture AND by Rust's hand-written Deserialize;
		// asserted directly here so the refine's teeth are visible without
		// reading the fixture file.
		const wire = firstOfType('jobAssign') as Record<string, unknown> & {
			still?: {frame: number; format: string};
		};
		const bad = {...wire, still: {frame: wire.durationFrames as number, format: 'png'}};
		expect(ServerMessageSchema.safeParse(bad).success).toBe(false);
	});

	it('fixtures cover the accrued measurements both PRESENT and ABSENT (F2)', () => {
		// Scoped to jobProgress: the ABSENT case is what an old node sends and
		// must keep parsing forever; the PRESENT case is the F2 raw-measurement
		// pair. Deleting either half silently narrows the tolerance contract.
		const progresses = casesOfType('jobProgress');
		expect(progresses.some((c) => 'elapsedMs' in (c.wire as object) && 'framesSoFar' in (c.wire as object))).toBe(true);
		expect(progresses.some((c) => !('elapsedMs' in (c.wire as object)))).toBe(true);
	});

	it('accepts assignments with no browser artifact (payload ships its own)', () => {
		const withBrowser = casesOfType('jobAssign')
			.map((c) => c.wire as Record<string, unknown>)
			.find((w) => 'browserSha256' in w);
		expect(withBrowser).toBeDefined();
		const {browserSha256, browserGetUrl, ...without} = withBrowser!;
		expect(ServerMessageSchema.parse(without)).toEqual(without);
	});

	it('accepts legacy assignment frames without an attempt lease', () => {
		const accepted = {type: 'jobAccepted', tenant: 'driffs', jobId: 'legacy-1'};
		expect(WorkerMessageSchema.parse(accepted)).toEqual(accepted);

		const legacyAssign = {...firstOfType('jobAssign')};
		delete legacyAssign.attempt;
		expect(ServerMessageSchema.parse(legacyAssign)).toEqual(legacyAssign);
	});

	// Negative contract: every `reject` fixture must FAIL to parse. Rust asserts
	// the same entries fail serde, so neither side can quietly loosen a bound
	// (protocolVersion pin, progress range, codec set) without the other noticing.
	for (const c of cases.reject) {
		it(`${c.direction} -> REJECTS ${c.name}`, () => {
			const schema =
				c.direction === 'worker' ? WorkerMessageSchema : ServerMessageSchema;
			expect(() => schema.parse(c.wire), c.reason).toThrow();
		});
	}

	it('purgeAfter:false is rejected (privacy rule baked into the type)', () => {
		const bad = {...firstOfType('jobAssign'), purgeAfter: false};
		expect(() => ServerMessageSchema.parse(bad)).toThrow();
	});
});
