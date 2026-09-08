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

/**
 * F-6 (verify): the brief's check is "the word `pricing` → 0", but the pin
 * above only banned `pricing.ts`, so prohibition PROSE survived (e.g.
 * "pricing is the dispatch service's concern" — statements that pricing does
 * NOT happen there). Widened: the bare word `pricing` is banned in non-test
 * source unless the EXACT line is on the allow-list below. Every entry is a
 * documented "pricing does not happen here" line; a NEW use of the word
 * fails the pin until it is consciously added here, and a stale entry fails
 * the allow-list-freshness test, so the list cannot rot silently.
 */
const PRICING_WORD = /\bpricing\b/i;
const ALLOWED_PRICING_PROSE: ReadonlySet<string> = new Set([
	// runner-core/src/runner-stdout-schema.ts — what the fields are NOT.
	"* ONLY — never money, never a price. Pricing is the dispatch service's",
	// runner-core/src/index.ts — the emitter forwards raw numbers only.
	'// on the existing 5 % throttled events. Raw numbers only; pricing',
	// protocol/src/index.ts — the runner must never price its own work.
	'* the runner cannot price its own work and must never try — pricing is the',
	// supervisor-core/src/protocol.rs — Rust twin of the same statement.
	"/// The runner cannot price its own work — pricing is the platform's private",
	// supervisor-core/src/runner.rs — forwarding doc, untouched values.
	'/// render start. Forwarded to dispatch untouched; pricing accrued',
	// supervisor-core/src/runner.rs — progress_to_wire doc (added with the
	// F-3 forwarding pin): the same statement at the extraction site.
	"/// The measurements must ride UNTOUCHED; pricing is dispatch's private",
]);

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

const runnerCoreAndProtocolFiles = () => [
	...collectSourceFiles(path.resolve(here, '../../../runner-core/src')),
	...collectSourceFiles(path.resolve(here, '..')),
];

const supervisorRustFiles = () =>
	collectSourceFiles(path.resolve(here, '../../../../crates/supervisor-core/src'))
		.filter((f) => !f.endsWith('tests.rs'));

/** F-6: lines using the bare word `pricing` that are not allow-listed prose. */
function pricingProseViolations(files: string[]): string[] {
	const violations: string[] = [];
	for (const file of files) {
		const lines = readFileSync(file, 'utf8').split('\n');
		lines.forEach((line, index) => {
			if (PRICING_WORD.test(line) && !ALLOWED_PRICING_PROSE.has(line.trim())) {
				violations.push(`${path.relative(here, file)}:${index + 1}: ${line.trim()}`);
			}
		});
	}
	return violations;
}

/** F-6 freshness: every allow-list entry must still exist verbatim somewhere. */
function allowListedLinesStillPresent(files: string[]): string[] {
	const present = new Set<string>();
	for (const file of files) {
		for (const line of readFileSync(file, 'utf8').split('\n')) {
			const trimmed = line.trim();
			if (PRICING_WORD.test(trimmed)) present.add(trimmed);
		}
	}
	return [...ALLOWED_PRICING_PROSE].filter((allowed) => !present.has(allowed));
}

describe('no pricing in the public runner (F2 grep pin)', () => {
	it('the scanned source set is non-empty (the pin has teeth)', () => {
		const files = runnerCoreAndProtocolFiles();
		expect(files.length).toBeGreaterThan(5);
	});

	it('no rate-card vocabulary appears in runner-core/protocol source', () => {
		const files = runnerCoreAndProtocolFiles();
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
		const files = supervisorRustFiles();
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

	it('F-6: the bare word "pricing" is banned outside the documented allow-list (runner-core/protocol)', () => {
		const violations = pricingProseViolations(runnerCoreAndProtocolFiles());
		expect(violations, `undocumented pricing prose in the public runner:\n  ${violations.join('\n  ')}`).toEqual([]);
	});

	it('F-6: the bare word "pricing" is banned outside the documented allow-list (supervisor-core)', () => {
		const violations = pricingProseViolations(supervisorRustFiles());
		expect(violations, `undocumented pricing prose in the public supervisor:\n  ${violations.join('\n  ')}`).toEqual([]);
	});

	it('F-6 freshness: every allow-list entry still exists verbatim (no rot)', () => {
		// The UNION of both scanned sets — the allow-list is global, so an
		// entry may legitimately live in either.
		const stale = allowListedLinesStillPresent([...runnerCoreAndProtocolFiles(), ...supervisorRustFiles()]);
		expect(stale, `stale allow-list entries (lines no longer present — prune them):\n  ${stale.join('\n  ')}`).toEqual([]);
	});
});
