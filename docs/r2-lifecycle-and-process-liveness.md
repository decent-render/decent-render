# Research report — R2 orphan lifecycle & process liveness in containers

Packet: `/tmp/decent-render-research-r2-and-liveness.md` (research packet 2)
Date: 2026-08-22 UTC. All local experiments run in Docker (`python:3.12-alpine`, default
settings unless noted) on this machine; scratch in `/tmp/zombie-demo/`. No repository edits, no
R2/Cloudflare changes, `~/.decent-worker` untouched.

---

## Executive summary

**Q1 — Can R2 lifecycle rules solve orphaned outputs without code? Mostly no — but they are a
genuine safety net.** R2 *does* support bucket lifecycle rules with **prefix** scoping and
age-based deletion, configurable via wrangler / S3 API / dashboard / Terraform, and the native
API expresses age in **seconds** (the Terraform example is literally "expire older than 24
hours"). Deletion happens "typically within 24 hours" of the computed expiration. **But the
tagging idea is dead on arrival: R2 does not implement object tagging at all** (no
Get/Put/DeleteObjectTagging, no `x-amz-tagging` header on uploads, and the lifecycle rule model
has no tag condition — prefix only). So a prefix convention (`pending/…`) plus a lifecycle rule
CAN make orphans self-cleaning, but because R2 has no atomic rename, "settling" an object means
copy+delete through the final prefix — extra Class A ops and a doubled-storage window. It also
cannot delete an orphan *immediately* on failed settlement; the best case is maxAge + up to
24 h. Recommendation shape: keep the DB-keyed sweep (or refuse-upload-after-cancel) as the
semantic fix; optionally add a coarse `pending/` lifecycle rule (e.g. 7 days) as a backstop.

**Q2 — Correct liveness on Linux.** `kill(pid, 0)` is not a liveness check — it is an
existence check, and **zombies exist** (reproduced under a non-reaping PID 1: zombie answers
`kill(pid,0) == 0` and `/proc/<pid>/stat` shows `Z`). Two corrections/refinements to the
packet's model, both experimentally proven:
1. **`--init`/tini does NOT fix zombies whose parent is still alive** — no init can reap
   another process's child. It only reaps *re-parented orphans*.
2. **A supervisor running as PID 1 without waitpid leaks zombies all by itself** — there is no
   kernel auto-reap for a PID-namespace init's direct children.

The portable idioms: if you are the parent → `waitpid` (via SIGCHLD handler / `WNOHANG` loop);
if you are not the parent → read `/proc/<pid>/stat` **field 3** (state `Z` ⇒ dead, parsing
after the *last* `)` because `comm` can contain spaces) or, on Linux ≥ 5.3, the race-free
answer: **`pidfd_open` + poll** — demonstrated live: the pidfd becomes readable the moment the
process dies *even while the zombie still fools `kill(pid,0)`*. Rust: `rustix::process::pidfd_open`
(verified on docs.rs), or plain `libc::waitpid`/`nix`. Runtime defaults that leave zombies:
Docker without `--init` (yes), **Kubernetes per-container PID namespaces (the default — pause
is NOT your container's PID 1)**, ECS without `initProcessEnabled`. Fly machines inject a
non-disableable `/init` PID 1 that reaps orphans (dispatch's own runtime, worth knowing).

---

## Q1 — R2 object lifecycle rules **[VERIFIED: capabilities]**

### What R2 supports (all primary-source verified)

| Capability | Status | Evidence |
|---|---|---|
| Lifecycle delete-by-age rules | ✅ `Expiration: {Days: N}` | R2 lifecycle docs, S3 API example |
| Prefix scoping | ✅ `Filter: {Prefix: "logs/"}` / rule-level `conditions.prefix` | R2 docs + CF API schema |
| Tag-based scoping | ❌ **does not exist** | CF API schema: rule conditions = `{prefix}` only |
| Object tags at all | ❌ **not implemented** | S3 compat table: `GetObjectTagging`/`PutObjectTagging`/`DeleteObjectTagging` ❌, `x-amz-tagging` on PutObject/CreateMultipartUpload/CopyObject ❌; cloudflare-docs PR #10255 "Mark object tagging APIs as unsupported" |
| Date-based deletion | ✅ `Expiration: {Date}` | docs example |
| Storage-class transition | ✅ (irrelevant here; incurs a Class A op) | docs |
| Abort incomplete multipart | ✅ (default rule: 7 days) | docs |
| Config via dashboard | ✅ | docs |
| Config via wrangler | ✅ `r2 bucket lifecycle add/set/list/remove` (set takes JSON of the CF API body) | docs |
| Config via S3 API | ✅ `PutBucketLifecycleConfiguration` / `GetBucketLifecycleConfiguration` (implemented; note `GetBucketLifecycle` v1-style is NOT) | docs + S3 compat table |
| Config via Terraform | ✅ `cloudflare_r2_bucket_lifecycle` resource | registry.terraform.io schema (fields: rules[].conditions.prefix, delete_objects_transition.condition{max_age|date,type}) |
| Rule limit | 1000 per bucket | docs |

Sources: developers.cloudflare.com/r2/buckets/object-lifecycles/ (page dated 2026-04-21),
developers.cloudflare.com/api/resources/r2/subresources/buckets/subresources/lifecycle/,
developers.cloudflare.com/r2/api/s3/api/ (dated 2026-07-31),
registry.terraform.io/providers/cloudflare/cloudflare/latest/docs/resources/r2_bucket_lifecycle.

### Age granularity and how fast deletion actually happens **[VERIFIED]**

- The **Cloudflare-native API/wrangler/Terraform express age in SECONDS** (`maxAge: number`,
  "Condition for lifecycle transitions to apply after an object reaches an age in seconds" —
  CF API schema). The Terraform provider's own example rule is `"Expire all objects older than
  24 hours"` → `max_age = 86400`.
- The **S3 API expresses whole days** (`Expiration.Days` — same shape as AWS S3, where the SDK
  validates ≥ 1 day).
- **Deletion timing:** "Objects will typically be removed from a bucket within 24 hours of the
  `x-amz-expiration` value." New uploads immediately reflect the rule in their
  `x-amz-expiration` (visible via HeadObject); *existing* objects may lag ("Most objects will
  be transitioned within 24 hours but may take longer depending on the number of objects").

**Practical floor:** even if the native API accepted `max_age = 60` seconds, effective deletion
is maxAge + up-to-24h, on a daily-ish sweep cadence. Design any orphan TTL with that ~1-day
slop in mind. (The *enforced minimum* maxAge value is not documented — see "could not
determine".)

### The "tag as unsettled, untag on settle" idea — impossible **[VERIFIED]**

R2 has no object tagging whatsoever (API table + docs PR), and the lifecycle rule model has no
tag condition even if it did. The closest legitimate equivalents:
- **Prefix convention** (the real option — below), or
- **Object metadata** (`x-amz-meta-*` on PutObject is supported) — but lifecycle rules cannot
  key off metadata, so it's only useful for a sweep's filtering, which is code anyway.

### Can a prefix convention + lifecycle rule make orphans self-cleaning? Yes, with real tradeoffs **[VERIFIED capability / INFERRED design]**

Design sketch (the only shape that works given no rename and no tags):

1. Dispatch presigns uploads under a **staging prefix**, e.g. `renders/pending/<jobId>.mp4`.
2. One lifecycle rule: prefix `renders/pending/`, delete at `max_age` = N days (e.g. 7).
3. On settlement, dispatch **copies** the object to the final key
   (`renders/done/<jobId>.mp4`) and deletes the pending copy (CopyObject + DeleteObject are
   both supported). The customer-facing URL is only ever the final key.

Tradeoffs vs a DB-keyed sweep or refuse-after-cancel:

| | lifecycle backstop | DB sweep | refuse upload after cancel |
|---|---|---|---|
| Orphan retention | N days + ≤24h | next sweep run | zero (nothing uploaded) |
| Precision | coarse (whole prefix) | exact (outputKey) | exact |
| Cost | one CopyObject (Class A) + transient double storage per settled render | ListObjects/DeleteObjects on schedule | none |
| Failure coupling | storage-layer, works even if dispatch DB lies | requires dispatch up | requires runner policy |
| Settled-object risk | **rule misconfig can delete good renders** (prefix must be exactly the staging prefix) | none | none |
| Auditable? | deletion is silent (no event unless R2 event notifications configured) | you log each decision | you log each refusal |

The lifecycle rule is a **good safety net and a bad primary mechanism**: it protects against
"orphan invisible to any query" forever (the current defect) at the cost of a copy per render.
Given the runner already knows cancel state before upload, option (b) (refuse upload after
cancel) plus a **long, dumb** pending-prefix rule as belt-and-braces is the combination I'd
take — and the sweep (a) remains the right tool if you also want *immediate* cleanup of the
already-accumulated orphans.

Cost note **[INFERRED]**: the docs list no charge for lifecycle rules themselves; IA
transitions incur Class A ops; deletion pricing wasn't separately verified here — check the R2
pricing page before relying on "deletes are free".

---

## Q2 — Process liveness inside containers

### The defect is real, and here is its exact shape **[VERIFIED — executed]**

Experiment matrix (all in Docker, python:3.12-alpine, scripts in `/tmp/zombie-demo/`):

| # | Setup | Result |
|---|---|---|
| E1 | `sh` PID 1 (no `--init`), parent alive, kills child, never waits | child is **Z**; `kill(pid,0)` **succeeds** (supervisor reads dead as alive) |
| E2 | **`--init`** (tini PID 1), same live non-reaping parent | child is **still Z**; `kill(pid,0)` still succeeds — **`--init` does not fix this case** |
| E3a | no `--init`, parent dies, orphan re-parented to `sh` PID 1, killed | **Z persists** (the packet's finding) |
| E3b | `--init`, orphan re-parented to tini, killed | **reaped instantly**, pid gone (packet's finding) |
| E4 | python **is** PID 1, kills its own child, never waits | **Z persists** — no kernel auto-reap for a PID-ns init's direct children |
| P | pidfd demo: `pidfd_open(pid)` → kill → poll | **pidfd readable (dead) while `kill(pid,0)` still says alive and state is Z** |

So the correction to the packet's model: `--init`/tini (and any reaping init, including
Kubernetes pause and Fly's `/init`) only reaps **re-parented orphans**. If the supervisor is
the zombie's *parent* — which is exactly the case when the supervisor spawned the child —
**only the supervisor's own `waitpid` can clear it**, on every runtime. This makes the fix
mandatory in the supervisor regardless of container runtime policy.

### Correct portable liveness idioms **[VERIFIED]**

In order of correctness for a Rust supervisor:

1. **If you spawned it: `waitpid`.** The zombie is your child; `waitpid(pid, &status, WNOHANG)`
   in a SIGCHLD handler or poll loop both reaps and gives you the truth. This is the only
   complete fix for the supervisor's own children (E2/E4). `libc::waitpid` or `nix`.
2. **If you didn't spawn it, on Linux ≥ 5.3: `pidfd_open` + poll** (demo P). Open the pidfd
   *when you first observe the pid*; the fd pins the process identity, so it is immune to PID
   reuse, and becomes readable (POLLIN) at process exit **even while unreaped as a zombie**.
   Rust: `rustix::process::pidfd_open(pid, PidfdFlags::empty())` — verified present in current
   rustix (docs.rs, feature `process`).
3. **Portable fallback: `/proc/<pid>/stat` field 3.** State `Z` ⇒ treat as dead. Verified
   semantics via proc_pid_stat(5): field (2) is `comm` **in parentheses and may contain
   spaces** (truncated at 16 chars) — so parse everything **after the last `)`** before
   splitting fields; field (3) is the state char. Combined with `kill(pid,0)`:
   - ENOENT `/proc/<pid>` ⇒ gone (reaped/dead).
   - state `Z` ⇒ dead-but-unreaped.
   - state R/S/D/I ⇒ alive.
   Gotchas: PID **reuse** (a pid can be Z and then re-used — pidfd is the only race-free
   answer); permissions (same-uid or CAP; reading another user's stat can EACCES); `/proc` not
   mounted is rare in containers but possible in minimal sandboxes — handle ENOENT-by-mount as
   "unknown", not "dead".

What `kill(pid, 0)` actually tells you, per POSIX and E1: the pid **exists** and you may signal
it. Zombies qualify. It is an existence check, not a liveness check.

### Do the common runtimes reap? **[VERIFIED]**

| Runtime | Reaps by default? | Detail |
|---|---|---|
| `docker run` (no `--init`) | ❌ | Your entrypoint is PID 1 of the container's PID ns; zombies accumulate if it doesn't wait (E1/E3a). |
| `docker run --init` | ⚠️ orphans only | docker-init (tini) as PID 1 reaps re-parented orphans (E3b) — **not** children of your still-alive processes (E2). |
| Kubernetes (default) | ❌ | With containerd/CRI each container gets its **own** PID namespace: your entrypoint is PID 1; the pod-level pause container is NOT in your ns. `shareProcessNamespace: true` (opt-in) makes the pod share one ns whose init is pause (it has a SIGCHLD→`waitpid(-1,…,WNOHANG)` reaper). k8s has **no** `--init` equivalent — open feature request kubernetes#84210 since 2019. Zombie accumulation with exec probes is a well-documented production failure (k8s #81042, containerd #5153). |
| ECS / Fargate | ⚠️ opt-in | Container-def `linuxParameters.initProcessEnabled: true` "Run an init process inside the container that forwards signals and reaps processes. Maps to the `--init` option" (AWS task-definition docs). Off by default. `pidMode: task` shares one PID ns per task. |
| Fly.io machines | ✅ orphans, always | Official docs: Fly injects a runtime `/init` as PID 1 "reaping orphaned child processes (PID 1 responsibilities), forwarding signals…". "You don't need tini, dumb-init, or s6-overlay… You can't disable or replace Fly's init." Your app runs as a non-PID-1 child (default mode; in the newer containers/Pilot mode your process IS PID 1). Relevant to decent-render's dispatch on Fly — but again: orphans only. |

Sources: AWS ECS task-definition parameters (initProcessEnabled, pidMode);
fly.io/docs/getting-started/troubleshooting ("The init process"); kubernetes PR #36853 (pause
SIGCHLD reaper), issues #84210, #81042, #50865; community.fly.io threads confirming PID-1
behavior; local experiments for the Docker rows.

### If the supervisor IS PID 1 **[VERIFIED behavior / INFERRED one pointer]**

- It inherits full init responsibilities: as E4 proves, **its own direct children are NOT
  auto-reaped by the kernel** — it must `waitpid` (SIGCHLD handler + WNOHANG loop; or a
  blocking `waitpid(-1)` thread).
- Orphaned descendants anywhere in the container get re-parented **to it**, so its reaper also
  becomes the container-wide garbage collector.
- If it can't be PID 1 but wants to adopt orphans, `prctl(PR_SET_CHILD_SUBREAPER, 1)` makes it
  a subreaper (this is tini's `-s` mode). *(Pointer from tini's docs/practice — not
  independently re-verified here.)*
- Extra PID-1 gotcha worth knowing: the kernel ignores **default** signal dispositions for PID
  1 — a SIGTERM handler must be installed explicitly or `kill -TERM 1` does nothing
  *(standard, documented kernel behavior; cited from knowledge, not fetched — verify against
  signal(7) when implementing)*.

### Rust crates **[VERIFIED for rustix; others from established docs]**

- `rustix::process::pidfd_open` → `OwnedFd` — verified current docs.rs.
- `libc` / `nix` — `waitpid`, `kill`, `sigaction`/`signal_sa`: the classic path, fully
  supported. (`nix` also gained a pidfd module in recent releases; the docs.rs URL 404'd for
  the shape I tried — verify the exact module on the version you pin.)
- `procfs` crate — typed `/proc/<pid>/stat` parsing if you go that route.

---

## What I could not determine

- **R2 minimum enforceable `maxAge`** (seconds) — the API schema allows a number; no doc
  states a floor (S3-side it's 1 day). If you build the pending-prefix backstop with a short
  TTL, validate empirically in a scratch bucket first (not done here — no bucket writes
  allowed).
- **Exact deletion latency distribution** — Cloudflare only commits to "typically within 24
  hours"; no SLA figure is published.
- **R2 deletion/pricing specifics** (are lifecycle deletes billed?) — not fetched; check the
  R2 pricing page.
- **Whether `x-amz-expiration` is surfaced on R2 HeadObject responses** (it is on S3) —
  plausible but unverified; useful as a rule-verification probe.
- **nix crate pidfd module name/API on the latest version** — docs.rs fetch failed; rustix is
  verified and sufficient.
- **PID-1 signal-disposition kernel special case** — asserted from well-known documented
  behavior (signal(7)); the man page was not fetched in this pass.

## Artifacts

- `/tmp/zombie-demo/` — experiment scripts (exp_parent_kills.py, exp_orphan.py,
  inspect.py, exp_pidfd.py) and their outputs as captured above.
- Docker image used: `python:3.12-alpine` (fresh pull).

DECENT_RESEARCH_R2_DONE
