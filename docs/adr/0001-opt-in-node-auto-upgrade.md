# ADR 0001 — Opt-in node auto-upgrade

- **Status:** Proposed; Phase 1 and non-durable Phase 2 implemented in working trees, Commit 1 schema/migration generated, none released/deployed/applied
- **Date:** 2026-09-04
- **Decider:** Ray
- **Public scope:** `bins/decent`, `crates/supervisor-core`
- **Private scope:** `farm-web/apps/dispatch` rollout controls and alerting

---

## Decision summary

Unattended supervisor upgrades are **off by default** and explicitly enabled per
machine with `decent auto-upgrade on`.

The connection core—not a status-file poller—owns the safe handoff. After
dispatch announces `updateAvailable`, the core waits for 15 **continuously idle**
minutes. At the boundary it closes the WebSocket from the same event loop that
owns job acceptance, joins completed teardown handles, and returns
`ConnectionExit::AutoUpgrade`. Only then does the installed daemon run its
existing package-manager path. A successful on-disk change makes the daemon exit
cleanly; launchd `KeepAlive` / systemd `Restart=always` loads the new binary.

The node never installs bytes supplied by dispatch. macOS continues to trust the
Homebrew tap; Linux continues to trust the cargo-dist release installer.

## Why this decision exists

Today dispatch already sends one `updateAvailable` frame per outdated connection,
and `decent status` shows the manual-upgrade banner. Community nodes will still
age indefinitely if a human must run `decent upgrade` on every machine. At the
same time, a naive timer or restart can interrupt tenant work or brick an
unattended machine.

The design therefore has five non-negotiable properties:

1. **Opt-in only.** Absent flag = today's manual-only behavior.
2. **Never cancel a render for an upgrade.** Upgrade eligibility is decided in
   the job-owning event loop, not from a racy external snapshot.
3. **No assignment during the package-manager window.** The socket closes first.
4. **No repeated failure loop.** An attempt is recorded durably before it starts;
   started/failed/no-op targets are suppressed for 24 hours.
5. **Dispatch is notification, not a code-delivery channel.**

## Review of the original proposal

The first draft proposed a detached child that would swap the binary and restart
the daemon. A source review and launchd experiment found the following problems:

1. **Critical: assignment race.** The child could run `brew upgrade` while the
   parent daemon remained connected and eligible. A job could arrive during the
   swap. Saying “pause first” did not specify an atomic relationship with the
   connection loop.
2. **The child was unnecessary.** Once the core performs a structural idle close,
   the old daemon can safely update its own file in-process; its mapped executable
   remains valid. A clean exit then lets the existing service manager reload it.
3. **A local `update-channel` file cannot control dispatch without wire or
   server-side identity policy.** The original Phase 2 “no protocol change” claim
   was false as written. Canary selection will initially be a server-side worker
   allowlist; a node-selected channel would require a protocol field.
4. **The XDG claim was false for the current CLI.** `token_path()` uses
   `$HOME/.config/decent`; only the Linux unit location honors
   `XDG_CONFIG_HOME`. Phase 1 deliberately colocates the flag with the token and
   does not silently move credentials. A separate migration is required before
   claiming XDG support.
5. **Unbounded package-manager processes were unacceptable.** A stuck brew lock,
   curl, or installer would leave the node offline forever. Upgrade subprocesses
   now run in their own process group with a 10-minute wall-clock bound and
   TERM→5-second-grace→KILL cleanup.

These findings supersede the detached-child mechanics in the original draft.
The launchd experiment remains useful evidence for process-group behavior and
for the bounded subprocess implementation.

## Phase 1 — node implementation

### Operator contract

```sh
decent auto-upgrade on      # explicit opt-in
decent auto-upgrade status  # flag + last attempt
decent auto-upgrade off     # default; does not interrupt a transaction already running
```

- Flag: `~/.config/decent/auto-upgrade`, mode 0600; parent mode 0700.
- Attempt ledger: `~/.config/decent/auto-upgrade-state.json`, mode 0600, atomic
  temp-write + rename.
- A running installed daemon notices flag changes within three seconds.
- Foreground `decent start` remains manual-only because no service manager is
  guaranteed to relaunch it.
- `decent status` exposes the live flag and the last attempt.

### Structural idle gate

`ConnectionConfig::auto_upgrade` enables a pure `AutoUpgradeGate` inside
`run_session`.

- A new target starts an idle window.
- Accepting any job resets the window synchronously in the job-accept arm. This
  matters even for jobs shorter than the 250 ms maintenance tick.
- Repeated notices for the same target do not reset the window.
- Turning the opt-in off or observing a live job/teardown resets the window.
- At 15 continuous idle minutes, the auto-upgrade tick is biased ahead of incoming
  frames. The socket closes before another assignment can be accepted.
