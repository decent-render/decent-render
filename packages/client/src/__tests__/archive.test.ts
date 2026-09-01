import {describe, expect, it} from 'vitest';
import {spawnSync} from 'node:child_process';
import {mkdtemp, mkdir, writeFile} from 'node:fs/promises';
import {gunzipSync} from 'node:zlib';
import {tmpdir} from 'node:os';
import path from 'node:path';

import {createTarGzip} from '../archive.js';

/**
 * Direct test of the hand-rolled ustar writer (C-9). The oracle is the
 * SYSTEM tar: if `tar -tf` can list what we wrote, the headers are real
 * ustar, not merely self-consistent.
 */
async function fixtureDir() {
  const dir = await mkdtemp(path.join(tmpdir(), 'decent-archive-test-'));
  await writeFile(path.join(dir, 'index.html'), '<html>render</html>');
  await mkdir(path.join(dir, 'nested', 'deep'), {recursive: true});
  await writeFile(path.join(dir, 'nested', 'deep', 'app.js'), 'console.log("render")');
  await mkdir(path.join(dir, 'empty'));
  return dir;
}

/** A path whose full name exceeds ustar's 100-byte name field. */
const LONG_DIR = `assets/${'d'.repeat(70)}`;
const LONG_BASENAME = `${'f'.repeat(60)}.js`;
const LONG_PATH = `${LONG_DIR}/${LONG_BASENAME}`;

function systemTarListing(archive: Buffer): string[] {
  const listing = spawnSync('tar', ['-tzf', '-'], {input: archive, encoding: 'utf8'});
  if (listing.status !== 0) throw new Error(`tar -tzf failed: ${listing.stderr}`);
  return listing.stdout.split('\n').filter(Boolean);
}

describe('createTarGzip', () => {
  it('writes a gzip stream the system tar can list, with nested files and empty directories present', async () => {
    const dir = await fixtureDir();
    const archive = await createTarGzip(dir);
    expect([archive[0], archive[1]]).toEqual([0x1f, 0x8b]);
    const names = systemTarListing(archive);
    expect(names).toContain('index.html');
    expect(names).toContain('nested/deep/app.js');
    // An empty directory is part of the bundle's shape; dropping it silently
    // makes the archive lossy.
    expect(names).toContain('empty/');
  });

  it('splits a >100-byte path into ustar prefix + name and the system tar reassembles it', async () => {
    const dir = await fixtureDir();
    await mkdir(path.join(dir, LONG_DIR), {recursive: true});
    await writeFile(path.join(dir, LONG_PATH), 'long');
    expect(Buffer.byteLength(LONG_PATH)).toBeGreaterThan(100);

    const archive = await createTarGzip(dir);
    const names = systemTarListing(archive);
    expect(names).toContain(LONG_PATH);

    // Header-level proof of the split: the raw ustar header for that entry
    // carries the basename in `name` (offset 0) and the directory in
    // `prefix` (offset 345) — not a truncated name.
    const tar = gunzipSync(archive);
    let found = false;
    for (let offset = 0; offset + 512 <= tar.byteLength; offset += 512) {
      const name = tar.toString('utf8', offset, offset + 100).replace(/\0.*$/, '');
      const prefix = tar.toString('utf8', offset + 345, offset + 500).replace(/\0.*$/, '');
      if (name === LONG_BASENAME && prefix === LONG_DIR) {
        found = true;
        break;
      }
    }
    expect(found).toBe(true);
  });

  it('refuses a path that cannot be split into the ustar fields', async () => {
    const dir = await fixtureDir();
    const tooLong = `${'g'.repeat(120)}.js`; // basename alone >100, no slash to split on
    await writeFile(path.join(dir, tooLong), 'x');
    await expect(createTarGzip(dir)).rejects.toThrow(/too long for tar/);
  });

  it('is deterministic for the same tree (content-addressing depends on it)', async () => {
    const dir = await fixtureDir();
    const a = await createTarGzip(dir);
    const b = await createTarGzip(dir);
    expect(Buffer.compare(a, b)).toBe(0);
  });
});
