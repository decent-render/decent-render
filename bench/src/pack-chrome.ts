/**
 * Packs a browser into the bench "payload" the way publish-runner-payload.ts
 * will: copy the Chrome tree next to the runner binary and write
 * `chrome/executable`, a manifest holding the browser path RELATIVE to the
 * payload root. The publish step knows what it downloaded, so the runner never
 * guesses a platform-specific nested layout.
 *
 * Seed it once with:  bun src/bench.ts … --save-chrome
 */
import {existsSync, readdirSync, rmSync, statSync, writeFileSync} from 'node:fs';
import path from 'node:path';
import {spawnSync} from 'node:child_process';

const root = path.resolve(import.meta.dir, '..');
const benchDir = path.join(root, '.bench');
const seed = path.join(benchDir, 'chrome-seed');
const dest = path.join(benchDir, 'chrome');

if (!existsSync(seed)) throw new Error('no chrome seed — run: bun src/bench.ts --frames=10 --save-chrome');

rmSync(dest, {recursive: true, force: true});
// APFS clone where available, plain copy otherwise.
if (spawnSync('cp', ['-Rc', seed, dest]).status !== 0) spawnSync('cp', ['-R', seed, dest]);

/** Locate the browser executable inside an extracted Chrome tree. */
function findBrowser(dir: string): string | null {
	const candidates: string[] = [];
	const walk = (d: string, depth: number) => {
		if (depth > 8) return;
		for (const entry of readdirSync(d)) {
			const full = path.join(d, entry);
			let s;
			try {
				s = statSync(full);
			} catch {
				continue;
			}
			if (s.isDirectory()) {
				walk(full, depth + 1);
			} else if (
				// macOS: "Google Chrome for Testing" in .app/Contents/MacOS
				// Linux:  "chrome" / "chrome-headless-shell" at the tree root
				(entry === 'Google Chrome for Testing' && full.includes('Contents/MacOS')) ||
				entry === 'chrome' ||
				entry === 'chrome-headless-shell'
			) {
				if (s.mode & 0o111) candidates.push(full);
			}
		}
	};
	walk(dir, 0);
	// Prefer the shallowest match — helper binaries sit deeper.
	candidates.sort((a, b) => a.split(path.sep).length - b.split(path.sep).length);
	return candidates[0] ?? null;
}

const browser = findBrowser(dest);
if (!browser) throw new Error(`no browser executable found under ${dest}`);

const relative = path.relative(benchDir, browser);
writeFileSync(path.join(dest, 'executable'), `${relative}\n`);
console.error(`packed chrome → ${dest}`);
console.error(`manifest: ${relative}`);
