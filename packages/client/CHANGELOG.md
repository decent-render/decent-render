# Changelog

## 0.5.0 — unreleased

- **R2 legacy cut**: the R1 deploy-window hedges are gone — complete status
  REQUIRES the `outputSizeInBytes` KEY (value stays nullable: mirrors the
  nullable column for HEADs with no size), and webhook `composition.codec`
  is nullable-not-optional (dispatch always sends it; null for stills).
  `renderStillOnFarm` unchanged: it requires a measured NUMBER and throws
  `OUTPUT_SIZE_UNAVAILABLE` on null.

## 0.4.0 — unreleased

- **`renderStillOnFarm()` (FARM-STILL)** — render ONE frame of a
  composition on the farm as a LOSSLESS PNG: `renderStillOnFarm({apiKey,
  bundleSha256, inputProps, frame, compositionId?, chromiumOptions?,
  compositionWidth, compositionHeight, fps, durationFrames, selfRender?,
  kind?}) → {url, sizeInBytes, renderId, frame, verification,
  creditsSettled}`. Enqueues on the SAME `POST /api/v1/renders` route and
  schema as `enqueueRender` (with the optional `still` directive) and polls
  to completion with the identical walk-away-cancel contract as
  `renderMediaOnFarm`. Not a frame extracted from an mp4 — the node renders
  the frame directly (`renderStill`, `chrome-for-testing`, `gl: 'angle'`)
  and verifies the PNG (signature + IHDR geometry) BEFORE uploading.
  `verification` is HONESTLY `pending` for stills: no off-node referee
  exists — the caller's own evidence chain (sha256 + parity re-check) is
  the verification. `chromiumOptions.gl` is honored only as `'angle'` (the
  farm-wide constant); other values are refused client-side with
  `CHROMIUM_GL_UNSUPPORTED` before any network call. `kind` defaults to
  `'gpu'` (selection only — the certification stills target WebGPU-capture
  compositions). A complete response without a measured
  `outputSizeInBytes` is a named client error (`OUTPUT_SIZE_UNAVAILABLE`),
  never a guessed figure.
- **`enqueueRender` accepts `still`** — `{frame: int ≥ 0, format: 'png'}`
  (optional; absent ⇒ today's video request, byte-identical). The
  cross-field bound `still.frame < durationFrames` lives ON the schema,
  which is also the dispatch front door's validator — one schema, one
  validator, one enqueue path; a still outside the composition is refused
  identically at both ends. `durationFrames` is the composition's REAL
  duration; the still renders ONE frame of it.
- **Status/webhook honesty for stills** — complete status responses may
  carry `outputSizeInBytes` (optional + nullable; dispatch HEADs the
  output at completion), and webhook `composition.codec` is now nullable —
  a still job has no video codec, and null is the honest value (a fake
  `'h264'` would be a lie the ledger cannot audit).

## 0.3.0 — published

- **Cancel-ack witness (F4)**: render status responses may carry
  `cancelAckedAt` (ISO date or null) and `cancelTeardownMs` (integer ≥ 0
  or null) — when the node acknowledged the farm's cancel (AFTER the
  job's teardown + purge joined, never on cancel-frame receipt) and how
  long that teardown took, ms. Both OPTIONAL + nullable: a dispatch
  predating the fields omits them during the deploy window, and a
  pre-F4 node never sends an ack, so its rows read NULL forever.

- **Accrued-cost protocol (F2)**: render status responses may carry
  `accruedCredits` (integer ≥ 0 or null) and `accruedAt` — the farm's
  mid-flight priced accrual, computed from the node's raw measurements.
  Both are OPTIONAL in the schema so responses from a dispatch predating
  the fields still parse during the deploy window, and NULL (never zero)
  while a pre-F2 node renders. Terminal money stays `creditsSettled`.

- **Behaviour:** `verifyWebhookSignature()` now enforces a replay window —
  a delivery whose `X-Decent-Timestamp` is more than `toleranceSeconds`
  (default 300) from `now` returns `false` before the HMAC is compared, and a
  timestamp that is not a unix-seconds integer never verifies. Pass
  `toleranceSeconds` to widen the window, `now` to pin the clock in tests.
  New export `WEBHOOK_TIMESTAMP_TOLERANCE_SECONDS`. Test fixtures with a
  fixed old timestamp must pass `now` (or a wide tolerance).

## 0.2.0 — 2026-09-02

- **Breaking (types):** `creditsSettled` on complete-status responses and in
  `RenderMediaOnFarmResult` is now `number | null` — null when the job
  completed before measured settlement existed (pre-migration-0016 rows);
  it was never a zero.
- **Breaking (surface):** the root entry no longer re-exports the Zod schema
  module (`export *` removed). Import schemas from the new
  `@decent-render/client/schemas` subpath; the root keeps the functions,
  `FarmApiError`, `isFarmApiError`, and the request/response types the
  function signatures use.
- **`FarmApiError.kind`** (`'http' | 'client'`) distinguishes real HTTP
  failures from client-side conditions — poll timeout, abort, a terminal
  render state delivered inside a 200 poll, pre-archive validation — whose
  `status` is SYNTHETIC (an HTTP-shaped hint, not a response status). New
  `isFarmApiError()` type guard.
- **`bundleAndUpload()` validates `remotionVersion` before archiving.** A
  version that is not a full `major.minor.patch` release throws
  `FarmApiError` (`kind: 'client'`, `code: 'INVALID_REMOTION_VERSION'`)
  without bundling, reading the filesystem, or calling the farm.

- `renderMediaOnFarm()` now cancels the render it abandons: on the internal
  `timeoutMs` and on an external `AbortSignal` it POSTs the cancel endpoint
  before throwing/rejecting (best-effort, never masks the original error).
  Previously a timed-out or aborted caller left the job rendering and billing
  on the farm.
- `bundleAndUpload()` archives now include empty directories (ustar type `5`).
  They were silently dropped. Bundles without empty directories hash exactly
  as before.

## 0.1.0

- Add workspace-scoped Remotion bundle upload and registration.
- Add render enqueue, progress polling, cancellation, and completion helpers.
- Add hold-aware balance and webhook CRUD helpers.
- Export the exact Zod request/response schemas consumed by farm handlers.
- Add HMAC webhook signature verification.
