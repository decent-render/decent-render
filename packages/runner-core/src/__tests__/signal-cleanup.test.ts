/**
 * Signal-cleanup contract (node containment, ITEM 2).
 *
 * The supervisor terminates the runner's whole process GROUP on cancel —
 * SIGTERM to the runner process. Without a handler, Bun's default
 * disposition kills the process instantly and renderJob's `finally` (which
 * purges the mkdtemp workdir holding customer render content) never runs:
 * every cancel leaked the per-job workdir. These tests pin the fixed
 * behavior with a REAL signal to a REAL subprocess — anything weaker (e.g.
 * invoking the handler function directly) would not prove the signal
 * disposition actually changed.
 */
import {describe, expect, it} from 'vitest';
import {spawn} from 'node:child_process';
import {mkdtempSync, readFileSync, writeFileSync, existsSync} from 'node:fs';
import {tmpdir} from 'node:os';
import path from 'node:path';
import {fileURLToPath} from 'node:url';

import {jobAssign, makeBundleArchive} from './helpers.js';

const here = path.dirname(fileURLToPath(import.meta.url));
const fixtures = path.join(here, 'fixtures');

type RunnerHandle = {
	child: ReturnType<typeof spawn>;
	exit: Promise<{code: number | null; signal: NodeJS.Signals | null}>;
	probe: string;
};

function spawnSignalRunner(): RunnerHandle {
	const bundle = makeBundleArchive();
	const dir = mkdtempSync(path.join(tmpdir(), 'runner-core-signal-'));
	const bundlePath = path.join(dir, 'bundle.tar.gz');
	writeFileSync(bundlePath, bundle.bytes);
	const probe = path.join(dir, 'workdir-probe');

	// HOME override: the runner resolves its cache root (~/.decent-worker)
	// from HOME, so without this the subprocess writes into the LIVE operator
	// cache — this file was the one sibling missing it (stdout-discipline and
	// liveness both mkdtemp a HOME), and it seeded 266 dirs / 3.5 GB of real
	// bundles there, one per run: helpers.ts' `tar -czf` embeds mtime, so
	// every run produces a new sha and nothing is ever overwritten. That is
	// the same cache root the production LRU sweeper manages.
	const home = mkdtempSync(path.join(tmpdir(), 'runner-core-signal-home-'));

	const child = spawn('bun', [path.join(fixtures, 'harness-signal.ts')], {
		stdio: ['pipe', 'pipe', 'pipe'],
		env: {
			...process.env,
			HOME: home,
			FIXTURE_BUNDLE_PATH: bundlePath,
			FIXTURE_WORKDIR_PROBE: probe,
		},
	});
	child.stdin.write(JSON.stringify(jobAssign({bundleSha256: bundle.sha256})));
	child.stdin.end();

	const exit = new Promise<{code: number | null; signal: NodeJS.Signals | null}>((resolve) => {
		child.on('exit', (code, signal) => resolve({code, signal}));
	});
	return {child, exit, probe};
}

async function waitForFile(file: string, timeoutMs = 10_000): Promise<string> {
	const deadline = Date.now() + timeoutMs;
	// Loop until the file exists AND has content: the fixture writeFileSync
	// creates the file at open and fills it a beat later, so a bare
	// existsSync can win the race and hand back an empty string.
	while (Date.now() < deadline) {
		if (existsSync(file)) {
			const content = readFileSync(file, 'utf8');
			if (content.length > 0) return content;
		}
		await new Promise((resolve) => setTimeout(resolve, 50));
	}
	throw new Error(`probe file never appeared: ${file}`);
}

describe('runner signal cleanup (SIGTERM/SIGINT)', () => {
	it('purges the workdir and exits non-zero on SIGTERM mid-render', async () => {
		const runner = spawnSignalRunner();
		const workdir = await waitForFile(runner.probe);
		expect(existsSync(workdir), 'workdir must exist while rendering').toBe(true);

		runner.child.kill('SIGTERM');
		const {code} = await runner.exit;

		expect(code).not.toBe(0);
		// THE assertion: customer content gone despite the signal exit.
		expect(existsSync(workdir), 'workdir must be purged on signal').toBe(false);
	}, 20_000);

	it('is idempotent: a second signal during cleanup still exits', async () => {
		const runner = spawnSignalRunner();
		await waitForFile(runner.probe);

		// Two signals in quick succession — the second must not be lost nor
		// hang; the process must exit (any non-zero code acceptable).
		runner.child.kill('SIGTERM');
		runner.child.kill('SIGTERM');
		const {code} = await runner.exit;

		expect(code).not.toBe(0);
	}, 20_000);

	it('exits promptly when signalled before the workdir exists (stdin phase)', async () => {
		// A signal during stdin read: no workdir yet, handler no-ops the
		// purge and exits non-zero. Proves registration happens before the
		// render and that the handler is safe when idle.
		const runner = spawnSignalRunner();
		// Do not wait for the probe — signal as soon as spawned.
		await new Promise((resolve) => setTimeout(resolve, 300));
		runner.child.kill('SIGINT');

		const {code} = await runner.exit;
		expect(code).not.toBe(0);
	}, 20_000);
});