- `run()` keeps its existing public `Result<()>` API. The CLI uses the additive
  `run_until_exit()` API to receive `ConnectionExit::AutoUpgrade { version }`.

### Attempt and failure behavior

1. Persist `Started` before invoking a package manager. If persistence fails, do
   not upgrade; reconnect.
2. macOS: refresh the tap, then bounded `brew upgrade decent`.
3. Linux: bounded cargo-dist installer; outer process-group timeout plus curl
   connect/transfer limits.
4. Verify the on-disk binary actually changed using the existing N-28/N-29 logic.
5. `Upgraded`: persist result and exit cleanly for service-manager reload.
6. `AlreadyCurrent` or error: persist result, suppress that target for 24 hours,
   reconnect in the same old process.
7. A leftover `Started` record (process/machine died mid-attempt) also suppresses
   for 24 hours—the safe response to uncertainty is not an immediate retry.

## Phase 2 — dispatch rollout safety (implemented, not deployed)

This must deploy before enabling auto-upgrade on a node nobody can physically
reach.

1. **Canary targeting:** `DISPATCH_UPDATE_CANARY_WORKER_IDS` is a server-side
   allowlist. Canary nodes receive the tap/emergency-pin version; stable nodes
   receive `DISPATCH_STABLE_SUPERVISOR`. Setting canaries without a valid exact
   stable semver fails dispatch startup rather than falling through to all-fleet.
2. **Upgrade observation:** in-memory bounded tracking remembers
   `{workerId, fromVersion, targetVersion, notifiedAt}` after a frame is actually
   sent. Exact-or-newer parseable registration resolves it; old/malformed versions
   never produce false success.
3. **Dark-node alarm:** a notified node that disconnects and remains absent for 15
   minutes produces an alert log and increments aggregate missing/alerted state.
4. **Health visibility:** public `/health/version` exposes release targets only.
   Authenticated `/internal/supervisor-rollout` exposes pending/missing/alerted
   counts. Worker identities remain in dispatch logs, not HTTP responses.
5. **Known limit:** the current working-tree tracker resets on dispatch restart.
   The replacement durable design is specified in
   `farm-web/docs/supervisor-rollout-ledger-design-2026-09-04.md` and is planned
   before any user onboarding.

## Phase 2b — durable rollout control plane (planned, testing only)

Because the farm currently has zero users and no legacy fleet contract, we can
make the durable ledger mandatory before launch rather than preserving the
in-memory behavior as a compatibility path.

The planned implementation is a campaign table, per-worker projection, and
append-only campaign audit table, controlled through an authenticated admin API.
`homebrew-latest` is resolved once at campaign creation and persisted as an
exact version; promotion always uses that exact version and never re-reads the
tap. The web app authenticates farm sessions and `super_admin`; dispatch keeps
its server-to-server internal-secret boundary.

Commit 1 (schema + migration generation), Commit 2 (the injected durable
repository/store with hermetic tests), and Commit 3 (authenticated admin
control API) are now present in the working tree. The migration has not been
applied to production and dispatcher notification wiring remains a deliberate
later commit.

The full schema, endpoint contract, transition rules, crash semantics, test
matrix, and migration order are specified in the farm-web design document
linked above. No runtime code or migration is implied by this ADR section yet.

## Phase 3 — required-upgrade policy (later)

Only after Phase 1 is released and Phase 2 is proven:

- Add an additive `maintenance`/`draining` state to the wire if dispatch must keep
  a node visible while withholding assignments. Rust schema, TypeScript schema,
  and `packages/protocol/fixtures/v2.json` move together.
- Add `minSupervisorVersion` selection/connect enforcement. Do not make an old
  node unusable until the self-serve upgrade path is released and witnessed.

## Phase 4 — rollback (documented, not implemented)

Rollback is a new explicit campaign targeting an exact version from the same
Homebrew/cargo-dist trust chain. It never mutates the original campaign, never
pushes binaries from dispatch, and never auto-rolls back solely because a node
is dark. A rollback campaign requires a canary and explicit promotion just like
an upgrade campaign.

The detailed future design—including strict downgrade completion semantics,
parent-campaign linkage, audit requirements, and the protocol evidence needed
for automatic rollback—is in
`farm-web/docs/supervisor-rollout-ledger-design-2026-09-04.md`.

Automatic rollback remains evidence-gated. It requires protocol-level attempt
markers, durable health criteria beyond reconnect, a tested downgrade path,
independent human alerting, and an operator disable switch. Until then,
rollback is manual campaign creation plus observation.

## Verification performed

### launchd process-group experiment — 2026-09-04

Sandbox label `com.decent-render.adr-verify`; production
`com.decent-render.decent` remained loaded and untouched. Test plist matched
production's `RunAtLoad`, `KeepAlive=true`, and `ExitTimeOut=30`.

