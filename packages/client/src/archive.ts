import {readdir, readFile, stat} from 'node:fs/promises';
import path from 'node:path';
import {gzipSync} from 'node:zlib';

type Entry = {name: string; bytes: Buffer; mode: number};

function writeText(target: Buffer, offset: number, length: number, value: string): void {
  target.write(value.slice(0, length), offset, length, 'utf8');
}

function writeOctal(target: Buffer, offset: number, length: number, value: number): void {
  const encoded = Math.max(0, value).toString(8).padStart(length - 1, '0').slice(-(length - 1));
  writeText(target, offset, length, `${encoded}\0`);
}

function splitTarPath(name: string): {name: string; prefix: string} {
  if (Buffer.byteLength(name) <= 100) return {name, prefix: ''};
  const slash = name.lastIndexOf('/');
  if (slash <= 0) throw new Error(`Bundle path is too long for tar: ${name}`);
  const prefix = name.slice(0, slash);
  const basename = name.slice(slash + 1);
  if (Buffer.byteLength(prefix) > 155 || Buffer.byteLength(basename) > 100) {
    throw new Error(`Bundle path is too long for tar: ${name}`);
  }
  return {name: basename, prefix};
}

function tarHeader(entry: Entry): Buffer {
  const header = Buffer.alloc(512);
  const names = splitTarPath(entry.name);
  writeText(header, 0, 100, names.name);
  writeOctal(header, 100, 8, entry.mode & 0o777);
  writeOctal(header, 108, 8, 0);
  writeOctal(header, 116, 8, 0);
  writeOctal(header, 124, 12, entry.bytes.byteLength);
  writeOctal(header, 136, 12, 0);
  header.fill(0x20, 148, 156);
  header[156] = '0'.charCodeAt(0);
  writeText(header, 257, 6, 'ustar');
  writeText(header, 263, 2, '00');
  writeText(header, 345, 155, names.prefix);
  const checksum = header.reduce((sum, byte) => sum + byte, 0);
  writeOctal(header, 148, 8, checksum);
  return header;
}

async function collect(root: string, relative = ''): Promise<Entry[]> {
  const current = path.join(root, relative);
  const names = (await readdir(current)).sort();
  const entries: Entry[] = [];
  for (const name of names) {
    const childRelative = relative ? path.posix.join(relative, name) : name;
    const child = path.join(root, childRelative);
    const info = await stat(child);
    if (info.isDirectory()) entries.push(...await collect(root, childRelative));
    else if (info.isFile()) entries.push({name: childRelative, bytes: await readFile(child), mode: info.mode});
  }
  return entries;
}

export async function createTarGzip(root: string): Promise<Buffer> {
  const chunks: Buffer[] = [];
  for (const entry of await collect(root)) {
    chunks.push(tarHeader(entry), entry.bytes);
    const padding = (512 - (entry.bytes.byteLength % 512)) % 512;
    if (padding) chunks.push(Buffer.alloc(padding));
  }
  chunks.push(Buffer.alloc(1024));
  return gzipSync(Buffer.concat(chunks));
}
