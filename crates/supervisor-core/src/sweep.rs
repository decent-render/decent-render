//! Startup sweep of abandoned job workdirs (the SIGKILL backstop).
//!
//! `WorkDir`'s `Drop` purge covers every exit path that runs Rust code —
//! returns, panics, graceful signals. It cannot cover SIGKILL or power loss:
//! there, no `Drop` runs and the workdir (customer render content) stays on
//! the operator's disk forever. This module is the backstop: on supervisor
//! start, remove job workdirs that provably have no live owner.
//!
//! ## Safety rule (why this cannot delete a live sibling's workdir)
//!
//! Two supervisors may run on one machine, and a naive "delete every `job-*`
//! dir" sweep would delete a LIVE sibling's workdir mid-render. The rule here
//! is conservative by construction, in two tiers:
//!
//! 1. **Supervisor workdirs** — named `job-<jobId>-<pid>-<nanos>-<counter>`
//!    by [`WorkDir::new`](crate::purge::WorkDir). The creating supervisor's
//!    pid is embedded in the name. A dir is removed only if that pid is
//!    provably dead (`kill(pid, 0)` → `ESRCH`). A live sibling answers the
//!    probe and is skipped; our own pid is skipped outright.
//! 2. **Runner workdirs** — `mkdtemp("job-<jobId>-XXXXXX")` dirs created by
//!    runner-core; the name carries no owner, so no liveness probe is
//!    possible. These are removed only when the newest file inside is older
//!    than [`STALE_RUNNER_DIR_AGE`] — a threshold far beyond any legitimate
//!    render lifetime in this system, chosen so an in-flight job (which is
//!    minutes old, not days) can never qualify.
//!
//! Every non-match is `Foreign` and never touched. When in doubt the sweep
//! leaks a directory rather than deleting a live job's files: deleting
//! nothing is a bug, deleting a sibling's in-flight workdir is much worse.
//!
//! Residual, accepted hazard (documented, safe direction): if a dead
//! supervisor's pid is reused by an unrelated process, its leftover dirs are
//! skipped until that pid dies again — they leak, they are never wrongly
//! deleted. Clock-failure dirs (`nanos == 0` from `WorkDir::new`) fail the
//! epoch sanity check and then either classify as `Foreign` (typical
//! counters) or fall through to `Runner` (a 6-alphanumeric counter); either
//! way they cannot be swept by pid, and the `Runner` fall-through is still
//! age-gated, so the safe direction holds.

use std::path::Path;
use std::time::{Duration, SystemTime};

/// Runner `mkdtemp` workdirs older than this are considered abandoned.
///
/// No legitimate render outlives this: the supervisor's silence timeout is
/// 120 s, dispatch's stale-assignment cap is minutes-to-hours, and the OS
/// itself (macOS `/tmp` periodic cleanup) is more aggressive than this.
/// Seven days makes "live but silent for a week" impossible in practice
/// while still reclaiming the disk of a hard-killed node.
pub const STALE_RUNNER_DIR_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Minimum plausible epoch-nanoseconds (≈ 2001-09) for the `<nanos>` field of
/// a supervisor workdir name. Anything smaller is a clock failure or a
/// misparse, and misclassification must fail toward `Foreign` (never sweep).
const MIN_EPOCH_NANOS: u64 = 1_000_000_000_000_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Classification {
    /// A supervisor-owned `WorkDir` (`job-<jobId>-<pid>-<nanos>-<ctr>`),
    /// with the creating supervisor's pid.
    Supervisor { owner_pid: u32 },
    /// A runner-core `mkdtemp` dir (`job-<jobId>-XXXXXX`) — no owner in the name.
    Runner,
    /// Not ours. Never touched.
    Foreign,
}

/// Classify a temp-dir entry name. Order matters: the strict supervisor
/// pattern (three trailing numeric fields with a sane epoch-nanos) is tried
/// first; only a failure falls through to the looser runner pattern.
///
/// `WorkDir::new` names are `job-<jobId>-<pid>-<nanos>-<counter>` — at least
/// FIVE dash-separated parts (job ids may themselves contain dashes, adding
/// more). Runner-core dirs are `job-<jobId>-XXXXXX` (mkdtemp suffix).
pub(crate) fn classify_dir_name(name: &str) -> Classification {
    if !name.starts_with("job-") {
        return Classification::Foreign;
    }
    let parts: Vec<&str> = name.split('-').collect();
    // Supervisor pattern: "job" + ≥1 job-id part + pid + nanos + counter.
    if parts.len() >= 5 {
        let (owner_pid, nanos, counter) = (
            parts[parts.len() - 3],
            parts[parts.len() - 2],
            parts[parts.len() - 1],
        );
        if let (Ok(pid), Ok(nanos), Ok(_ctr)) = (
            owner_pid.parse::<u32>(),
            nanos.parse::<u64>(),
            counter.parse::<u64>(),
        ) {
            if pid != 0 && nanos >= MIN_EPOCH_NANOS {
                return Classification::Supervisor { owner_pid: pid };
            }
        }
    }
    // Runner pattern: exactly one trailing mkdtemp suffix, 6 chars of
    // [A-Za-z0-9_], after "job-" + at least one job-id part.
    if parts.len() >= 3 {
        let suffix = parts[parts.len() - 1];
        if suffix.len() == 6
            && suffix
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return Classification::Runner;
        }
    }
    Classification::Foreign
}

