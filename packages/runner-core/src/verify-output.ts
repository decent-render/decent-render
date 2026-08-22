/**
 * Post-render, pre-upload verification of the rendered file.
 *
 * The failure this exists for is HONEST HARDWARE FAILURE on a trusted node —
 * a dead GPU or a broken ANGLE path silently emitting black frames — not a
 * lying operator. Every node is owned by us, so there is no adversary to
 * out-sample and nothing to make unpredictable; the checks run on every job,
 * in the open, and are as cheap as they can be.
 *
 * Before this existed, `renderJob` read the output and PUT it straight to R2,
 * and reported `frames` as `composition.durationInFrames` — a number copied
 * off the composition, never measured against the file. A truncated render,
 * a zero-byte file, or 300 uniformly black frames all uploaded clean and
 * reported success with a full frame count.
 *
 * ffprobe/ffmpeg come from the render payload's own `remotion-binaries/`
 * (the same directory Remotion itself uses), so this adds no dependency and
 * no assumption about what is installed on the operator's machine.
 */
import {spawnSync} from 'node:child_process';
import {existsSync, statSync} from 'node:fs';
import path from 'node:path';

/** What the file actually turned out to contain. */
export type OutputProbe = {
  codec: string;
  width: number;
  height: number;
  /** MEASURED frame count — never the composition's claim. */
  frames: number;
};

/**
 * How Remotion's shipped binaries find their own shared libraries.
 *
 * macOS: the dylibs carry BARE install names and the executables have no
 * `LC_RPATH`, so dyld's only route to them is its fallback of searching the
 * current working directory. The runner renders from a per-job mkdtemp
 * workdir, so spawning from there dies with "Library not loaded:
 * libavdevice.dylib" — which this module would otherwise have reported as
 * "output does not decode", failing every good render on every node.
 *
 * Linux: every shipped variant carries `RPATH $ORIGIN` (old-style transitive
 * `DT_RPATH`, so the inter-libav dependencies resolve too), which makes the
 * libraries resolve relative to the BINARY and needs neither of these.
 *
 * We set both anyway — harmless on Linux, and `cwd` + `*_LIBRARY_PATH` is
 * exactly what Remotion's own `callFf()` does, so this stays aligned with
 * upstream rather than depending on a dyld fallback.
 *
 * Verified 2026-08-21: darwin-arm64 by running the shipped payload here;
 * linux-{x64,arm64}-{gnu,musl} at the pinned 4.0.506 by executing the real
 * binaries in Docker (ELF headers + `env -i`, cwd=/).
 */
function ffEnv(binariesDirectory: string): NodeJS.ProcessEnv {
  const prepend = (existing: string | undefined) =>
    existing === undefined || existing === '' ? binariesDirectory : `${binariesDirectory}:${existing}`;
  return {
    ...process.env,
    DYLD_LIBRARY_PATH: prepend(process.env.DYLD_LIBRARY_PATH),
    LD_LIBRARY_PATH: prepend(process.env.LD_LIBRARY_PATH),
  };
}

function runFf(binary: string, args: string[], binariesDirectory: string) {
  return spawnSync(binary, args, {cwd: binariesDirectory, env: ffEnv(binariesDirectory), encoding: 'utf8'});
}

export type VerifyOptions = {
  outputLocation: string;
  /** `composition.durationInFrames` — the claim we are checking against. */
  expectedFrames: number;
  expectedWidth?: number;
  expectedHeight?: number;
  /** The payload's `remotion-binaries/`; null when the payload ships none. */
  binariesDirectory: string | null;
  log?: (message: string) => unknown;
};

/**
 * Luma at or below this counts as black. Not zero: h264 is lossy, so a
 * genuinely black frame quantises to small non-zero values rather than to
 * exact 0.
 */
const BLACK_MAX_LUMA = 6;

/**
 * Frames sampled for content. First, middle and last is enough to catch a
 * dead GPU — decoding every frame of a long render is not a cost worth
 * paying on every job for a fault that is never subtle.
 */
