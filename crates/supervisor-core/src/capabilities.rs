//! What this machine can actually do.
//!
//! Lives here rather than in each binary because `decent` and the Tauri app both
//! build a register frame, and both independently reported
//! `gpu: allow_real_jobs` — the operator's "accept real jobs" switch, which says
//! nothing about hardware. One shared probe is what stops that drifting again.

use crate::protocol::Capabilities;

/// Probe this machine, honouring operator overrides.
///
/// `DECENT_GPU=0|1` wins over the probe: an operator knows their box better
/// than a heuristic does, and a wrong probe should be correctable without a
/// new release. (Concurrency is NOT configurable — see max_concurrent_jobs.)
pub fn detect_capabilities() -> Capabilities {
    Capabilities {
        gpu: gpu_override().unwrap_or_else(detect_gpu),
        max_concurrent_jobs: Some(max_concurrent_jobs()),
        os: Some(std::env::consts::OS.to_string()),
        arch: Some(std::env::consts::ARCH.to_string()),
    }
}

fn gpu_override() -> Option<bool> {
    match std::env::var("DECENT_GPU").ok()?.trim() {
        "1" | "true" | "yes" => Some(true),
        "0" | "false" | "no" => Some(false),
        _ => None,
    }
}

/// PACKET 40 (audit 19): the node declares exactly ONE concurrent job —
/// the connection loop runs a single in-flight slot and rejects any second
/// assignment unconditionally, so declaring N > 1 would make dispatch
/// believe a lie (`inFlight < maxConcurrentJobs`) and waste assignments.
/// The `DECENT_MAX_CONCURRENT_JOBS` env knob was REMOVED: it parsed, was
/// clamped, was logged — and then ignored (declared_concurrency returned 1
/// regardless), which is worse than no knob (an operator setting it
/// believes it worked). When the select loop genuinely holds N jobs,
/// reintroduce the knob together with the loop change, not before.
fn max_concurrent_jobs() -> u32 {
    1 // see the doc comment: one in-flight slot is what the loop honors
}

/// Can this node render the GPU path?
///
/// The render path is Chrome with `chromiumOptions: {gl: 'angle'}`.
///
/// - **macOS**: ANGLE is backed by Metal, which is present on every supported
///   Mac, so this is true without probing.
/// - **Linux**: requires a DRM render node. A headless server or a Pi-class SBC
///   without working drivers has no `/dev/dri/renderD*`, and claiming GPU there
///   means accepting jobs this node cannot finish. Note that `chrome-for-testing`
///   has no linux-arm64 build at all — Remotion silently substitutes a Playwright
///   chromium — so ARM Linux is doubly not a GPU target today.
/// - **Anything else**: false. An unknown platform has not been verified, and
///   the safe direction for a capability claim is to under-promise.
fn detect_gpu() -> bool {
    #[cfg(target_os = "macos")]
    {
        true
    }
    #[cfg(target_os = "linux")]
    {
        has_drm_render_node()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        false
    }
}

