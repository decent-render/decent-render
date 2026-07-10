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
ledger are. What the open source buys is _auditability_: the **purge rule**
(`purgeAfter` on every job assignment → the per-job working directory is
deleted when the job ends, success or failure, panic included) is verifiable
in `crates/supervisor-core/src/purge.rs`. Your machine only ever holds
platform bundles and transient job assets — never persisted user content.

Licensed **Apache-2.0**. The render payload (platform Remotion bundles), the
dispatch service, and the credit system are separate, closed components.

## Layout

| Path                     | What                                                                                                                                                                                                                  |
| ------------------------ | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `crates/supervisor-core` | The core: wire protocol (v2), outbound WebSocket loop, job-execution orchestration (payload download + sha256 verify + spawn versioned runner + stream progress + upload + cancel), observable status bus, purge rule |
| `bins/decent`       | Thin CLI over the core                                                                                                                                                                                                |
| `apps/decent-app`        | Tauri v2 desktop app over the same core (**in-repo and maintained — a windowed console for local debugging; the CLI is the primary operator surface**)                                                                |

One core: the CLI is the shipped operator surface. The in-repo Tauri app
drives the exact same `connection::run` code path with richer observability,
and is kept maintained as a windowed console for local debugging. The CLI is
the primary operator surface; a web dashboard (`decent-render.farm`) will be the
management surface for tracking your machines.

## Install

Apple Silicon macOS is the supported release target.

```sh
brew install decent-render/tap/decent
```

Or install the latest GitHub Release with its generated shell installer. Build
from source when developing (requires Rust + Cargo):

```sh
cargo install --git https://github.com/decent-render/decent-render decent
```

## Usage

> **Pre-v0.1 compatibility name:** v0.0.4 is installed as `decent`. The
> public CLI will be renamed to `decent` before v0.1, with an upgrade shim that
> preserves existing token/config/launchd state.

```sh
# Store a token issued by the tenant/network you are joining.
decent login --token <worker-jwt>

# Install the unattended launchd daemon against production dispatch.
decent install

# Inspect and control it.
decent status
decent pause
decent resume
decent upgrade

# Or run the foreground terminal dashboard instead of the daemon.
decent tui --dispatch-url wss://dispatch.example.com/ws --allow-real-jobs
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
over NDJSON stdout. The actual Remotion render happens inside that runner — the
open supervisor orchestrates; the render payload is a closed, versioned
component (see open/closed framing below). Cancellation kills the runner within
a grace window. The TS reference worker (`scripts/spike-worker.ts` in driffs)
that this architecture ports is proven end-to-end through the live farm.

A safety gate (`allow_real_jobs`, default **off**) refuses `jobAssign` until the
operator explicitly opts in — both on the CLI (`--allow-real-jobs`) and in the
app (UI toggle). The app cannot bypass the purge rule; it observes and
controls, the core enforces workdir deletion structurally (`WorkDir::Drop`).

## Development

```sh
cargo fmt --all -- --check
cargo clippy -p supervisor-core -p decent --all-targets --all-features -- -D warnings
cargo test -p supervisor-core -p decent
```

Read [AGENTS.md](./AGENTS.md) for invariants and the full gate matrix,
[CONTRIBUTING.md](./CONTRIBUTING.md) before changing code, and
[RELEASING.md](./RELEASING.md) before creating a tag. User-facing node changes
are tracked in [CHANGELOG.md](./CHANGELOG.md).
