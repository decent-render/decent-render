# Changelog

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
