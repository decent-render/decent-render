# Changelog

## Unreleased

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