function sampleIndices(frames: number): number[] {
  if (frames <= 1) return [0];
  if (frames === 2) return [0, 1];
  return [0, Math.floor(frames / 2), frames - 1];
}

/**
 * Smallest believable size per frame. Deliberately absurd (a real 640x360
 * h264 frame is orders of magnitude larger) — this catches the "valid
 * container, no actual video" case, not marginal compression.
 */
const MIN_BYTES_PER_FRAME = 8;

/**
 * Largest believable bytes per CLAIMED output pixel, h264 or vp8.
 *
 * The ceiling is derived from what the job says it rendered — its frame
 * count and dimensions — because a flat cap cannot be both safe for a
 * long 4K render and tight for a 30-frame preview: one number that fails
 * a legitimate render is worse than no cap at all. Real h264 at CRF lands
 * around 0.02–0.05 bytes/pixel even for hard content, and vp8 is roughly
 * twice that; 0.1 covers both codecs with ≥2× headroom. Only a runaway —
 * an encode emitting garbage frames forever, a pathological image
 * sequence — crosses it.
 */
const MAX_BYTES_PER_CLAIMED_PIXEL = 0.1;

/**
 * Flat floor for the derived ceiling: no render this farm serves produces
 * more, whatever it claims (the E2E fixture is ~48KB; heaviest real
 * compositions are minutes of 1080p ≈ 1GB), and it bounds the damage a
 * render with no dimension claim can do to node memory at read time.
 */
const MIN_OUTPUT_CEILING_BYTES = 2 * 1024 ** 3;

/**
 * The largest output we will read and upload for these composition claims.
 * Generous on purpose: tripping on a legitimate render is a worse failure
 * than the unbounded read this exists to prevent.
 */
export function maxOutputBytes(expectedFrames: number, expectedWidth?: number, expectedHeight?: number): number {
	if (!Number.isFinite(expectedFrames) || expectedFrames <= 0) return MIN_OUTPUT_CEILING_BYTES;
	if (!Number.isFinite(expectedWidth) || !Number.isFinite(expectedHeight) || expectedWidth! <= 0 || expectedHeight! <= 0) {
		return MIN_OUTPUT_CEILING_BYTES;
	}
	const derived = Math.ceil(expectedFrames * expectedWidth! * expectedHeight! * MAX_BYTES_PER_CLAIMED_PIXEL);
	return Math.max(MIN_OUTPUT_CEILING_BYTES, derived);
}

function findBinary(name: string, binariesDirectory: string | null): string | null {
  if (binariesDirectory === null) return null;
  const candidate = path.join(binariesDirectory, name);
  return existsSync(candidate) ? candidate : null;
}

function probeOutput(ffprobe: string, file: string, binariesDirectory: string): OutputProbe & {fps: number} {
  const run = (extra: string[]) =>
    runFf(
      ffprobe,
      [
        '-v', 'error',
        '-select_streams', 'v:0',
        '-show_entries', 'stream=codec_name,width,height,nb_frames,nb_read_frames,r_frame_rate',
        '-of', 'json',
        ...extra,
        file,
      ],
      binariesDirectory,
    );

  let result = run([]);
  if (result.status !== 0) {
    throw new Error(`rendered output does not decode — ffprobe failed: ${(result.stderr || '').trim() || `exit ${result.status}`}`);
  }
  let stream = (JSON.parse(result.stdout || '{}').streams ?? [])[0];
  if (stream === undefined) {
    throw new Error('rendered output contains no video stream');
  }

  // `nb_frames` is absent for some muxers. Fall back to actually counting,
  // which is the expensive path — say so rather than silently paying it.
  let frames = Number(stream.nb_frames);
  if (!Number.isFinite(frames) || frames <= 0) {
    result = run(['-count_frames']);
    if (result.status !== 0) {
      throw new Error(`rendered output does not decode — ffprobe frame count failed: ${(result.stderr || '').trim()}`);
    }
    stream = (JSON.parse(result.stdout || '{}').streams ?? [])[0] ?? stream;
    frames = Number(stream.nb_read_frames);
  }
  if (!Number.isFinite(frames) || frames <= 0) {
    throw new Error('rendered output has no decodable frames');
  }

  // r_frame_rate arrives as "30/1". Used only to turn a frame index into a
  // seek timestamp; a wrong-but-positive fps costs sample accuracy, never
  // correctness, so fall back rather than fail.
  const [num, den] = String(stream.r_frame_rate ?? '').split('/');
  const parsed = Number(num) / (Number(den) || 1);
  const fps = Number.isFinite(parsed) && parsed > 0 ? parsed : 30;

  return {
    codec: String(stream.codec_name ?? 'unknown'),
    width: Number(stream.width),
    height: Number(stream.height),
    frames,
    fps,
  };
}

