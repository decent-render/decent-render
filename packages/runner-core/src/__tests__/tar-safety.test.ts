/**
 * PACKET 39 — tar extraction safety regression pins (TS path).
 *
 * These pin MEASURED behaviour, not assumed safety. On 2026-08-26 both
 * production-relevant tars were fed the malicious archives below:
 *
 *   bsdtar 3.5.3 (macOS, what operators actually run):
 *     ../ member            → REFUSED ("Path contains '..'", exit 1)
 *     absolute path         → leading '/' stripped, written INSIDE dest
 *     symlink write-through → REFUSED ("Cannot extract through symlink", exit 1)
 *     symlink replace       → symlink entry replaced by the regular file,
 *                             outside target UNTOUCHED (exit 0)
 *
 *   GNU tar 1.34 (ubuntu:22.04 — the CI image):
 *     ../ member            → REFUSED ("Member name contains '..'", exit 2)
 *     absolute path         → leading '/' stripped, written INSIDE dest
 *     symlink write-through → REFUSED ("Cannot open: Not a directory", exit 2)
 *     symlink replace       → symlink REMAINS pointing outside (the tars
 *                             differ here) but nothing was written through;
 *                             the outside target is UNTOUCHED (exit 0)
 *
 * Both production paths extract into a FRESH empty directory per archive
 * (never reused), so the residual-symlink shape cannot be triggered by a
 * second extraction. If a future tar version or flag change weakens any
 * of these, these tests go RED and the hole is caught in CI — which now
 * installs ffmpeg AND runs on ubuntu's GNU tar, complementing the bsdtar
 * a developer machine runs.
 */

import {afterAll, beforeAll, describe, expect, it} from 'vitest';
import {spawnSync} from 'node:child_process';
import {existsSync, mkdtempSync, readFileSync, rmSync, statSync, writeFileSync} from 'node:fs';
import {tmpdir} from 'node:os';
import path from 'node:path';
import {createGzip} from 'node:zlib';
import {PassThrough} from 'node:stream';

let lab: string;
const p = (name: string) => path.join(lab, name);

/** Build a .tar.gz with fully controlled member entries. */
function buildArchive(name: string, entries: Array<{name: string; data?: Buffer; type?: 'file' | 'symlink'; linkname?: string}>): string {
  // ustar by hand — exact control over member names, no library normalizing them.
  const chunks: Buffer[] = [];
  const header = (name: string, size: number, type: string, linkname = '') => {
    const h = Buffer.alloc(512);
    h.write(name.slice(0, 100), 0, 'utf8'); // name
    h.write('0000644\0', 100, 'ascii'); // mode
    h.write('0000000\0', 108, 'ascii'); // uid
    h.write('0000000\0', 116, 'ascii'); // gid
    h.write(size.toString(8).padStart(11, '0') + '\0', 124, 'ascii'); // size
    h.write('00000000000\0', 136, 'ascii'); // mtime
    h.write('        ', 148, 'ascii'); // checksum placeholder
    h.write(type, 156, 'ascii'); // typeflag
    h.write(linkname.slice(0, 100), 157, 'utf8');
    h.write('ustar\0', 257, 'ascii');
    h.write('00', 263, 'ascii');
    let sum = 0;
    for (const b of h) sum += b;
    h.write(sum.toString(8).padStart(6, '0') + '\0 ', 148, 'ascii');
    return h;
  };
  for (const e of entries) {
    const data = e.data ?? Buffer.from('escaped-content-p39');
    if (e.type === 'symlink') {
      chunks.push(header(e.name, 0, '2', e.linkname ?? 'target'));
    } else {
      chunks.push(header(e.name, data.length, '0'));
      chunks.push(data);
      const pad = (512 - (data.length % 512)) % 512;
      if (pad) chunks.push(Buffer.alloc(pad));
    }
  }
  chunks.push(Buffer.alloc(1024)); // end-of-archive
  const tar = Buffer.concat(chunks);
  // gzip via zlib (deterministic enough for a fixture)
  const gz = new PassThrough();
  // simple gzip with default settings using zlib.gzipSync instead:
  // eslint-disable-next-line @typescript-eslint/no-require-imports
  const {gzipSync} = require('node:zlib') as typeof import('node:zlib');
  void gz; void createGzip;
  const archive = path.join(lab, name);
  writeFileSync(archive, gzipSync(tar));
  return archive;
}

