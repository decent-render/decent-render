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

			// 1. TS must ACCEPT what the fixture (canonical = Rust's wire output) carries.
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

	it('fixtures cover the outputSizeInBytes drift scar both ways', () => {
		const names = cases.cases.map((c) => c.name);
		expect(names.some((n) => n.includes('ABSENT'))).toBe(true);
		expect(names.some((n) => n.includes('PRESENT'))).toBe(true);
	});

	it('purgeAfter:false is rejected (privacy rule baked into the type)', () => {
		const assign = cases.cases.find((c) => c.name === 'jobAssign');
		expect(assign).toBeDefined();
		const bad = {...(assign!.wire as object), purgeAfter: false};
		expect(() => ServerMessageSchema.parse(bad)).toThrow();
	});
});
