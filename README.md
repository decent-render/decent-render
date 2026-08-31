# Decent Render — node supervisor

Open-source node supervisor for the **Decent render network**: a distributed
render farm for Remotion compositions, including GPU (WebGPU/Metal) renders
that serverless infrastructure can't do. Operators run the `decent` CLI on
an Apple-Silicon Mac; the supervisor opens a single **outbound** WebSocket to
the dispatch service (GitHub-Actions-runner model — works behind any NAT, zero
router configuration), receives jobs, renders, uploads, and purges.

**driffs is tenant #1.** The protocol is multi-tenant by construction — every
message carries a `tenant` field.

## Why open source

The worker binary is not the moat — demand, coordination, and the credit
ledger are. What the open source buys is _auditability_ of the code that runs
on your machine and touches tenant content:

- the **purge rule** (`purgeAfter` on every job assignment → the per-job
  working directory is deleted when the job ends, success or failure, panic
  included) is verifiable in `crates/supervisor-core/src/purge.rs`;
- the **render path itself** is verifiable in `packages/runner-core`
  (`@decent-render/runner-core`): the bundle sha256 gate, the render settings,
  the presigned upload of the output, and the working-directory purge.

Your machine only ever holds platform bundles and transient job assets — never
persisted user content.

### What this does not prove

Auditing this repository tells you what the render path *does*. It does not
prove that the payload binary your supervisor downloads was built from this
source. That binary is compiled and published by the closed platform from
`packages/runner-core` plus a pinned `@remotion/renderer`, and the build is not
reproducible — `bun build --compile` does not currently produce byte-identical
output across runs, so the sha256 of a payload cannot be re-derived from this
repository. What the supervisor verifies is that the bytes it downloaded match
the sha256 dispatch advertised for that payload; it cannot verify provenance.
Closing that gap needs reproducible builds, which is not implemented.

Licensed **Apache-2.0**. The compiled render payload, the platform Remotion
bundles, the dispatch service, and the credit system are separate, closed
components.

## Layout

| Path                     | What                                                                                                                                                                                                                  |
| ------------------------ | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `crates/supervisor-core` | The core: wire protocol (v2), outbound WebSocket loop, job-execution orchestration (payload download + sha256 verify + spawn versioned runner + stream progress + upload + cancel), observable status bus, purge rule |
| `bins/decent`       | Thin CLI over the core                                                                                                                                                                                                |
| `packages/runner-core`   | The render path that runs inside the payload: bundle download + sha256 verify + cached extract, Remotion render (renderer injected, no version pinned here), presigned upload, workdir purge. Published as `@decent-render/runner-core` |
| `apps/decent-app`        | Tauri v2 desktop app over the same core (**in-repo and maintained — a windowed console for local debugging; the CLI is the primary operator surface**)                                                                |

One core: the CLI is the shipped operator surface. The in-repo Tauri app
drives the exact same `connection::run` code path with richer observability,
and is kept maintained as a windowed console for local debugging. The CLI is
the primary operator surface; a web dashboard (`decent-render.farm`) will be the
management surface for tracking your machines.

## Install

Apple Silicon macOS and Linux (x86_64 and aarch64, glibc).

**macOS** — Homebrew, which is also how `decent upgrade` works:

```sh
brew install decent-render/tap/decent
```

**Linux** — the installer published with each release:

```sh
curl -LsSf https://github.com/decent-render/decent-render/releases/latest/download/decent-installer.sh | sh
```

Homebrew does run on Linux and the formula would work there, but effectively no
one operating a headless render box has it installed, so the installer is the
supported Linux path. `decent upgrade` re-runs it for you.

Build from source when developing (requires Rust + Cargo):

```sh
cargo install --git https://github.com/decent-render/decent-render decent
```

### What a Linux node can and cannot do

