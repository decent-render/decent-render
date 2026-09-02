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