const SAMPLE_WIDTH = 64;
const SAMPLE_HEIGHT = 36;

/**
 * Decode the sampled frames to tiny greyscale rasters. Downscaled because the
 * question is only "is there any picture here at all", which survives any
 * amount of scaling, and 2.3KB per frame keeps this free.
 *
 * One seek per sample rather than a `select` filter: Remotion ships a
 * STRIPPED ffmpeg, configured with `--disable-filters/--disable-muxers/
 * --disable-encoders` plus explicit allowlists. `scale` and `format` are
 * present, `select` is NOT, and `rawvideo` exists as an encoder but not as a
 * muxer — so a select-based sampler works on a developer's homebrew ffmpeg
 * and fails on every production node. Seeking also decodes only around each
 * sample instead of walking the whole file.
 *
 * The allowlist is IDENTICAL on darwin-arm64 and on all four Linux variants
 * (x64/arm64 × gnu/musl) at the pinned 4.0.506 — verified 2026-08-21 by
 * running the real binaries, not by reading about them. Everything this
 * sampler depends on (`scale`, `format`, the `image2pipe` muxer, the
 * `rawvideo` encoder) is present on every one of them.
 */
function sampleFrames(
  ffmpeg: string,
  file: string,
  indices: number[],
  fps: number,
  binariesDirectory: string,
): Uint8Array[] {
  const frameSize = SAMPLE_WIDTH * SAMPLE_HEIGHT;
  const out: Uint8Array[] = [];
  for (const index of indices) {
    const result = spawnSync(
      ffmpeg,
      [
        '-v', 'error',
        '-ss', String(index / fps),
        '-i', file,
        '-frames:v', '1',
        '-vf', `scale=${SAMPLE_WIDTH}:${SAMPLE_HEIGHT},format=gray`,
        // image2pipe + the rawvideo CODEC, not the rawvideo MUXER: Remotion's
        // stripped build ships the encoder but not the muxer, so `-f rawvideo`
        // dies with "Requested output format 'rawvideo' is not known" on every
        // production node while working fine on a homebrew ffmpeg.
        '-f', 'image2pipe',
        '-c:v', 'rawvideo',
        '-',
      ],
      {cwd: binariesDirectory, env: ffEnv(binariesDirectory), encoding: 'buffer', maxBuffer: 16 * 1024 * 1024},
    );
    if (result.status !== 0) {
      const stderr = Buffer.isBuffer(result.stderr) ? result.stderr.toString('utf8') : '';
      throw new Error(`rendered output does not decode — frame sampling failed: ${stderr.trim() || `exit ${result.status}`}`);
    }
    const raw = result.stdout as unknown as Buffer;
    // A seek past the last decodable frame yields nothing. That is a sampling
    // miss, not a corrupt file — skip it; the caller requires at least one.
    if (raw.length >= frameSize) out.push(new Uint8Array(raw.subarray(0, frameSize)));
  }
  return out;
}

function maxLuma(frame: Uint8Array): number {
  let max = 0;
  for (const value of frame) if (value > max) max = value;
  return max;
}

function identical(a: Uint8Array, b: Uint8Array): boolean {
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i += 1) if (a[i] !== b[i]) return false;
  return true;
}

/**
 * Verify the rendered file. Throws on anything that means we should not
 * upload; returns what the file MEASURABLY contains so the caller can report
 * measured numbers instead of the composition's claims.
 */
