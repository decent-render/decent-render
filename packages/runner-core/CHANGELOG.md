# Changelog

## 0.1.1

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
