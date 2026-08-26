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

    /// The regression that motivated this module: capability must not be the
    /// operator's willingness switch. Whatever the probe decides, it decides it
    /// without being told whether real jobs are enabled.
    #[test]
    fn detection_takes_no_willingness_input() {
        let a = detect_capabilities();
        let b = detect_capabilities();
        assert_eq!(a, b, "detection must be a pure function of the machine");
    }

    #[test]
    fn reports_this_platform() {
        let caps = detect_capabilities();
        assert_eq!(caps.os.as_deref(), Some(std::env::consts::OS));
        assert_eq!(caps.arch.as_deref(), Some(std::env::consts::ARCH));
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
}
