# Releasing Decent Render

Releases are integrity events, not only version bumps. A release is complete
when the tagged source, manifest version, generated assets, Homebrew artifact,
and downloaded binary all agree.

## `decent-node` release

### 1. Prepare

1. Start from a clean, synchronized `main`.
2. Add the user-facing changes under the target version in `CHANGELOG.md`.
3. Update `bins/decent-node/Cargo.toml`.
4. Run `cargo check -p decent-node` so `Cargo.lock` records the same version.
5. Run the complete gates from `AGENTS.md`.
6. Run `bash scripts/release-check.sh X.Y.Z`.
7. Commit only the version/changelog/lock changes:
   `chore(node): release vX.Y.Z`.

### 2. Tag and publish

1. Create an annotated tag: `git tag -a vX.Y.Z -m "decent-node vX.Y.Z"`.
2. Push the release commit, then the single tag.
3. Watch the `Release` cargo-dist workflow to completion.
4. Do not manually upload a partial substitute if the workflow stalls or fails.
   Fix the workflow/runner problem and rerun it.

### 3. Verify the release

The GitHub Release must contain the cargo-dist set, not only a binary archive:

- Apple Silicon tarball
- shell installer
- checksums
- dist manifest
- generated source archive

Then verify from a clean download location:

```sh
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/decent-render/decent-render/releases/download/vX.Y.Z/decent-node-installer.sh \
  | sh

decent-node --version   # must report X.Y.Z
decent-node --help
decent-node status
```

Finally update the Homebrew formula and verify `brew upgrade decent-node` on an
Apple Silicon machine. The formula version, URL, and SHA-256 must match the
GitHub Release.

## Failed release recovery

- A tag with no GitHub Release is incomplete.
- A release with only one manually uploaded archive is incomplete.
- Preserve the historical tag/release record; document the gap in the changelog.
- For the next release, fix/rerun cargo-dist and verify the full asset set.
- Never move an existing published tag to new source.

Historical state at this document's creation:

- `v0.0.2` has a tag but no GitHub Release.
- `v0.0.3` was manually recovered with only the macOS tarball.

Treat v0.0.4 as the first release that must restore the complete automated
release contract.

## `@decent-render/protocol` release

The npm package is independently versioned and published through OIDC trusted
publishing; no npm token exists.

1. Update `packages/protocol/CHANGELOG.md`.
2. Bump `packages/protocol/package.json`.
3. Run its build and conformance tests.
4. Commit and push.
5. Run `.github/workflows/publish-protocol.yml` with the version.
6. Approve the protected `publish` environment.
7. Verify the npm package version and provenance.
8. Install it in a clean consumer and run a minimal schema import.

Changing package version does not change `PROTOCOL_VERSION`. A wire change must
follow the cross-language procedure in `CONTRIBUTING.md`.

## Tauri app

The Tauri app is maintained but is not part of cargo-dist. Do not imply that a
CLI release also distributes or notarizes the desktop app. Define and verify a
separate app release process before publishing a public app artifact.
