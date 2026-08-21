# scripts/e2e — local end-to-end render rig

This is the rig that proved the first end-to-end render through the farm
(packet 3, 2026-08-21) and the browser-containment work after it. It drives
the REAL supervisor binary against a LOCAL dispatch harness over the REAL
wire protocol — no fly/Turso/R2/npm involved. Nothing here talks to any
production system; the harness replaces dispatch entirely.

## Pieces

| File | What it is |
|---|---|
| `local-dispatch.mjs` | Minimal WebSocket dispatch (Bun.serve, one port: HTTP artifacts + `/ws`). Validates every worker frame against the REAL zod schemas from `packages/protocol` (repo-local dist) and its own jobAssign against `ServerMessageSchema` before sending. `--cancel-after=1` switches to event-driven cancel at progress ≥ 0.3. |
| `make-bundle.mjs` | Builds a real Remotion bundle (webpack, via `@remotion/bundler`) from `bundle-src/` into `$E2E_ARTIFACTS/bundle.tar.gz`. Run it from a checkout that has `@remotion/bundler` installed (e.g. the farm-web runner app) — see README section below. |
| `bundle-src/` | Minimal composition: 30 frames @30fps, 640×360, colour sweep + frame counter (`p3comp`). |

Artifacts the supervisor downloads (payload/browser/bundle tarballs) are
content-addressed by sha and LARGE (browser ~166MB). Keep them OUT of git:
they live in `/tmp/p3-artifacts` (override: `--artifacts` / `E2E_ARTIFACTS`).

## Reproduce an end-to-end render

```sh
export PATH="/opt/homebrew/opt/node@26/bin:$PATH"

# 0. supervisor binary
cd decent-render && cargo build -p decent

# 1. artifacts (one-time):
#    payload.tar.gz + browser.tar.gz — build via farm-web's
#    scripts/publish-runner-payload.ts --dry-run --out=/tmp/p3-artifacts
#    (its documented local-build path; no R2, no DB rows).
#    bundle.tar.gz — bun scripts/e2e/make-bundle.mjs   (needs @remotion/bundler on the import path; run from farm-web/apps/runner-<v> or copy the import)

# 2. local dispatch harness (success run)
bun scripts/e2e/local-dispatch.mjs --port=8790 --artifacts=/tmp/p3-artifacts

# 3. supervisor (another terminal)
RUST_LOG=info ./target/debug/decent start \
  --dispatch-url ws://127.0.0.1:8790/ws --token p3-local-token --allow-real-jobs

# 4. verify output
ffprobe -v error -show_entries stream=codec_name,width,height,nb_frames \
  -of json /tmp/p3-artifacts/uploaded-output.mp4
```

Cancel run instead of success: add `--cancel-after=1` to step 2 (event-driven
cancel at the first jobProgress ≥ 0.3).

## What a clean run proves

register → jobAssign → payload+browser+bundle download with sha256 verify →
runner spawn in own process group → real Chrome render (h264) → output PUT →
workdir purge — plus, on the cancel run: group TERM → runner SIGTERM handler
workdir purge → no orphaned Chrome / runner processes (check with
`pgrep -f decent-worker`).

## Keep out of git

The tarballs under the artifacts dir are large and machine-local. Never commit
them; the artifacts dir is a scratch space (`/tmp/...`), which keeps it out of
the repo by construction.