export function verifyRenderedOutput(options: VerifyOptions): OutputProbe {
  const {outputLocation, expectedFrames, expectedWidth, expectedHeight, binariesDirectory} = options;
  const log = options.log ?? (() => {});

  if (!existsSync(outputLocation)) {
    throw new Error('render reported success but produced no output file');
  }
  const sizeInBytes = statSync(outputLocation).size;
  if (sizeInBytes === 0) {
    throw new Error('render produced a zero-byte output file');
  }
  // The size ceiling rides the SAME stat, before any read or decode: the
  // caller reads the file into memory right after this returns, so a cap
  // that ran later would bound nothing. Derived from the composition's own
  // claims (see MAX_BYTES_PER_CLAIMED_PIXEL) so a legitimate long render
  // cannot trip it; a flat cap tight enough to catch a runaway would also
  // fail real 4K work.
  const ceiling = maxOutputBytes(expectedFrames, expectedWidth, expectedHeight);
  if (sizeInBytes > ceiling) {
    throw new Error(
      `rendered output is ${sizeInBytes} bytes, exceeding the ${ceiling} byte cap for ${expectedFrames} claimed frames — refusing the read/upload to protect node memory`,
    );
  }

  const ffprobe = findBinary('ffprobe', binariesDirectory);
  const ffmpeg = findBinary('ffmpeg', binariesDirectory);
  if (binariesDirectory === null || ffprobe === null || ffmpeg === null) {
    // Never silent. The structural checks above already ran; the content
    // checks cannot, and an operator reading logs must be able to see that
    // this job went out unverified. Refusing to upload an otherwise good
    // render because a diagnostic tool is missing would be a worse failure
    // than the one being guarded against.
    log(
      `WARNING: no ffprobe/ffmpeg in the render payload (binariesDirectory=${binariesDirectory ?? 'null'}) — uploading WITHOUT content verification. Black-frame and frame-count checks did NOT run.`,
    );
    return {codec: 'unverified', width: expectedWidth ?? 0, height: expectedHeight ?? 0, frames: expectedFrames};
  }

  const {fps, ...probe} = probeOutput(ffprobe, outputLocation, binariesDirectory);

  if (sizeInBytes < probe.frames * MIN_BYTES_PER_FRAME) {
    throw new Error(`rendered output is implausibly small: ${sizeInBytes} bytes for ${probe.frames} frames`);
  }
  if (probe.frames !== expectedFrames) {
    throw new Error(`rendered output has ${probe.frames} frames, composition declares ${expectedFrames}`);
  }
  if (expectedWidth !== undefined && expectedHeight !== undefined) {
    if (probe.width !== expectedWidth || probe.height !== expectedHeight) {
      throw new Error(`rendered output is ${probe.width}x${probe.height}, composition declares ${expectedWidth}x${expectedHeight}`);
    }
  } else {
    log('composition exposed no width/height — dimension check skipped');
  }

  const indices = sampleIndices(probe.frames);
  const frames = sampleFrames(ffmpeg, outputLocation, indices, fps, binariesDirectory);
  if (frames.length === 0) {
    throw new Error('rendered output yielded no decodable frames when sampled');
  }

  if (frames.every((frame) => maxLuma(frame) <= BLACK_MAX_LUMA)) {
    throw new Error(
      `rendered output is uniformly black across all ${frames.length} sampled frames (of ${probe.frames}) — the usual cause is a dead GPU or a broken ANGLE path on this node`,
    );
  }

  // NOT a failure: a static composition (a held title card, a still frame)
  // legitimately renders identical frames, and rejecting those would break
  // real work to catch a fault the black-frame check already covers.
  if (frames.length > 1 && frames.every((frame) => identical(frame, frames[0]))) {
    log(`WARNING: all ${frames.length} sampled frames are identical — expected only if this composition is static`);
  }

  log(`output verified: ${probe.frames} frames, ${probe.width}x${probe.height}, ${probe.codec}, ${sizeInBytes} bytes`);
  return probe;
}
