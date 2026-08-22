//! Per-job sleep assertions (packet 18) — a sleeping node must not
//! silently kill renders.
//!
//! Audit finding: no power assertion existed anywhere. A macOS node that
//! idle-sleeps mid-render suspends the job (not kills it), and dispatch
//! hard-fails it ~10 minutes later while the operator never learns why.
//! The one prior real-world node loss (farm-web air-recovery-2026-07-12)
//! was exactly this.
//!
//! ## Mechanism (macOS): a caffeinate child, per job
//!
//! [`JobKeepAwake::acquire`] spawns `/usr/bin/caffeinate -i` (assert
//! "prevent idle sleep" only) as a direct child of the SUPERVISOR, and
//! [`JobKeepAwake::Drop`] kills it and waits for the exit. Chosen over an
//! IOKit `IOPMAssertionCreateWithName` FFI call:
//!
//! - No new dependencies and no unsafe FFI surface. The candidate crates
//!   bind the full IOKit API; hand-rolled FFI for one call is a bigger
//!   liability than a well-understood system binary.
//! - caffeinate IS the IOKit assertion wrapped in a process: its exit (or
//!   kill) releases the assertion deterministically. No assertion-id
//!   bookkeeping, no leak window if the supervisor is SIGKILLed — the
//!   kernel reaps the child and the assertion with it.
//! - Process-management noise is one pid per RENDERING node at a time
//!   (held only while a job is in flight — never while idle, per the
//!   brief), and Drop-based cleanup rides the same teardown discipline
//!   as `WorkDir`.
//!
//! ## Process-group placement (load-bearing, see runner.rs ITEM 1)
//!
//! The RUNNER is spawned with `process_group(0)` — a NEW process group
//! whose pgid equals the runner pid — so a cancel's group TERM reaches
//! Chrome. The caffeinate child must NOT join that group: it would be
//! TERMed by the same group kill that tears the render down... which is
//! actually FINE for the assertion (the job is over at that point), but
//! worse than fine: terminate_child signals the group at CANCEL time
//! while the grace/drain logic is still running and the job's teardown
//! is not the assertion's business. Simpler and provably correct: this
//! module spawns caffeinate WITHOUT `process_group`, so it stays in the
//! SUPERVISOR's process group — the one thing that never gets a group
//! TERM during a job — and only this module's Drop ever kills it, on
//! every exit path of run_job (Drop cannot be skipped without aborting
//! the whole process).
//!
//! ## Residual limits — what this does NOT do (honesty)
//!
//! `caffeinate -i` holds a "PreventUserIdleSystemSleep"-class assertion.
//! It does NOT prevent:
//!
//! - **lid-close sleep** on a laptop (clamshell), except in the
//!   display-attached-and-powered corner cases macOS itself defines;
//! - **forced sleep** (power button, Apple menu, `pmset sleepnow`);
//! - **shutdown/restart**, or battery-clamped behavior on laptops.
//!
//! If the machine sleeps anyway, the render is suspended, not killed —
//! and the wall-clock cap will not notice either (see the
//! MAX_JOB_WALL_TIME note in runner.rs: tokio's monotonic clock does not
//! advance across system sleep). With the idle assertion held, the
//! hazard shrinks to lid-close/forced sleep; always-on nodes should also
//! disable idle sleep system-wide (docs/operator-quickstart.md).
//!
//! ## Linux
//!
//! Not implemented — documented honestly. Linux render nodes are
//! servers; default server installs do not idle-sleep, and the D-Bus
//! `org.freedesktop.ScreenSaver`/logind inhibit path would drag in a
//! D-Bus client dependency for a hazard Linux nodes do not have by
//! default. The acquire/release shape here (a guard object with Drop) is
//! the seam a future Linux implementation slots into unchanged.

#[cfg(target_os = "macos")]
use std::process::{Command, Stdio};

/// Test observability: how many caffeinate guards are currently held in
/// THIS process. Production code never reads it; the integration test
/// uses it to prove acquire/drop wiring without pgrep censuses (which
/// cannot distinguish sibling tests' caffeinates — same parent pid).
#[cfg(target_os = "macos")]
static ACTIVE_GUARDS: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

