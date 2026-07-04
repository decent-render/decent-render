# Decent Render — Operator App

A Tauri v2 desktop app that wraps the same `supervisor-core` as the `decent-node`
CLI. One core, two skins: the app drives the exact same `connection::run` code
path, just with status/log channels attached.

## Prerequisites

- [Bun](https://bun.sh) 1.3+
- [Rust](https://rustup.rs) (stable, matches the workspace)
- Tauri v2 system dependencies (on macOS: Xcode Command Line Tools)

## Getting started

```sh
cd apps/decent-app
bun install          # frontend deps (React + Vite + Tauri API)
bun run tauri dev    # launches the app in dev mode
```

The first `bun run tauri dev` compiles the Rust backend (may take a few minutes
the first time). Subsequent runs are fast thanks to incremental compilation.

## Using the app

1. **Dispatch URL** — defaults to `ws://localhost:8790/ws` (local dispatch).
2. **Worker Token** — paste a freshly-minted worker JWT.
   Mint one from the driffs repo: `bun scripts/mint-worker-token.ts my-node driffs`
3. **Start** — connects to dispatch, registers, and begins heartbeating.
   The connection badge shows `REGISTERED` on success.
4. **Accept real render jobs** — toggle this ON to allow `jobAssign` processing.
   Default is OFF (same safety posture as `--allow-real-jobs` on the CLI).
5. **Current Job** — when a job is assigned, shows the job ID, tier, phase,
   and a live progress bar.
6. **Session Stats** — completed / failed / canceled counters for this session.
7. **Log Tail** — tailable log stream from the supervisor core.

## Config persistence

- Dispatch URL, workdir root, and the allow-real-jobs default are persisted to
  the platform config dir (`~/Library/Application Support/decent-render/config.json`
  on macOS).
- The worker token is stored in the same config file (gitignored, never committed).
  A future version will use the OS keychain for the token.

## How the app maps to the CLI

| Concern              | CLI (`decent-node`)              | App (`decent-app`)                    |
|----------------------|----------------------------------|---------------------------------------|
| Core code path       | `connection::run`                | `connection::run` (same function)     |
| Observability        | `Observability::default()`       | `Observability::channels()`           |
| Allow real jobs      | `--allow-real-jobs` flag         | UI toggle → `Observability::set_*`    |
| Status visibility    | `tracing` logs (stdout)          | `watch::channel` → Tauri events → UI  |
| Connection control   | process lifecycle                | Start/Stop buttons → task abort       |
| Purge rule           | enforced by `WorkDir::Drop`      | enforced by `WorkDir::Drop` (same)    |

The app **cannot** bypass the purge rule — it observes and controls, but the
core enforces workdir deletion structurally.

## Earnings / allocation / multi-tenant console

Deferred to Phase 2. The operator dashboard for earnings, settlement, and
multi-tenant allocation requires:
- Network identity (own domain, operator signup flow)
- Settlement layer (driffs credits ledger + denomination field)
- Operator DPA / ToS signing flow

These are Phase-2 milestones per the plan doc. The app's UI shell is ready
to receive them — they'll be additional cards/tabs, not a rewrite.

## Architecture

```
┌──────────────────────────────────────────┐
│  Webview (React + TypeScript + Vite)     │
│  ├─ Connection card (state, identity)     │
│  ├─ Controls (start/stop, toggle)         │
│  ├─ Current job (progress bar)            │
│  ├─ Session stats                         │
│  └─ Log tail                              │
└──────────────┬───────────────────────────┘
               │ Tauri commands + events
┌──────────────┴───────────────────────────┐
│  Rust Backend (src-tauri/src/main.rs)    │
│  ├─ start_connection() → spawns task     │
│  ├─ stop_connection() → aborts task      │
│  ├─ set_allow_real_jobs() → atomic flag   │
│  └─ Poll loop → emits status + log events │
└──────────────┬───────────────────────────┘
               │ Observability bundle (channels attached)
┌──────────────┴───────────────────────────┐
│  supervisor-core (crates/supervisor-core) │
│  ├─ connection::run() ← same as CLI       │
│  ├─ runner.rs (payload download + render)  │
│  ├─ purge.rs (WorkDir::Drop)              │
│  └─ status.rs (watch + broadcast channels) │
└──────────────────────────────────────────┘
```
