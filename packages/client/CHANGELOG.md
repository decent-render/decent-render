# Changelog

## 0.3.0 — unreleased

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