Both architectures render. Only x86_64 can use the GPU path: `chrome-for-testing`
has no stable `linux-arm64` build, so on ARM, Remotion substitutes a Playwright
chromium. `decent` detects this and reports `gpu: false`, which means dispatch
sends it standard jobs only — that is correct behaviour, not a misconfiguration.
On x86_64 the GPU path needs a DRM render node (`/dev/dri/renderD*`); without
one, `decent` again reports `gpu: false`.

Alpine and other musl distributions are not supported: the render payloads are
built against glibc, so a musl node would install and then fail to run them.

## Usage

> **Renamed from `decent-node`:** the CLI is `decent` as of v0.0.5. A
> `decent-node` shim still forwards to it on macOS and is removed at v0.1;
> `decent install` migrates the token and the daemon label for you.

```sh
# Store a token issued by the tenant/network you are joining.
decent login --token <worker-jwt>

# Install the unattended daemon against production dispatch.
# launchd agent on macOS; systemd user unit (with lingering) on Linux.
decent install

# Inspect and control it.
decent status
decent pause
decent resume
decent upgrade

# Or run the foreground terminal dashboard instead of the daemon.
# (Both default to the production dispatch URL; override with --dispatch-url.)
decent tui --allow-real-jobs
```

Worker tokens are minted by the platform (tenant) you register with. Real jobs
remain disabled unless the operator explicitly opts in. Do not run the TUI and
installed daemon simultaneously with the same device token.

The future management surface is `decent-render.farm`: operators manage paired
machines there; tenants manage API keys, usage, rotation/revocation, and
webhooks. CLI/manual token scripts are bootstrap and internal-testing paths.

## Status

Implemented: register + heartbeat + protocol v2 + purge guard + **job-execution
orchestration** + observable status bus. (A Tauri desktop app also lives
in-repo over the same core, maintained as a windowed console — the CLI is the
primary operator surface today.)

Job execution works by **spawning versioned render payloads**: the supervisor
downloads the assigned payload, verifies its sha256, extracts it, and spawns the
bundled `decent-render-runner` binary, streaming progress/done/error events back
over NDJSON stdout. The actual Remotion render happens inside that runner, whose
logic is `packages/runner-core` in this repo. The payload stays a *versioned,
platform-built artifact* — it is compiled from that source together with a
pinned `@remotion/renderer` and published by the closed platform, so the
supervisor verifies its sha256 but not its provenance (see "What this does not
prove" above). Cancellation kills the runner within a grace window. The TS reference worker (`scripts/spike-worker.ts` in driffs)
that this architecture ports is proven end-to-end through the live farm.

A safety gate (`allow_real_jobs`, default **off**) refuses `jobAssign` until the
operator explicitly opts in — both on the CLI (`--allow-real-jobs`) and in the
app (UI toggle). The app cannot bypass the purge rule; it observes and
controls, the core enforces workdir deletion structurally (`WorkDir::Drop`).

**Updates are manual by design.** The node never self-updates (a bad release
must not brick an unattended machine); `decent upgrade` runs
`brew upgrade decent` on macOS or the installer on Linux and restarts the
daemon. The wire has an `updateAvailable` frame the CLI already renders as a
banner, but the dispatch service does not send it yet — release notification
is tracked as open work on the platform side.

## Development

```sh
cargo fmt --all -- --check
cargo clippy -p supervisor-core -p decent --all-targets --all-features -- -D warnings
cargo test -p supervisor-core -p decent
```

The TypeScript packages are installed and gated per package (there is no root
workspace):

```sh
cd packages/runner-core && bun install && bun run build && bun run test
```

Same for `packages/protocol` and `packages/client`. CI runs all three.

Read [AGENTS.md](./AGENTS.md) for invariants and the full gate matrix,
[CONTRIBUTING.md](./CONTRIBUTING.md) before changing code, and
[RELEASING.md](./RELEASING.md) before creating a tag. User-facing node changes
are tracked in [CHANGELOG.md](./CHANGELOG.md).
