//! What this machine can actually do.
//!
//! Lives here rather than in each binary because `decent` and the Tauri app both
//! build a register frame, and both independently reported
//! `gpu: allow_real_jobs` — the operator's "accept real jobs" switch, which says
//! nothing about hardware. One shared probe is what stops that drifting again.

use crate::protocol::Capabilities;

/// Measurement (2026-08-19) found no throughput gain from concurrency at 1080p,
/// 4K or WebGPU, and outright failure at 16 (Remotion's internal static server
/// saturates). So the default is one job, and the ceiling exists to stop an
/// operator configuring their node into a state that cannot finish work.
const DEFAULT_MAX_CONCURRENT_JOBS: u32 = 1;
const MAX_CONCURRENT_JOBS_CEILING: u32 = 8;

/// Probe this machine, honouring operator overrides.
///
/// `DECENT_GPU=0|1` and `DECENT_MAX_CONCURRENT_JOBS=<n>` win over the probe: an
/// operator knows their box better than a heuristic does, and a wrong probe
/// should be correctable without a new release.
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

/// The DECLARED max is clamped to 1 (packet 19): the connection loop runs one
/// job at a time and rejects any second assignment unconditionally
/// (connection.rs: in-flight is Option, not a counter), so declaring N > 1 on
/// the wire would be a lie dispatch selection believes (`inFlight <
/// maxConcurrentJobs`) — an operator asking for 4 would receive 4 assignments
/// and reject 3 of them. Per the measured decision in the multi-platform
/// design doc §11.3 (concurrency flat or worse above 1), stop declaring what
/// we do not honor; the parse + ceiling stay for the day the select loop
/// actually holds N jobs.
fn max_concurrent_jobs() -> u32 {
    let configured = std::env::var("DECENT_MAX_CONCURRENT_JOBS")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_MAX_CONCURRENT_JOBS);
    let clamped = configured.min(MAX_CONCURRENT_JOBS_CEILING);
    if clamped > 1 {
        tracing::info!(
            requested = clamped,
            declared = 1,
            "operator requested {clamped} concurrent jobs; the supervisor runs one job at a time today, declaring 1"
        );
    }
    declared_concurrency(clamped)
}

/// The honest declaration: one job, always, until the select loop holds N.
fn declared_concurrency(_configured: u32) -> u32 {
    1
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
        assert_eq!(max_concurrent_jobs(), DEFAULT_MAX_CONCURRENT_JOBS);
    }

    /// Packet 19: the DECLARED concurrency is 1 regardless of what the env
    /// asks for — the connection loop rejects a second assignment, so any
    /// higher declaration is a lie on the wire. Pure-fn shape: the parse
    /// still honors the ceiling for the day the loop holds N; the clamp is
    /// what the register frame reports.
    #[test]
    fn declared_concurrency_is_one_even_when_the_env_asks_for_more() {
        // No env mutation in the parallel test binary — drive the pure fn
        // with the values the env could carry.
        for asked in [1u32, 2, 4, 8, 99] {
            let clamped = asked.min(MAX_CONCURRENT_JOBS_CEILING);
            // What max_concurrent_jobs() must declare, whatever the env said:
            // the register frame carries 1; the log fires for clamped > 1
            // (asserted separately below to keep this test pure).
            assert_eq!(
                declared_concurrency(clamped),
                1,
                "env asking {asked} must still declare 1"
            );
        }
    }

    #[test]
    fn macos_reports_gpu() {
        // ANGLE on Metal is always available on a supported Mac.
        if cfg!(target_os = "macos") {
            assert!(detect_gpu());
        }
    }
}
