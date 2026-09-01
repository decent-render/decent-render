import {readFileSync} from 'node:fs';
import path from 'node:path';
import {fileURLToPath} from 'node:url';

import {describe, expect, it} from 'vitest';

import {runnerEventSchema} from '../runner-stdout-schema.js';

const here = path.dirname(fileURLToPath(import.meta.url));
// The fixture lives in packages/protocol (the shared contract home, next to
// v2.json) and is read by relative path — NOT copied — so both languages
// parse the same file (the Rust leg lives in runner.rs's test module).
const fixtures = JSON.parse(
	readFileSync(
		path.resolve(here, '../../../protocol/fixtures/runner-stdout-v1.json'),
		'utf8',
	),
) as {
	contractVersion: number;
	accept: Array<{name: string; wire: unknown}>;
	reject: Array<{name: string; reason: string; wire: unknown}>;
};

describe('runner stdout v1 — Rust⇄TS golden-fixture conformance', () => {
	// Teeth: a suite that iterates an empty fixture set passes vacuously.
	it('the accept fixture set is non-empty', () => {
		expect(fixtures.accept.length).toBeGreaterThan(0);
	});

	it('the reject fixture set is non-empty', () => {
		expect(fixtures.reject.length).toBeGreaterThan(0);
	});

	it.each(fixtures.accept)('accept: $name', ({wire}) => {
		const parsed = runnerEventSchema.parse(wire);
		// The discriminated union must keep the tag on the parsed value.
		expect(parsed.type).toBe((wire as {type: string}).type);
	});

	it.each(fixtures.reject)('reject: $name', ({wire}) => {
		expect(() => runnerEventSchema.parse(wire)).toThrow();
	});
});
