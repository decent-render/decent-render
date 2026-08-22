# Changelog

## 0.1.3

- Retry the whole render (composition selection + renderMedia) exactly ONCE
  when the first attempt fails with a delayRender timeout — the GPU-adapter
  contention failure class where a hung `navigator.gpu.requestAdapter()`
  promise never settles and the delayRender window expires. The hung promise
  dies with the Chrome that owns it, so one fresh attempt usually wins the
  adapter. The retry matches Remotion's runtime error FORMAT (any
  `A delayRender() … was called but not cleared after …ms`), not
  tenant-authored labels; non-delayRender failures never retry; a cancel
  observed during the first attempt suppresses the retry; wall-clock keeps
  counting across attempts; and the retry is visible on the runner's log
  line (`[retry] attempt 1 failed with a delayRender timeout … attempt 2 of
  2`) so an operator can trace exactly what happened.

## 0.1.2

- SIGTERM/SIGINT now purge the active working directory before exit, so a
  canceled or superseded job cannot leave rendered customer content on the
  operator's disk. The handler records the cancel first; nothing after it can
  lose that state.
- A canceled job never uploads. `renderJob` re-checks the cancel flag
  immediately before the output PUT, behind a one-tick yield whose ordering
  (signal handler runs before a post-sync `setImmediate`) is probe-verified on
  both macOS and Linux Bun. What remains is only a signal landing while the
  PUT is already on the wire.
- Rendered output is verified before upload: container/stream probing and
  sampled frame decoding using the payload's own ffmpeg/ffprobe, so black or
  corrupt renders (dead GPU, broken codec) fail the job instead of shipping.
  Implausibly small and implausibly large outputs are rejected from the same
  pre-read `stat` — the size cap runs before any file read.
- The output PUT streams from disk instead of buffering the entire file
  (previously up to the 2 GiB cap) in memory. Content-Length is always sent —
  never chunked transfer encoding, which S3-compatible presigned PUTs reject —
  verified at the socket level under both Bun and Node. Failure paths drain
  the stream deterministically before the workdir purge.
- `jobComplete` reports the measured frame count from the probed file, not the
  composition's declared duration.

## 0.1.1

- Requires `@decent-render/protocol` ^0.1.1. The browser fields added in
  protocol 0.1.2 are optional and unread here — the supervisor resolves the
  browser and passes a path — so this package does not gate on that release.
  Publish order stays protocol first, then runner-core.

- The browser may now come from outside the payload. `resolveBrowserExecutable`
  is exported and prefers `DECENT_BROWSER_EXECUTABLE` — set by the supervisor
  after it fetches and verifies a standalone browser artifact — falling back to
  the payload's own `chrome/executable` manifest. Splitting the browser out means
  a given Chrome is downloaded once per operator instead of once per Remotion
  version; nothing changes for payloads that still bundle their own.
- An injected path that does not exist falls through to the payload rather than
  failing the job, so a stale environment variable cannot take a working payload
  offline.

## 0.1.0

- Initial public release. The runner main loop and render job move here from the
  closed platform repo so the code that touches tenant content is auditable.
- `runRunner`: stdin `jobAssign` frame → NDJSON `progress`/`done`/`error` frames
  on stdout, with renderer stdout chatter diverted to stderr.
- `renderJob`: bundle download with sha256 verification and content-addressed
  caching, injected-renderer render, presigned output upload, working-directory
  purge in `finally`.
- `RendererApi`: structural types for the injected `@remotion/renderer` slice.
  This package intentionally declares no `@remotion/*` dependency.
