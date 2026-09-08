import {readFileSync, readdirSync, statSync} from 'node:fs';
import path from 'node:path';
import {fileURLToPath} from 'node:url';

import {describe, expect, it} from 'vitest';

/**
 * GREP PIN (F2 accrued-cost protocol): no pricing, no price vocabulary, and
 * no private-package reference may appear in the public node-side source.
 *
 * The public/private repo split is deliberate (workspace CLAUDE.md): the
 * rate card and the settle/accrual arithmetic are the platform's private
 * moat and live in farm-web. The runner reports RAW MEASUREMENTS
 * (elapsedMs/framesSoFar); dispatch prices them. If someone "helpfully"
 * moves pricing into the runner (or even imports the private package), this
 * test fails the commit — a code review of a large diff can miss one import,
 * a grep cannot.
 *
 * The scan covers non-test source only (.ts/.tsx under packages/runner-core/src
 * and packages/protocol/src, .rs under crates/supervisor-core/src excluding
 * the tests.rs modules): fixture and test files legitimately DESCRIBE the
 * contract and may name the forbidden concepts in prose.
 */
const here = path.dirname(fileURLToPath(import.meta.url));

const FORBIDDEN = [
	/packages\/billing/i,
	/\bcredits\b/i,
	/\bcalculateRenderCost\b/,
	/\bpricing\.ts\b/,
];

function isTestFile(file: string): boolean {
	return file.endsWith('__tests__') || file === 'tests.rs' || file.endsWith('.test.ts');
}

function collectSourceFiles(root: string, out: string[] = []): string[] {
	for (const entry of readdirSync(root)) {
		const full = path.join(root, entry);
		if (statSync(full).isDirectory()) {
			if (entry === '__tests__' || entry === 'node_modules' || entry === 'dist') continue;
			collectSourceFiles(full, out);
		} else if (/\.(ts|tsx|rs)$/.test(entry) && !isTestFile(entry)) {
			out.push(full);
		}
	}
	return out;
}

describe('no pricing in the public runner (F2 grep pin)', () => {
	it('the scanned source set is non-empty (the pin has teeth)', () => {
		const files = [
			...collectSourceFiles(path.resolve(here, '../../../runner-core/src')),
			...collectSourceFiles(path.resolve(here, '..')),
		];
		expect(files.length).toBeGreaterThan(5);
	});

	it('no rate-card vocabulary appears in runner-core/protocol source', () => {
		const files = [
			...collectSourceFiles(path.resolve(here, '../../../runner-core/src')),
			...collectSourceFiles(path.resolve(here, '..')),
		];
		const violations: string[] = [];
		for (const file of files) {
			const contents = readFileSync(file, 'utf8');
			for (const pattern of FORBIDDEN) {
				if (pattern.test(contents)) {
					violations.push(`${path.relative(here, file)} matches ${pattern}`);
				}
			}
		}
		expect(violations, `pricing leaked into the public runner:\n  ${violations.join('\n  ')}`).toEqual([]);
	});

	it('no rate-card vocabulary appears in supervisor-core Rust source', () => {
		const rustRoot = path.resolve(here, '../../../../crates/supervisor-core/src');
		const files = collectSourceFiles(rustRoot).filter((f) => !f.endsWith('tests.rs'));
		expect(files.length).toBeGreaterThan(5);
		const violations: string[] = [];
		for (const file of files) {
			const contents = readFileSync(file, 'utf8');
			for (const pattern of FORBIDDEN) {
				if (pattern.test(contents)) {
					violations.push(`${path.relative(here, file)} matches ${pattern}`);
				}
			}
		}
		expect(violations, `pricing leaked into the public supervisor:\n  ${violations.join('\n  ')}`).toEqual([]);
	});
});
