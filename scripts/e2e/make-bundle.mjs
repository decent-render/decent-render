import {bundle} from '@remotion/bundler';
import {createHash} from 'node:crypto';
import {readFileSync, writeFileSync, mkdtempSync} from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import {spawnSync} from 'node:child_process';
import {fileURLToPath} from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const outDir = process.env.E2E_ARTIFACTS ?? '/tmp/p3-artifacts';

const out = await bundle({
	entryPoint: path.join(here, 'bundle-src/index.ts'),
	webpackOverride: (c) => c,
});
console.error('bundled to', out);
const staging = mkdtempSync(path.join(os.tmpdir(), 'decent-e2e-bundle-'));
const archive = path.join(staging, 'bundle.tar.gz');
const r = spawnSync('tar', ['-czf', archive, '-C', out, '.']);
if (r.status !== 0) throw new Error('tar failed: ' + r.stderr);
writeFileSync(path.join(outDir, 'bundle.tar.gz'), readFileSync(archive));
const bytes = readFileSync(path.join(outDir, 'bundle.tar.gz'));
console.log('bundle.tar.gz', bytes.length, 'bytes sha256=' + createHash('sha256').update(bytes).digest('hex'));
