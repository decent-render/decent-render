# Releasing Decent Render

Releases are integrity events, not only version bumps. A release is complete
when the tagged source, manifest version, generated assets, Homebrew artifact,
and downloaded binary all agree.

## `decent` release

### 1. Prepare

1. Start from a clean, synchronized `main`.
2. Add the user-facing changes under the target version in `CHANGELOG.md`.
3. Update `bins/decent/Cargo.toml`.
4. Run `cargo check -p decent` so `Cargo.lock` records the same version.
5. Run the complete gates from `AGENTS.md`.
6. Run `bash scripts/release-check.sh X.Y.Z`.
7. Commit only the version/changelog/lock changes:
   `chore(node): release vX.Y.Z`.

### 2. Tag and publish

1. Create an annotated tag: `git tag -a vX.Y.Z -m "decent vX.Y.Z"`.
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
  https://github.com/decent-render/decent-render/releases/download/vX.Y.Z/decent-installer.sh \
  | sh

decent --version   # must report X.Y.Z
decent --help
decent status
```

Finally update the Homebrew formula and verify `brew upgrade decent` on an
Apple Silicon machine. The formula version, URL, and SHA-256 must match the
GitHub Release.

cargo-dist auto-generates and pushes `Formula/decent.rb` to the tap on tag
(configured in `dist-workspace.toml` `[dist.homebrew]`). The deprecated
`Formula/decent-node.rb` (compatibility shim) is maintained by the
`update-homebrew-shim.yml` workflow, which runs on release completion.

### `HOMEBREW_TAP_TOKEN` — what it must be (learned the hard way, v0.0.9)

The `publish-homebrew-formula` job pushes to `decent-render/homebrew-tap`,
a **different repo** from this one, so `GITHUB_TOKEN` cannot do it. The
`HOMEBREW_TAP_TOKEN` secret must be a **fine-grained PAT** with:

- **Resource owner: `decent-render`** (the ORG, not a personal account).
  This is fixed at creation and cannot be edited afterwards. A token created
  against a personal account can never see org repos — it shows
  "access to zero repositories" and fails with
  `remote: Permission to decent-render/homebrew-tap.git denied` / HTTP 403.
- Repository access: **Only select repositories → `homebrew-tap`**
- Repository permissions: **Contents: Read and write**. Nothing else.
  (Metadata read-only is automatic.) Not Actions, not Deployments, not
  Secrets — the job only commits and pushes one file, and this secret lives
  in a PUBLIC repo, so extra scope is pure blast radius. For the same reason
  prefer this over a classic `repo`-scope token, which would also reach the
  private `farm-web`.

**Verify the token, not your own login.** Checking `gh api repos/... --jq
.permissions` with your ambient `gh` credentials reports what YOUR ACCOUNT
can do and will happily say `push: true` for a token that has nothing. Ask
GitHub as the token:

```sh
curl -s -H "Authorization: Bearer $TOKEN" \
  https://api.github.com/repos/decent-render/homebrew-tap \
  | python3 -c 'import sys,json; print(json.load(sys.stdin).get("permissions"))'
```

**When the token expires the job fails silently in exactly this way.** The
release itself still succeeds — only the tap goes stale — so `brew install`
quietly serves the previous version. If a release looks fine but
`brew info decent-render/tap/decent` shows the old version, check the token
first.

**Manual fallback** (used for 0.0.8 `5963aa0` and 0.0.9 `869c38f`): the
release publishes the generated formula as an asset, so

```sh
gh release download vX.Y.Z --repo decent-render/decent-render --pattern 'decent.rb' -O /tmp/decent.rb
git clone https://github.com/decent-render/homebrew-tap /tmp/tap
cp /tmp/decent.rb /tmp/tap/Formula/decent.rb
cd /tmp/tap && git commit -m "decent X.Y.Z" -- Formula/decent.rb && git push
```

Then confirm: `brew info decent-render/tap/decent` reports the new version,
and the formula's sha256 equals the release's `.tar.xz.sha256`.

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
