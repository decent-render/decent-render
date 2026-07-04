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
ledger are. What the open source buys is *auditability*: the **purge rule**
(`purgeAfter` on every job assignment → the per-job working directory is
deleted when the job ends, success or failure, panic included) is verifiable
in `crates/supervisor-core/src/purge.rs`. Your machine only ever holds
platform bundles and transient job assets — never persisted user content.

Licensed **Apache-2.0**. The render payload (platform Remotion bundles), the
dispatch service, and the credit system are separate, closed components.

## Layout

| Path | What |
|---|---|
| `crates/supervisor-core` | The core: wire protocol (v2), WebSocket connection loop, purge rule |
| `bins/decent-node` | Thin CLI over the core (the Tauri desktop shell comes later) |

## Usage

```sh
decent-node start --dispatch-url ws://localhost:8790/ws --token <jwt>
# or via env:
DISPATCH_URL=wss://dispatch.example.com/ws WORKER_TOKEN=<jwt> decent-node start
```

Worker tokens are minted by the platform (tenant) you register with.

## Status

Skeleton: register + heartbeat + protocol types + purge guard. Job execution
(bundle download with sha256 verification, Remotion render under the proven
WebGPU recipe, presigned upload) is the next milestone — the TS reference
worker it ports from is proven end-to-end.

## Development

```sh
cargo test
cargo clippy -- -D warnings
cargo fmt --check
```
