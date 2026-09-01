#!/bin/sh
# Linux gate for the Rust crates, runnable on a macOS dev box via Docker.
#
# Why: `bins/decent` and `crates/supervisor-core` carry `cfg(target_os =
# "linux")` code (systemd units, DRM detection) that macOS never compiles.
# 2026-09-01: a Linux-only call site referenced a module-level helper
# unqualified; every macOS gate was green and CI was red for three pushes.
# Run this before pushing anything that touches a `cfg(linux)` region.
#
# Mirrors ci.yml's "Rust + CLI quality gates" step (clippy over the
# workspace minus the Tauri app, then the tests). Uses named volumes so the
# Linux target dir and registry cache never touch the host's `target/`.
set -eu
cd "$(dirname "$0")/.."
IMAGE="${RUST_IMAGE:-rust:1.96-bookworm}"
# --init: tini as pid 1. Without it pid 1 is the `sh -c` below, which sits
# in read() on the command substitution and never reaps orphans — the
# kill-tree tests' grandchildren then linger as zombies, kill(pid, 0) still
# succeeds on a zombie, and five containment tests report "survived"
# (farm-web BACKLOG N-8, diagnosed 2026-09-02: STAT Z, PPID 1, every time).
# Real hosts (systemd, launchd, the CI runner) reap orphans; the gate must too.
exec docker run --rm --init \
  -v "$PWD":/src -w /src \
  -v decent-render-linux-target:/target \
  -v decent-render-linux-cargo:/usr/local/cargo/registry \
  -e CARGO_TARGET_DIR=/target \
  "$IMAGE" sh -c '
    set -eu
    rustup component add clippy >/dev/null 2>&1 || rustup component add clippy
    cargo clippy --workspace --exclude decent-app --all-targets --all-features -- -D warnings
    # Full output on failure (a filtered summary hid the first real Linux
    # bug this script caught: an ExecStart=ExecStart= unit line).
    # Default test parallelism, as in CI: the "load-sensitive" reds that
    # once pinned this to 4 threads were the missing-init zombies above.
    if ! out=$(cargo test --workspace --exclude decent-app 2>&1); then
      printf "%s\n" "$out" | grep -vE "^\s*(Compiling|Running|Finished)" | tail -60
      exit 1
    fi
    printf "%s\n" "$out" | grep -E "^test result"
    echo LINUX-GATE-OK
  '
