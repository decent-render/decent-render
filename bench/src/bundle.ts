/**
 * Bundles the golden benchmark compositions and packs them exactly like a
 * tenant bundle: a tar.gz whose sha256 is the content address the runner
 * verifies before extracting.
 */
import {bundle} from '@remotion/bundler';
import {createHash} from 'node:crypto';
import {mkdirSync, readFileSync, rmSync, writeFileSync} from 'node:fs';
import path from 'node:path';
import {spawnSync} from 'node:child_process';

const root = path.resolve(import.meta.dir, '..');
const outDir = path.join(root, '.bench');
mkdirSync(outDir, {recursive: true});

const serveDir = path.join(outDir, 'bundle');
rmSync(serveDir, {recursive: true, force: true});

console.error('bundling benchmark compositions…');
const bundled = await bundle({
	entryPoint: path.join(root, 'src/compositions/index.ts'),
	outDir: serveDir,
	onProgress: (p) => {
		if (p % 25 === 0) console.error(`  webpack ${p}%`);
	},
});

const archive = path.join(outDir, 'bundle.tar.gz');
rmSync(archive, {force: true});
const tar = spawnSync('tar', ['-czf', archive, '-C', bundled, '.'], {stdio: 'inherit'});
if (tar.status !== 0) throw new Error(`tar failed with ${tar.status}`);

const bytes = readFileSync(archive);
const sha256 = createHash('sha256').update(bytes).digest('hex');
writeFileSync(path.join(outDir, 'bundle.json'), JSON.stringify({sha256, sizeBytes: bytes.byteLength}, null, 2));

console.error(`bundle ${sha256.slice(0, 12)} — ${(bytes.byteLength / 1024 / 1024).toFixed(1)}MB → ${archive}`);
