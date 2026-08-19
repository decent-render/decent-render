# @decent-render/runner-core

The render logic that runs on an operator's machine: the runner main loop and
the render job itself — bundle download + sha256 verification, Remotion
`selectComposition`/`renderMedia`, presigned upload of the output, and working-
directory purge.

This is the code that touches tenant content, so it lives here in the open
rather than inside the closed platform.

## Why this package has no Remotion dependency

The farm serves several Remotion versions at once, and each one must render on
the exact version a tenant's bundle was built against. So runner-core takes the
renderer as an **injected argument** and declares no `@remotion/*` dependency at
all. Each versioned runner app imports its own pinned renderer and hands it in:

```ts
#!/usr/bin/env bun
import {runRunner} from '@decent-render/runner-core';
import {renderMedia, selectComposition} from '@remotion/renderer';

await runRunner({renderMedia, selectComposition});
```

The `@remotion/renderer` import MUST stay in the app's entry file. That is what
makes `bun build --compile` embed *that app's* pinned version; an import from
inside this package would resolve against this package's directory instead.
`RendererApi` in `src/renderer-api.ts` is therefore a structural type over the
slice of `@remotion/renderer` that is actually used — not an import of it.
`src/__tests__/renderer-api.test.ts` asserts the invariant (no `@remotion/*` in
the manifest, no renderer import in the sources).

## What the render path does

`renderJob` (`src/render-job.ts`), in order:

1. Downloads the pinned bundle from the presigned GET, **hashes the bytes and
   rejects the job if the sha256 does not match** what dispatch advertised.
   Nothing downstream runs on unverified bytes.
2. Caches the verified bundle at `~/.decent-worker/bundles/<sha256>` and
   extracts it; a later job with the same sha reuses it without re-downloading.
3. Fetches `{compositionId, inputProps}` from the presigned props GET.
4. Renders into a per-job temp dir with `chromeMode: 'chrome-for-testing'` and
   `chromiumOptions: {gl: 'angle'}`, concurrency 1, `colorSpace: 'bt709'`,
   codec `vp8` → `out.webm` / `h264` → `out.mp4`. Progress is throttled to 5%
   steps plus a final 1.
5. Uploads the output via the presigned PUT.
6. **Purges the per-job working directory in a `finally`** — success, failure,
   or throw.

`runRunner` (`src/index.ts`) wraps that as the payload process: one `jobAssign`
frame in on stdin, NDJSON `progress`/`done`/`error` frames out on stdout, exit 0
or 1.

## stdout is the protocol; logs go to stderr

The supervisor parses stdout as NDJSON, so `runRunner` swaps
`process.stdout.write` to stderr before doing anything else and writes frames
through the captured original. Renderers log liberally; this keeps that chatter
out of the stream.

Known gap: under Bun, `console.log` writes to fd 1 directly and does **not**
route through `process.stdout.write`, so it is not caught by that swap. The
supervisor ignores non-NDJSON stdout lines (`ignoring non-NDJSON runner stdout
line`), so this costs observability, not correctness. It is pinned by a
characterization test in `src/__tests__/stdout-discipline.test.ts`.

## Runtime

`runRunner` reads stdin via `Bun.stdin` and is built to be compiled with
`bun build --compile`. `renderJob` itself is plain Node API usage and is
exercised under Node in the test suite.

## Develop

```sh
cd packages/runner-core
bun install
bun run build
bun run test             # 20 tests; the stdout suite spawns real `bun` subprocesses
```

## Publishing

Same posture as the other packages in this repo: **OIDC trusted publishing, no
npm token exists.** Publishing is a `workflow_dispatch` GitHub Actions workflow
([publish-runner-core.yml](../../.github/workflows/publish-runner-core.yml))
that authenticates to npm with a short-lived per-run OIDC token.

To publish a new version: bump `version` here, commit + push, then GitHub →
Actions → **Publish runner-core package** → Run workflow, and approve the
`publish` environment prompt.

## What auditing this package does and does not prove

It proves what the render path does on an operator's machine: the sha256 gate,
the render settings, the upload, the purge.

It does **not** prove that the binary an operator downloads was built from this
source. The payload tarball is compiled and published by the closed platform,
and the builds are not reproducible, so an operator can verify the sha256 that
dispatch advertises for the payload but cannot independently derive that sha
from this repository.

## License

Apache-2.0.
