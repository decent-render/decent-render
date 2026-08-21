# Remotion's shipped ffmpeg/ffprobe — what the node can rely on

> **Why this is in the repo.** `packages/runner-core/src/verify-output.ts` shells
> out to the ffmpeg/ffprobe inside the render payload to verify a render before
> uploading it (#12). Which filters, muxers and encoders those binaries actually
> have — and how they find their own shared libraries — is load-bearing for that
> code and is NOT obvious: the shipped ffmpeg is a stripped, allowlist-configured
> build, so anything developed against a homebrew ffmpeg passes locally and fails
> on every production node. Two such defects were caught exactly that way on
> 2026-08-21 before they shipped.
>
> Findings below are VERIFIED BY EXECUTION, not by reading docs. Re-verify on a
> Remotion version bump; the allowlist is a build-time decision upstream and can
> change without notice.
>
> Acted on in `2cc5940` (library path) and `5f8c823` (the verifier itself).

Original research packet: `/tmp/decent-render-research-remotion-binaries.md`
Date: 2026-08-21 (UTC evening). Remotion version under test: **4.0.506** (matches both
`~/dev/driffs` and `farm-web/apps/runner-4.0.506`).

**Method note:** all four Linux compositor variants (`linux-x64-gnu`, `linux-arm64-gnu`,
`linux-x64-musl`, `linux-arm64-musl`) were downloaded via `npm pack` into
`/tmp/remotion-binaries-research/` and **executed** in Docker (`debian:bookworm-slim` for gnu,
`alpine:3.21` for musl; x64 via `--platform linux/amd64`, arm64 native). The darwin-arm64
package from the runner's own `node_modules` was executed on this Mac. No repo was modified; no
project `node_modules` was touched; `~/.decent-worker` untouched.

---

## Executive summary

| # | Question | Answer | Confidence |
|---|----------|--------|------------|
| Q1 | Is the Linux ffmpeg stripped the same way? | **Yes — identical filter set to macOS; all your dependencies are present** | VERIFIED (executed) |
| Q2 | Supported API for locating/using the binaries? | **Yes: `binariesDirectory` (documented option) + `getExecutablePath()` / `callFf()` (exported, typed, but not on the docs site). `callFf` even implements your cwd fix itself** | VERIFIED (source + docs) |
| Q3 | Does Linux need the cwd trick? | **No. Linux builds carry `RPATH $ORIGIN`; dylibs resolve relative to the binary. The cwd trick remains harmless and is what Remotion itself does** | VERIFIED (ELF headers + executed) |

The two macOS findings do **not** reproduce as problems on Linux: the filter/muxer strip is the
same (a non-problem for you — your dependency set survives), and the dylib-loading cwd
dependence is a macOS-only packaging artifact (Linux uses `$ORIGIN` rpath).

---

## Q1 — Is the Linux ffmpeg stripped the same way? **[VERIFIED]**

### The binaries are the same architecture everywhere

Every compositor package ships: `ffmpeg` + `ffprobe` (tiny ~300 KB C *wrapper* executables),
`libav{codec,device,filter,format,util}.so|.dylib` + `libsw{resample,scale}.so|.dylib`
(the real code — stripped of symbols), and `remotion` (the Rust compositor). The same
ffmpeg build recipe is used on every platform; running `-version` prints the full configure
line, which is explicit allowlist-based stripping:

```
--disable-filters --enable-filter=aformat --enable-filter=atrim … --enable-filter=scale …
--disable-encoders --enable-encoder=opus … --enable-encoder=png --enable-encoder=mjpeg …
  --enable-encoder=rawvideo …
--disable-muxers --enable-muxer='webm,opus,mp4,wav,mp3,mov,matroska,hevc,h264,gif,image2,
  image2pipe,adts,m4a,mpegts,null,avi'
--disable-demuxers --enable-demuxer='…,image2,image2pipe,matroska,mov,mp3,…'
--disable-decoders --enable-decoder='…,h264,hevc,…,rawvideo'
```
(ffmpeg n7.1, full configure line captured from the arm64-gnu binary)

### Your exact dependency checklist

Executed `ffmpeg -filters / -muxers / -encoders` on **all four Linux variants**:

| Dependency | linux-x64-gnu | linux-arm64-gnu | linux-x64-musl | linux-arm64-musl | darwin-arm64 |
|---|---|---|---|---|---|
| `scale` filter | ✅ | ✅ | ✅ | ✅ | ✅ |
| `format` filter | ✅ | ✅ | ✅ | ✅ | ✅ |
| `select` filter | ❌ | ❌ | ❌ | ❌ | ❌ |
| `image2pipe` **muxer** | ✅ | ✅ | ✅ | ✅ | ✅ |
| `rawvideo` **encoder** | ✅ | ✅ | ✅ | ✅ | ✅ |
| `mjpeg` encoder | ✅ | ✅ | ✅ | ✅ | ✅ |
| `png` encoder | ✅ | ✅ | ✅ | ✅ | ✅ |

Filter sets are **identical across all five variants** (set-diff empty; 42 named filters:
`abuffer abuffersink acopy adelay aformat amerge amix anull anullsrc apad aresample asetpts
asetrate atempo atrim buffer buffersink colorspace concat copy crop fieldorder format hflip
loudnorm null nullsrc palettegen paletteuse pan rotate scale silencedetect sine split
tinterlace tonemap transpose trim vflip volume zscale`).

### End-to-end execution proof (arm64-gnu and x64-gnu, and x64-musl)

Inside the container I synthesized a 10-frame PNG stream, encoded `h264/mp4` with the shipped
ffmpeg (`image2pipe` demuxer + `libx264`), probed it with the shipped ffprobe, and ran the
runner's exact extraction pipeline:

```
$ /b/ffmpeg -i t.mp4 -vf scale=16:16 -f image2pipe -c:v rawvideo -pix_fmt rgb24 out.raw
extracted bytes: 7680   # expected 16*16*3*10 = 7680 ✓
```

### Failure-mode parity with macOS — confirmed identical error behavior

- `-f rawvideo` as **muxer**: `Requested output format 'rawvideo' is not known.` — identical
  message to macOS. VERIFIED on arm64-gnu.
- `-vf select=…`: filter absent (verified by `-filters`; a direct `-vf select` run collides
  with the also-absent `wrapped_avframe` null-output encoder before reaching the filter error,
  but the filter is definitively not registered).
- **New nuance beyond the packet:** `rawvideo` is also absent as a **demuxer**
  (`-f rawvideo -i …` → `Unknown input format: 'rawvideo'`), and `lavfi` is not compiled in at
  all (no `nullsrc`/`testsrc` input device). Neither affects your pipeline (renders are real
  mp4/webm files; you read via `image2pipe`), but any future probe tooling must not assume them.

### Per-platform differences that DO exist (encoders only — do not affect you)

Configure-line diffs between variants (all VERIFIED from `-version` output):

- **linux-x64-gnu only:** `--enable-encoder=h264_nvenc,hevc_nvenc,libaom_av1` + `--enable-libaom`
- **linux-arm64-gnu:** no hw encoders
- **darwin-arm64:** `h264_videotoolbox`, `hevc_videotoolbox`, `prores_videotoolbox` encoders
- Filters/muxers/decoders: identical everywhere.

**Q1 conclusion: your verifier (`scale`, `format`, `image2pipe` muxer, `rawvideo` encoder,
avoiding `select`) is safe on every Linux variant of 4.0.506 — and the fallbacks `mjpeg`/`png`
encoders are present too.**

---

## Q2 — Supported API for locating/using the binaries? **[VERIFIED]**

Three tiers exist, from most to least supported:

### 1. `binariesDirectory` — the documented contract (docs site + option registry)

Render APIs (`renderMedia`, `getVideoMetadata`, `extractAudio`, … since v4.0.120), the CLI
(`--binaries-directory`), and `Config.setBinariesDirectory()` all accept it. The option's own
description (read from `@remotion/renderer/dist/options/binaries-directory.js`, identical text on
https://www.remotion.dev/docs/renderer/extract-audio#binariesdirectory):

> "The directory where the platform-specific binaries and libraries that Remotion needs are
> located. Those include an `ffmpeg` and `ffprobe` binary, a Rust binary for various tasks, and
> various shared libraries. If the value is set to `null`, which is the default, then the path of
> a platform-specific package located at `node_modules/@remotion/compositor-*` is selected."

So the contract your runner relies on ("pass the compositor package dir; the files are named
`ffmpeg`/`ffprobe` (+ `.exe` on Windows) plus shared libs") is an intentional, documented
interface — not an accident you're leaning on.

### 2. `getExecutablePath()` — exported, typed, used by Remotion itself, but no docs page

```ts
import {getExecutablePath} from '@remotion/renderer';
const p = getExecutablePath({type: 'ffprobe', binariesDirectory, indent: false, logLevel: 'error'});
```
Source: `@remotion/renderer/dist/compositor/get-executable-path.js` — resolves
`binariesDirectory ?? @remotion/compositor-<platform><libc>.dir`, handles win32 `.exe` suffixes,
and picks **musl vs gnu automatically** via `process.report` (⚠️ under Bun it *assumes glibc*
and logs a warning — see operational notes). Re-exported from the package index
(`exports.getExecutablePath` in `dist/index.js`, typed in `dist/index.d.ts`).

### 3. `callFf()` — exported runner for the shipped binaries; **it implements your cwd fix**

```ts
import {callFf} from '@remotion/renderer';
const task = callFf({bin: 'ffprobe', args: ['-hide_banner', '-version'],
                     binariesDirectory, indent: false, logLevel: 'error'});
```
Source: `dist/call-ffmpeg.js` (verbatim):
```js
const cwd = path.dirname(executablePath);           // ← the cwd trick
const task = execa(executablePath, args, {
    cwd,
    env: getExplicitEnv(cwd),                       // ← DYLD_LIBRARY_PATH=cwd on macOS only
    ...options,
});
```
plus `makeFileExecutableIfItIsNot()`. This is precisely the decent-render fix — path
resolution + cwd + macOS `DYLD_LIBRARY_PATH` + chmod — maintained upstream. Note the comment in
`get-explicit-env.js` linking remotion-dev/remotion#3862 ("Should work out of the box, but
sometimes it doesn't").

Caveat: `callFf`/`getExecutablePath` are **exported but undocumented** (no docs page found;
searches only surface `ensureFfprobe` (removed in v4), the ffmpeg install page, and the CLI).
They are stable in practice (present across 4.0.x, Remotion's own internals depend on them) but
technically internal-adjacent — pin your Remotion version and you're safe either way.

### Also exported (documented) but not what you need
- `getVideoMetadata()` — metadata only (fps/size/codec), implemented via the Rust compositor,
  **deprecated in favor of Mediabunny** for v5.
- `extractAudio()` — audio-only extraction.
- No exported "extract frame(s) from a file" API for arbitrary (non-composition) videos — your
  ffmpeg pipeline remains the right tool.

**Q2 conclusion: keep your own spawn if you want zero new coupling (the `binariesDirectory`
contract is documented and stable), but `getExecutablePath({type:'ffprobe'|'ffmpeg', binariesDirectory})`
is the cheapest hardening — it replaces the hand-rolled join, handles libc/arch selection and
Windows suffixes, and tracks any upstream renames. `callFf` would additionally outsource the
cwd/env handling.**

---

## Q3 — Does Linux need the cwd trick? **[VERIFIED: no — rpath $ORIGIN]**

### Static evidence (ELF dynamic tags, all four variants)

`objdump -p` on every `ffmpeg`/`ffprobe` in the four Linux packages shows:

```
RPATH        $ORIGIN
NEEDED       libavdevice.so
NEEDED       libavfilter.so
NEEDED       libavformat.so
NEEDED       libavcodec.so
NEEDED       libswresample.so
NEEDED       libswscale.so
NEEDED       libavutil.so
```
DT_RPATH (not RUNPATH) — the old-style, transitive variant, so the inter-`libav*` dependencies
resolve through the executable's `$ORIGIN` as well. `libavformat.so`'s own NEEDED entries
(`libavcodec.so`, `libavutil.so`) have no embedded path and are resolved by that same RPATH.

### Dynamic evidence

All end-to-end runs (Q1) were executed with `cwd=/` and `env -i` (empty environment — no
`LD_LIBRARY_PATH`): encoding, probing, and extraction all succeeded from a foreign cwd. The
cwd trick is unnecessary on Linux.

### Why macOS differs (root cause, VERIFIED)

`otool -L ffmpeg` (darwin-arm64): the wrapper references its libraries by **bare install
names** — `libavdevice.dylib`, `libavfilter.dylib`, … — with **no `@rpath`/`@loader_path` and no
`LC_RPATH`** in the binary. dyld's fallback search for a bare relative name includes the
current working directory, hence the observed cwd dependence. Reproduced on this machine:
running the runner's `ffprobe` from `/tmp` → `dyld: Library not loaded: libavdevice.dylib`;
from the binaries dir → works. (This is why Remotion's `callFf` sets
`cwd = dirname(executable)` *and* `DYLD_LIBRARY_PATH` on macOS.)

**Q3 conclusion: on Linux nodes no cwd (or env) handling is required. Keeping your existing
cwd=binaries-dir spawn is harmless (it's exactly what upstream `callFf` does) and keeps the
macOS and Linux code paths identical — recommended.**

---

## Contradictions / corrections vs. the packet's macOS findings

1. **None of substance.** Both macOS findings were reproduced/confirmed as accurate; neither
   turns into a Linux blocker.
2. **"~50 filters" on macOS vs 42 on Linux — artifact.** The named filter sets are identical
   (empty diff darwin↔linux). The macOS count presumably included banner/legend lines.
3. **Additions the packet didn't have:** `rawvideo` is absent as a *demuxer* too (macOS report
   only noted the muxer); `lavfi` is entirely absent (no synthetic input for probes);
   linux-x64-gnu additionally carries NVENC/AV1 encoders; darwin carries VideoToolbox encoders.

---

## Operational notes for the Linux rollout (new findings)

1. **Alpine/musl nodes need system `libstdc++`** — the musl builds' `libavfilter.so`/
   `libavcodec.so` need `libstdc++.so.6` (Alpine: `apk add libstdc++`); without it the binaries
   die at load. Base images: prefer a glibc distro (Debian/Ubuntu) where this doesn't apply.
2. **Bun + musl caveat (if the runner stays on Bun):** `getExecutablePath`'s libc detection
   (`isMusl`) *assumes glibc under Bun* (Bun lacks `process.report`). On a musl host running
   under Bun, Remotion itself would pick the `-gnu` binaries and fail at loader. The farm's
   bun-installed layout (`.bun/` paths observed) makes this a real edge: if you ever run Alpine
   nodes under Bun, the runner must select the compositor package itself rather than via
   `getExecutablePath`. On glibc nodes it's moot.
3. **Pin + probe:** the stripped component set is a per-build property. All of the above is
   exactly true for **4.0.506**; any Remotion bump can change the allowlists. Cheap insurance:
   a boot-time capability probe on each node (`ffmpeg -hide_banner -filters/-muxers/-encoders`
   grep for `scale`, `format`, `image2pipe`, `rawvideo`) that fails the node loudly before it
   accepts jobs — the same class of guard that caught this whole issue.

## What I could not determine

- **Windows**: not tested (no Windows host); `getExecutablePath` implies `ffmpeg.exe` naming.
- **Future/other Remotion versions**: only 4.0.506 was tested (the version driffs and the
  runner pin). No changelog guarantee about the component allowlists exists — hence the probe
  recommendation.
- **Whether `callFf`/`getExecutablePath` count as "supported" long-term**: they are exported and
  typed in the public index but have no documentation page; I found no statement of intent.
  `binariesDirectory` is the only formally documented contract.
- **The upstream build script source** for the ffmpeg configure line (it is not in the decent
  fork's tree); the configure string embedded in the shipped binaries is the primary evidence
  used instead.

## Artifacts

- Packages + extraction: `/tmp/remotion-binaries-research/` (four tarballs + unpacked dirs)
- Captured outputs: `filters-*.txt`, `cfg-*.txt` in that directory
- Docker images used: `debian:bookworm-slim` (amd64+arm64), `alpine:3.21` (amd64+arm64)

DECENT_RESEARCH_BINARIES_DONE
