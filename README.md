# Decent Render — node supervisor

Open-source node supervisor for the **Decent render network**: a distributed
render farm for Remotion compositions, including GPU (WebGPU/Metal) renders
that serverless infrastructure can't do. Operators run one small signed app on
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
| `bins/decent-node`       | Thin CLI over the core                                                                                                                                                                                                |
| `apps/decent-app`        | Tauri v2 desktop app over the same core — device pairing, OS-keychain token storage, connection controls, live job progress, session stats, log tail, earnings console                                                |

One core, two skins: the CLI and the Tauri app drive the exact same
`connection::run` code path, just with different observability attached.

## Usage

```sh
decent-node start --dispatch-url ws://localhost:8790/ws --token <jwt>
# or via env:
DISPATCH_URL=wss://dispatch.example.com/ws WORKER_TOKEN=<jwt> decent-node start
```

Worker tokens are minted by the platform (tenant) you register with.

## Status

Implemented: register + heartbeat + protocol v2 + purge guard + **job-execution
orchestration** + observable status bus + the Tauri operator app.

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
cargo test
cargo clippy -- -D warnings
cargo fmt --check
```
