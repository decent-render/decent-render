import {readFileSync} from 'node:fs';
import path from 'node:path';
import {fileURLToPath} from 'node:url';

import {describe, expect, it} from 'vitest';

import {ServerMessageSchema, WorkerMessageSchema} from '../index';

const here = path.dirname(fileURLToPath(import.meta.url));
const cases = JSON.parse(
	readFileSync(path.resolve(here, '../../fixtures/v2.json'), 'utf8'),
) as {
	protocolVersion: number;
	cases: Array<{name: string; direction: 'worker' | 'server'; wire: unknown}>;
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

	it('purgeAfter:false is rejected (privacy rule baked into the type)', () => {
		const bad = {...firstOfType('jobAssign'), purgeAfter: false};
		expect(() => ServerMessageSchema.parse(bad)).toThrow();
	});
});
