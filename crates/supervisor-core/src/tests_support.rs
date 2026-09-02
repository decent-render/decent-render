//! Test-only cleanup guards shared across this crate's test modules.
//!
//! One definition (BACKLOG N-24): every guard the tests use lives here so
//! the drop semantics are identical everywhere. Best-effort by design — a
//! failed cleanup must never fail a test.

use std::path::PathBuf;

/// Directory guard: recursively removes the directory on drop (best-effort).
/// Bind it right after creating a per-test temp dir; the removal happens
/// when the binding leaves scope (after the assertions).
pub(crate) struct RemoveDirOnDrop(pub PathBuf);

impl Drop for RemoveDirOnDrop {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

/// File guard: removes the file on drop (best-effort). Because Drop runs
/// after the test's assertions, evidence markers can still be inspected
/// during the test. (History: three group-kill tests wrote
/// `<tmp>/<job>.gc-pid` and never removed it — 1,070 stale files after a
/// day of cargo runs, 2026-09-02 — which is why this guard exists.)
pub(crate) struct RemoveOnDrop(pub PathBuf);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        std::fs::remove_file(&self.0).ok();
    }
}

/// Process-tree tests (spawn real children, measure TERM→KILL timing) hold
/// this for their whole body so they never overlap each other or a
/// network-heavy test. A tokio Mutex (not std) so async tests can
/// `lock().await` across their own runtimes; sync tests use
/// `blocking_lock()`.
pub(crate) static PROCESS_TREE_TESTS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[test]
fn process_tree_tests_lock_is_usable_when_nothing_holds_it() {
    // Contention-tolerant on purpose: under default test parallelism a
    // process-tree test may legitimately hold the lock at this exact
    // moment — a contended try_lock is not a failure. The only thing this
    // trivial test pins is that the static compiles and can be locked.
    if crate::tests_support::PROCESS_TREE_TESTS.try_lock().is_err() {
        eprintln!("lock contended by a concurrent process-tree test — fine");
    }
}
