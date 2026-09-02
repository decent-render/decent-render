#!/usr/bin/env bash
# Release consistency gate — runs in CI on every push and locally as step 1
# of RELEASING.md. Judged by exit code.
#
# Why (2026-09-02): v0.0.10 was tagged and released with no CHANGELOG
# section — the bump commit changed Cargo.toml and nothing else, and the
# cargo-dist pipeline cannot gate on it (its custom plan jobs do not block
# `host`, so a failing gate there would ship a release with no artifacts).
# The bump lands on main first, so main is where this is enforced:
#   1. bins/decent/Cargo.toml's version has a `## [X.Y.Z] - YYYY-MM-DD`
#      section in CHANGELOG.md;
#   2. on a tag push, the tag is exactly `v` + that version.
set -u
cd "$(dirname "$0")/.."

version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' bins/decent/Cargo.toml | head -1)
if [ -z "$version" ]; then
  echo "release-gate: could not read version from bins/decent/Cargo.toml" >&2
  exit 1
fi

if ! grep -Eq "^## \[$version\] - [0-9]{4}-[0-9]{2}-[0-9]{2}\$" CHANGELOG.md; then
  echo "release-gate: CHANGELOG.md has no '## [$version] - YYYY-MM-DD' section for the version in bins/decent/Cargo.toml" >&2
  echo "release-gate: move the [Unreleased] entries under a dated [$version] heading in the same commit as the bump" >&2
  exit 1
fi

ref="${GITHUB_REF:-}"
case "$ref" in
  refs/tags/*)
    tag="${ref#refs/tags/}"
    if [ "$tag" != "v$version" ]; then
      echo "release-gate: tag '$tag' does not match bins/decent/Cargo.toml version 'v$version'" >&2
      exit 1
    fi
    ;;
esac

echo "release-gate: OK — $version has a dated CHANGELOG section${ref:+ ($ref)}"