/// True if `pid` is provably dead. `EPERM` (exists, not ours) counts as
/// alive — err toward leaking over deleting.
#[cfg(unix)]
pub(crate) fn pid_is_dead(pid: u32) -> bool {
    if pid == std::process::id() {
        return false;
    }
    // SAFETY: kill(2) with signal 0 performs the permission+liveness check
    // only; no signal is delivered.
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if rc == 0 {
        return false;
    }
    matches!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    )
}

/// Non-unix fallback: no liveness probe, so no supervisor dir is ever provably
/// dead — the sweep leaks rather than deletes (released targets are unix, but
/// the crate must still compile elsewhere).
#[cfg(not(unix))]
pub(crate) fn pid_is_dead(_pid: u32) -> bool {
    false
}

/// Newest mtime at or below `path`. Never follows symlinks; an unreadable
/// entry degrades to "older" (sweepable) rather than aborting the walk —
/// if we cannot read it we also could not have written it recently.
fn newest_mtime(path: &Path) -> SystemTime {
    fn walk(p: &Path, newest: &mut SystemTime) {
        let Ok(md) = std::fs::symlink_metadata(p) else {
            return;
        };
        if let Ok(t) = md.modified() {
            if t > *newest {
                *newest = t;
            }
        }
        if md.is_dir() {
            if let Ok(rd) = std::fs::read_dir(p) {
                for entry in rd.flatten() {
                    walk(&entry.path(), newest);
                }
            }
        }
    }
    let mut newest = SystemTime::UNIX_EPOCH;
    walk(path, &mut newest);
    newest
}

/// Remove abandoned job workdirs under `base`, returning how many were
/// removed.
///
/// `base` is injectable so tests run against a scratch dir instead of the
/// real temp dir — a zero age-threshold in a test must never sweep live
/// sibling processes' workdirs on the host. `stale_runner_after` is
/// injectable for the same reason (tests exercise both sides of the age
/// gate without forging mtimes).
pub fn sweep_dir(base: &Path, stale_runner_after: Duration) -> usize {
    let me = std::process::id();
    let now = SystemTime::now();
    let mut removed = 0usize;
    let Ok(rd) = std::fs::read_dir(base) else {
        return 0;
    };
    for entry in rd.flatten() {
        let Some(name) = entry.file_name().into_string().ok() else {
            continue;
        };
        let should_remove = match classify_dir_name(&name) {
            Classification::Supervisor { owner_pid } => {
                // Own dirs are skipped outright (a same-process WorkDir is by
                // definition live), then removed only on proven owner death.
                owner_pid != me && pid_is_dead(owner_pid)
            }
            Classification::Runner => {
                let age = now
                    .duration_since(newest_mtime(&entry.path()))
                    .unwrap_or_default();
                age >= stale_runner_after
            }
            Classification::Foreign => false,
        };
        if !should_remove {
            continue;
        }
        let path = entry.path();
        match std::fs::remove_dir_all(&path) {
            Ok(()) => {
                tracing::info!(path = %path.display(), "swept stale job workdir");
                removed += 1;
            }
            Err(e) => {
                // A failed sweep is reported, never fatal: startup must not
                // die because one abandoned dir had odd permissions.
                tracing::warn!(path = %path.display(), error = %e, "stale workdir sweep failed");
            }
        }
    }
    removed
}