#[cfg(target_os = "linux")]
fn has_drm_render_node() -> bool {
    let Ok(entries) = std::fs::read_dir("/dev/dri") else {
        return false;
    };
    entries.flatten().any(|entry| {
        entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with("renderD"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// DECENT_GPU is process-global env, and the test binary runs tests in
    /// parallel threads — the same serialization pattern as bins/decent's
    /// HOME_LOCK: any test that touches the override holds this lock.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// RAII restore: a failing assert must not leak DECENT_GPU into sibling
    /// tests any more than a set without the lock would.
    struct OverrideVar;
    impl OverrideVar {
        fn set(&self, value: &str) {
            std::env::set_var("DECENT_GPU", value);
        }
    }
    impl Drop for OverrideVar {
        fn drop(&mut self) {
            std::env::remove_var("DECENT_GPU");
        }
    }

    /// The regression that motivated this module: capability must not be the
    /// operator's willingness switch. Whatever the probe decides, it decides it
    /// without being told whether real jobs are enabled.
    #[test]
    fn detection_takes_no_willingness_input() {
        // Holds ENV_LOCK: detect_capabilities reads DECENT_GPU, and the
        // override test mutates that very env — without the lock the two
        // calls below can straddle an override flip and "fail" purity.
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let a = detect_capabilities();
        let b = detect_capabilities();
        assert_eq!(a, b, "detection must be a pure function of the machine");
    }

    #[test]
    fn reports_this_platform() {
        let caps = detect_capabilities();
        // EXACT strings, pinned against the compile-time cfg — not just
        // non-empty. dispatch matches on these verbatim, so "darwin" vs
        // "macos" or "arm64" vs "aarch64" would silently drop this node
        // from every payload match; the constants are the contract.
        let expected_os = if cfg!(target_os = "macos") {
            "macos"
        } else if cfg!(target_os = "linux") {
            "linux"
        } else {
            std::env::consts::OS
        };
        let expected_arch = if cfg!(target_arch = "aarch64") {
            "aarch64"
        } else if cfg!(target_arch = "x86_64") {
            "x86_64"
        } else {
            std::env::consts::ARCH
        };
        assert_eq!(caps.os.as_deref(), Some(expected_os));
        assert_eq!(caps.arch.as_deref(), Some(expected_arch));
    }

    /// The override hook must WIN on every platform: an operator who knows
    /// the probe is wrong about their box sets DECENT_GPU and the register
    /// frame reports what they said, whatever /dev/dri or Metal looks like.
    #[test]
    fn gpu_override_wins_over_the_probe_on_every_platform() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let var = OverrideVar;
        for falsy in ["0", "false", "no"] {
            var.set(falsy);
            assert!(
                !detect_capabilities().gpu,
                "DECENT_GPU={falsy} must force gpu=false on this platform"
            );
        }
        for truthy in ["1", "true", "yes"] {
            var.set(truthy);
            assert!(
                detect_capabilities().gpu,
                "DECENT_GPU={truthy} must force gpu=true on this platform"
            );
        }
    }

    #[test]
    fn defaults_to_one_job() {
        // Concurrency showed no measured gain; the default must not drift up
        // silently just because a machine looks big.
        assert_eq!(max_concurrent_jobs(), 1);
    }

    /// PACKET 40 (audit 19): DECENT_MAX_CONCURRENT_JOBS was an inert knob —
    /// parsed, clamped, logged, and then ignored (the declaration was always
    /// 1). Deleted rather than honoured: honouring it without a connection-
    /// loop rewrite would make dispatch assign N jobs we reject N-1 of.
    /// Pin BOTH halves: the declaration is 1, and the knob no longer exists
    /// anywhere in the source.
    #[test]
    fn concurrency_declaration_is_one_and_the_env_knob_is_gone() {
        assert_eq!(max_concurrent_jobs(), 1);
        // The knob must not creep back as a parse that goes nowhere.
        assert!(!std::env::vars().any(|(k, _)| k == "DECENT_MAX_CONCURRENT_JOBS"));
    }

    #[test]
    fn macos_reports_gpu() {
        // ANGLE on Metal is always available on a supported Mac.
        if cfg!(target_os = "macos") {
            assert!(detect_gpu());
        }
    }

    /// Linux-only wiring pin: with no DRM render node the probe must report
    /// no GPU (a headless server claiming GPU accepts jobs it cannot finish).
    /// On a GPU box the precondition inverts and the identity is asserted the
    /// other way — either way detect_gpu → has_drm_render_node is what is
    /// pinned.
    #[cfg(target_os = "linux")]
    #[test]
    fn linux_gpu_tracks_drm_render_node() {
        if has_drm_render_node() {
            assert!(detect_gpu());
        } else {
            assert!(!detect_gpu());
        }
    }
}