/** The EXACT production extraction invocation (render-job.ts shape). */
function extract(archive: string, dest: string) {
  return spawnSync('tar', ['-xzf', archive, '-C', dest], {encoding: 'utf8'});
}

beforeAll(() => {
  lab = mkdtempSync(path.join(tmpdir(), 'tar-safety-'));
  // An outside canary: if ANY class writes outside dest, it can only
  // plausibly target these paths.
  writeFileSync(p('outside-canary.txt'), 'ORIGINAL');
});

afterAll(() => {
  if (lab) rmSync(lab, {recursive: true, force: true});
});

describe('tar traversal containment (measured, pinned)', () => {
  it('REFUSES a ../ member — nothing written outside the destination', () => {
    const archive = buildArchive('escape.tar.gz', [{name: '../escape-p39.txt'}]);
    const dest = mkdtempSync(path.join(lab, 'dest-escape-'));
    const res = extract(archive, dest);
    // Both tars FAIL the extraction for .. members.
    expect(res.status).not.toBe(0);
    // And nothing landed outside the destination.
    expect(existsSync(p('escape-p39.txt'))).toBe(false);
    expect(existsSync(path.join(lab, '..', 'escape-p39.txt'))).toBe(false);
  });

  it('NEUTRALIZES an absolute-path member — written INSIDE dest, not at /', () => {
    const archive = buildArchive('absolute.tar.gz', [{name: '/tmp/p39-absolute-escape.txt'}]);
    const dest = mkdtempSync(path.join(lab, 'dest-abs-'));
    const res = extract(archive, dest);
    expect(res.status).toBe(0);
    // Both tars strip the leading slash: the member exists UNDER dest...
    expect(existsSync(path.join(dest, 'tmp', 'p39-absolute-escape.txt'))).toBe(true);
    // ...and NOT at the absolute location.
    expect(existsSync('/tmp/p39-absolute-escape.txt')).toBe(false);
  });

  it('REFUSES writing THROUGH a symlink that points outside', () => {
    const archive = buildArchive('through.tar.gz', [
      {name: 'evil-link', type: 'symlink', linkname: '../through-victim.txt'},
      {name: 'evil-link/inner.txt', data: Buffer.from('THROUGH THE LINK')},
    ]);
    const dest = mkdtempSync(path.join(lab, 'dest-through-'));
    const res = extract(archive, dest);
    // bsdtar: "Cannot extract through symlink" exit 1; GNU: exit 2. Both FAIL.
    expect(res.status).not.toBe(0);
    // The outside target was never created.
    expect(existsSync(p('through-victim.txt'))).toBe(false);
  });

  it('leaves an OUTSIDE target UNTOUCHED when a member name collides with a symlink', () => {
    // GNU tar leaves the symlink in place; bsdtar replaces it with the file.
    // NEITHER writes through: the outside target keeps its original bytes.
    const canary = p('outside-canary.txt');
    const archive = buildArchive('collide.tar.gz', [
      {name: 'collide', type: 'symlink', linkname: canary},
      {name: 'collide', data: Buffer.from('CLOBBERED')},
    ]);
    const dest = mkdtempSync(path.join(lab, 'dest-collide-'));
    const res = extract(archive, dest);
    expect(res.status).toBe(0);
    // The canary survives byte-for-byte on BOTH tars (measured).
    expect(readFileSync(canary, 'utf8')).toBe('ORIGINAL');
    // And the dest has an entry (file on bsdtar, symlink on GNU) —
    // either way, containment held.
    expect(existsSync(path.join(dest, 'collide'))).toBe(true);
  });

  it('the outside canary is still intact at the end of the whole suite', () => {
    // Belt-and-braces: no earlier test leaked a write outside its dest.
    expect(readFileSync(p('outside-canary.txt'), 'utf8')).toBe('ORIGINAL');
  });
});

describe('MAX_EXTRACTED_BUNDLE_BYTES (2c)', () => {
  it('exports the packet-37-matching 4 GiB ceiling', async () => {
    const {MAX_EXTRACTED_BUNDLE_BYTES} = await import('../render-job.js');
    expect(MAX_EXTRACTED_BUNDLE_BYTES).toBe(4 * 1024 * 1024 * 1024);
  });
});