/// Production entry: sweep the OS temp dir with [`STALE_RUNNER_DIR_AGE`],
/// then retry THIS daemon's own recorded purge failures.
///
/// PACKET 37 (audit 4): the pid-liveness rule above can never reclaim a
/// dir THIS process failed to purge — our own pid is alive by definition.
/// The [`crate::purge`] failure list is exactly those dirs, and it is safe
/// to retry them by construction: a path enters the list only when a
/// WorkDir owned by THIS process finished its job and failed all removal
/// retries — there is no live WorkDir for it anywhere (the object is gone;
/// its job ended). A LIVE sibling's workdir never enters this list: the
/// sibling's WorkDir::drop has not run, so it has recorded nothing.
pub fn sweep_stale_workdirs() -> usize {
    let mut removed = sweep_dir(&std::env::temp_dir(), STALE_RUNNER_DIR_AGE);

    // Retry our own failures (bounded list; entries clear on success).
    for failure in crate::purge::outstanding_purge_failures() {
        // Extra safety: never touch a path that classifies as a LIVE
        // supervisor's dir (a foreign pid that is still alive). Our own
        // pid dirs are the expected shape here; anything else is skipped —
        // this list is ours, but defense costs nothing.
        match classify_dir_name(
            failure
                .path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default(),
        ) {
            Classification::Supervisor { owner_pid } => {
                if owner_pid != std::process::id() && !pid_is_dead(owner_pid) {
                    continue; // not provably dead, not ours — skip
                }
            }
            _ => continue, // not a supervisor-shaped name — not from our list
        }
        match std::fs::remove_dir_all(&failure.path) {
            Ok(()) => {
                tracing::info!(
                    path = %failure.path.display(),
                    "reclaimed previously failed purge"
                );
                crate::purge::clear_purge_failure(&failure.path);
                removed += 1;
            }
            Err(e) => {
                tracing::warn!(
                    path = %failure.path.display(),
                    error = %e,
                    "retry of previously failed purge failed again"
                );
            }
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unique-in-time scratch helper: a temp path that is cleaned on drop.
    struct Scratch(std::path::PathBuf);
    impl Scratch {
        fn new(tag: &str) -> Self {
            let p = std::env::temp_dir().join(format!(
                "sweep-test-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            Self(p)
        }
        fn mkdir(&self, name: &str) -> std::path::PathBuf {
            let d = self.0.join(name);
            std::fs::create_dir_all(&d).unwrap();
            d
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).ok();
        }
    }

    #[test]
    fn classifies_supervisor_workdirs() {
        assert_eq!(
            classify_dir_name("job-abc-123-2000000000000000000-0"),
            Classification::Supervisor { owner_pid: 123 }
        );
        // Job ids with dashes and digits stay supervisor dirs.
        assert_eq!(
            classify_dir_name("job-x-1-2-999-2000000000000000000-7"),
            Classification::Supervisor { owner_pid: 999 }
        );
    }

    #[test]
    fn rejects_supervisor_names_with_implausible_nanos() {
        // Clock-failure nanos (0) must not classify as anything sweepable by
        // pid — fail toward Foreign so it is never swept.
        assert_eq!(
            classify_dir_name("job-abc-123-0-0"),
            Classification::Foreign
        );
    }

    #[test]
    fn classifies_runner_mkdtemp_dirs() {
        assert_eq!(classify_dir_name("job-t-abc123"), Classification::Runner);
        assert_eq!(
            classify_dir_name("job-a-b-c-Ab_9zZ"),
            Classification::Runner
        );
        // Wrong suffix length is foreign.
        assert_eq!(classify_dir_name("job-t-abc"), Classification::Foreign);
        assert_eq!(classify_dir_name("job-t-abcdefg"), Classification::Foreign);
    }

    #[test]
    fn foreign_names_are_ignored() {
        assert_eq!(classify_dir_name("notjob-abc123"), Classification::Foreign);
        assert_eq!(
            classify_dir_name("bundle-dl-abc123"),
            Classification::Foreign
        );
        assert_eq!(classify_dir_name("job-"), Classification::Foreign);
    }

    #[cfg(unix)]
    #[test]
    fn runner_dirs_are_age_gated() {
        let s = Scratch::new("age");
        let live_like = s.mkdir("job-age-live-abc123"); // fresh mtime
                                                        // A threshold of MAX never considers anything stale: kept.
        assert_eq!(sweep_dir(&s.0, Duration::MAX), 0);
        assert!(live_like.exists());
        // A threshold of ZERO considers everything stale: removed.
        // (Tests inject the threshold rather than forging mtimes; production
        // uses STALE_RUNNER_DIR_AGE — see receipt.)
        sweep_dir(&s.0, Duration::ZERO);
        assert!(!live_like.exists(), "stale runner dir must be swept");
    }

    #[cfg(unix)]
    #[test]
    fn foreign_dirs_survive_a_zero_threshold_sweep() {
        let s = Scratch::new("foreign");
        let dir = s.mkdir("job-foreign-short-abc"); // 3-char suffix → Foreign
        sweep_dir(&s.0, Duration::ZERO);
        assert!(dir.exists(), "foreign dirs must never be swept");
    }

    #[cfg(unix)]
    #[test]
    fn supervisor_dirs_follow_owner_liveness() {
        // A real short-lived process: while alive its dir survives even a
        // zero runner threshold; once dead (killed AND reaped) it is swept.
        let s = Scratch::new("liveness");
        let mut owner = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("sleep available on unix");
        let pid = owner.id();
        let dir = s.mkdir(&format!("job-liveness-{pid}-2000000000000000000-0"));

        sweep_dir(&s.0, Duration::MAX);
        assert!(dir.exists(), "live owner's workdir must be kept");

        owner.kill().unwrap();
        owner.wait().unwrap(); // reap, or kill(pid,0) still answers
        sweep_dir(&s.0, Duration::MAX);
        assert!(!dir.exists(), "dead owner's workdir must be swept");
    }

    #[cfg(unix)]
    #[test]
    fn own_workdirs_are_never_swept() {
        // Same-process WorkDirs are live by definition — the sweep must not
        // touch them even with a zero threshold and a dead-looking... well,
        // our own pid is never probed. This guards the `owner_pid != me`
        // clause directly.
        let s = Scratch::new("own");
        let me = std::process::id();
        let dir = s.mkdir(&format!("job-own-{me}-2000000000000000000-0"));
        sweep_dir(&s.0, Duration::ZERO);
        assert!(dir.exists(), "own-pid workdir must never be swept");
    }
}
