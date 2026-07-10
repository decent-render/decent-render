#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "usage: $0 X.Y.Z" >&2
  exit 64
fi

version="$1"
root="$(git rev-parse --show-toplevel)"
cd "$root"

if [[ -n "$(git status --porcelain)" ]]; then
  echo "release check failed: working tree is not clean" >&2
  exit 1
fi

manifest_version="$(awk -F'"' '/^version = / {print $2; exit}' bins/decent-node/Cargo.toml)"

if [[ "$manifest_version" != "$version" ]]; then
  echo "release check failed: manifest=$manifest_version requested=$version" >&2
  exit 1
fi

if git rev-parse -q --verify "refs/tags/v$version" >/dev/null; then
  echo "release check failed: tag v$version already exists" >&2
  exit 1
fi

if ! grep -Fq "## [$version]" CHANGELOG.md; then
  echo "release check failed: CHANGELOG.md has no ## [$version] section" >&2
  exit 1
fi

cargo fmt --all -- --check
cargo clippy -p supervisor-core -p decent-node --all-targets --all-features -- -D warnings
cargo test -p supervisor-core -p decent-node
cargo build -p decent-node

actual="$(./target/debug/decent-node --version)"
if [[ "$actual" != *"$version"* ]]; then
  echo "release check failed: binary reports '$actual'" >&2
  exit 1
fi
./target/debug/decent-node --help >/dev/null

(
  cd packages/protocol
  bun install --frozen-lockfile
  bun run build
  bun run test
)

(
  cd apps/decent-app
  bun install --frozen-lockfile
  bun run build
  bun run test
)

echo "release check passed for decent-node v$version"