#[cfg(all(test, target_os = "macos"))]
pub(crate) fn active_guard_count_for_tests() -> i64 {
    ACTIVE_GUARDS.load(std::sync::atomic::Ordering::SeqCst)
}

/// What the job holds while it runs. On non-macOS this is a no-op guard
/// with the same shape, so run_job's exit paths need no cfg branches.
#[derive(Debug)]
pub struct JobKeepAwake {
    #[cfg(target_os = "macos")]
    child: Option<std::process::Child>,
}

impl JobKeepAwake {
    /// Acquire the per-job sleep assertion. Non-macOS: a no-op guard.
    ///
    /// Failure to acquire (caffeinate missing/broken) must NOT fail the
    /// job — a node without working caffeinate still renders correctly;
    /// it just carries the pre-packet-18 sleep risk. The miss is logged.
    #[allow(clippy::new_without_default)]
    pub fn acquire(job_id: &str) -> Self {
        #[cfg(target_os = "macos")]
        {
            // Deliberately NO process_group(0) here: the child stays in
            // the SUPERVISOR's process group (see module docs). Only
            // Drop kills it.
            match Command::new("/usr/bin/caffeinate")
                .arg("-i")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            {
                Ok(child) => {
                    ACTIVE_GUARDS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    tracing::debug!(
                        job_id,
                        pid = child.id(),
                        "idle-sleep assertion held for job"
                    );
                    Self { child: Some(child) }
                }
                Err(e) => {
                    tracing::warn!(job_id, error = %e, "caffeinate unavailable; rendering without an idle-sleep assertion");
                    Self { child: None }
                }
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = job_id;
            Self {}
        }
    }

    /// Test hook: the caffeinate child's pid, if held.
    #[cfg(target_os = "macos")]
    pub fn child_pid(&self) -> Option<u32> {
        self.child.as_ref().map(|c| c.id())
    }
}

impl Drop for JobKeepAwake {
    fn drop(&mut self) {
        #[cfg(target_os = "macos")]
        if let Some(mut child) = self.child.take() {
            ACTIVE_GUARDS.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            // SIGTERM first (caffeinate exits cleanly and releases the
            // assertion), SIGKILL if it somehow lingers past a second.
            kill_child_tree(&mut child);
        }
    }
}

#[cfg(target_os = "macos")]
fn kill_child_tree(child: &mut std::process::Child) {
    use std::time::{Duration, Instant};
    let pid = child.id() as i32;
    unsafe {
        libc::kill(pid, libc::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => {
                if Instant::now() >= deadline {
                    unsafe {
                        libc::kill(pid, libc::SIGKILL);
                    }
                    let _ = child.wait();
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => return,
        }
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    fn pid_alive(pid: u32) -> bool {
        // kill(pid, 0) probes existence without signaling.
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }

    #[test]
    fn assertion_held_while_guard_alive_and_released_on_drop() {
        let guard = JobKeepAwake::acquire("test-awake");
        let pid = guard
            .child_pid()
            .expect("caffeinate must spawn on macOS CI/dev machines");
        assert!(pid_alive(pid), "caffeinate child alive while job runs");
        drop(guard);
        // Released: the child is reaped by Drop's wait, so the pid is
        // gone (or at minimum no longer caffeinate — ESRCH after reaping
        // is the strong signal; poll briefly for the exit to land).
        let mut released = false;
        for _ in 0..100 {
            if !pid_alive(pid) {
                released = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(released, "caffeinate child must be gone after Drop");
    }

    #[test]
    fn caffeinate_is_not_in_a_new_process_group() {
        // THE placement proof: caffeinate must stay in the SUPERVISOR's
        // process group (never process_group(0)), so the runner's group
        // TERM cannot touch it and only Drop kills it.
        let guard = JobKeepAwake::acquire("test-pgroup");
        let pid = guard.child_pid().expect("caffeinate spawned");
        let out = std::process::Command::new("ps")
            .args(["-o", "pgid=", "-p", &pid.to_string()])
            .output()
            .expect("ps");
        let pgid: u32 = String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .expect("numeric pgid");
        let our_pgid = unsafe { libc::getpgrp() as u32 };
        assert_eq!(
            pgid, our_pgid,
            "caffeinate must share the spawning process's pgid ({}), got {}",
            our_pgid, pgid
        );
        drop(guard);
    }
}