| Subject | Process group | Result of `launchctl kickstart -k` |
|---|---|---|
| Test daemon 61999 | own | TERM; relaunched as 62392 |
| `setsid` child 62009 | own | survived and continued writing |
| plain child 62010 | daemon's | died at the exact kick second |

A clean exit 0 was relaunched by launchd in the same second (62392 → 63404).
The sandbox was booted out and removed; zero test agents remained.

### Source verification — systemd

The generated user unit has `Restart=always`, `RestartSec=10`, and no
`KillMode`, therefore systemd's `control-group` default applies. Clean daemon
exit reloads the binary after at most the configured ten-second delay. A live
Linux execution witness is still required before Linux fleet rollout.

### Automated gates currently passing

- Node config: opt-in default/off/on/off, 0600 file and 0700 directory.
- Attempt ledger: started/failed/rejected suppression, exact 24-hour boundary,
  expiry inside a long-lived connection, successful upgrade not suppressed.
- Core gate: continuous-idle requirement, busy/disabled reset, repeated notice,
  suppression, later-target eligibility.
- WebSocket integration: update notice → idle gate → close frame → exact
  `AutoUpgrade` process outcome.
- Bounded subprocess test: stuck child process group is terminated.
- `cargo test -p supervisor-core -p decent`: 223 tests passing as of this change.
- Focused Clippy with warnings denied: passing.
- Farm control plane: 488 tests passing; full web+dispatch TypeScript check and
  production build passing.

## Risk and open-issue register

| # | Risk / decision | Current treatment | Next proof/action |
|---|---|---|---|
| R1 | Assignment arrives during upgrade | Closed structurally: core closes first from job-owning event loop | Keep integration regression test |
| R2 | Very short job occurs between maintenance ticks | Job-accept arm synchronously resets gate | Keep pure gate test |
| R3 | Repeated notice causes package-manager loop | Same target does not reset timer; durable 24h suppression expires automatically in a long-lived connection | Test process-level failure/reconnect end-to-end |
| R4 | Package manager hangs | 10m process-group bound; TERM then KILL | Failure-inject real brew shim in sandbox |
| R5 | SIGTERM/reboot arrives during package-manager transaction | Signal is observed, but launchd/systemd may hard-kill after their 30s stop bound; package manager transactional guarantees are relied on | Decide whether separate helper service is justified before broad rollout |
| R6 | Binary changed but final result ledger write fails | Exit/reload anyway; new version registration is source of truth; log error | Dispatch telemetry must resolve by observed version |
| R7 | Dispatch target is malformed/stale | Exact semver validation; malformed is rejected, equal/older ignored and suppressed; package channel remains source of truth | Closed in Phase 1 tests |
| R8 | Stable/canary policy from node-local flag | Rejected: dispatch cannot see it | Implement server-side allowlist; protocol later if operator-selectable channels matter |
| R9 | Dispatch restarts and forgets rollout alarms | In-memory tracker insufficient | Durable DB record requires migration review |
| R10 | Linux clean-exit behavior only source-verified | Unit says `Restart=always`; no live witness | Run sandbox on first Linux node |
| R11 | Homebrew install path changes on upgrade | Production plist points at stable `/opt/homebrew/bin/decent` symlink; verified on this machine | Pin service-unit regression test to stable opt path behavior |
| R12 | XDG config inconsistency | Phase 1 follows existing token directory | Separate credential migration; do not silently change token path |
| R13 | Operator disables during transaction | Command documents that already-started transaction is not canceled | Keep; canceling mid-swap is less safe |
| R14 | Node below future minimum version | No enforcement yet | Phase 3 only after upgrade path witness |
| R15 | Bad release starts but fails health after restart | Detection in Phase 2; rollback remains manual | Decide Phase 4 from evidence |

## Ordered next work

1. Add the remaining process-level harness that composes the already-tested
   idle-close and injected apply halves, proving successful exit and failure
   reconnect in one hermetic daemon test.
2. Decide R5: accept package-manager transactional recovery for the Air canary or
   introduce a separate launchd/systemd upgrade-helper service before wider use.
3. Wire the durable repository into dispatch notification/alert paths from
   `farm-web/docs/supervisor-rollout-ledger-design-2026-09-04.md`; Commit 1
   schema/migration, Commit 2 store, Commit 3 admin API, and Commit 4 dispatcher
   integration are complete locally. Run the Drizzle/Turso migration procedure
   before touching production schema.
4. Review both repository diffs, then version/release the node and deploy dispatch
   through their separate human-gated workflows.
5. Manually witness banner + manual upgrade first; configure stable pin + Air
   worker-id canary; only then enable auto-upgrade on the physically reachable Air.
6. Witness one subsequent release end-to-end, including internal rollout counts.
7. Run the service-manager witness on Linux before any Linux opt-in.

No deploy, migration, version bump, tag, push, or fleet opt-in is authorized by
this ADR.
