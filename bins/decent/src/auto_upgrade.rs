//! Persistent opt-in and attempt ledger for unattended supervisor upgrades.
//!
//! The connection core owns the idle-safe handoff. This module owns only
//! node-local policy: explicit opt-in and suppression after a failed attempt.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::{version_triplet, UpgradeOutcome};

pub const RETRY_SUPPRESS_MS: u64 = 24 * 60 * 60 * 1000;
const FLAG_FILE: &str = "auto-upgrade";
const STATE_FILE: &str = "auto-upgrade-state.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AttemptOutcome {
    Started,
    Failed,
    Rejected,
    Upgraded,
    AlreadyCurrent,
}

impl AttemptOutcome {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Failed => "failed",
            Self::Rejected => "rejected",
            Self::Upgraded => "upgraded",
            Self::AlreadyCurrent => "already current",
        }
    }

    pub const fn needs_attention(self) -> bool {
        matches!(self, Self::Started | Self::Failed | Self::Rejected)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttemptRecord {
    pub target_version: String,
    pub attempted_at_ms: u64,
    pub outcome: AttemptOutcome,
    pub detail: String,
}

#[derive(Debug)]
pub enum ApplyDisposition {
    Restart {
        from: String,
        to: String,
        record_error: Option<String>,
    },
    Reconnect {
        outcome: AttemptOutcome,
        detail: String,
    },
}

/// Execute the node-local half of an idle-safe handoff. The package channel is
/// injected so the complete validation/ledger/decision path is hermetic in
/// tests; production passes `upgrade_binary`.
pub fn apply(
    config_dir: &Path,
    target_version: &str,
    current_version: &str,
    attempted_at_ms: u64,
    upgrade: impl FnOnce() -> anyhow::Result<UpgradeOutcome>,
) -> anyhow::Result<ApplyDisposition> {
    match (
        version_triplet(target_version),
        version_triplet(current_version),
    ) {
        (Some(target), Some(current)) if target > current => {}
        (Some(_), Some(_)) => {
            let detail = format!("ignored non-newer dispatch target; running {current_version}");
            write_attempt(
                config_dir,
                &AttemptRecord {
                    target_version: target_version.into(),
                    attempted_at_ms,
                    outcome: AttemptOutcome::AlreadyCurrent,
                    detail: detail.clone(),
                },
            )?;
            return Ok(ApplyDisposition::Reconnect {
                outcome: AttemptOutcome::AlreadyCurrent,
                detail,
            });
        }
        _ => {
            let detail = "dispatch target is not an exact semantic version".to_string();
            write_attempt(
                config_dir,
                &AttemptRecord {
                    target_version: target_version.into(),
                    attempted_at_ms,
                    outcome: AttemptOutcome::Rejected,
                    detail: detail.clone(),
                },
            )?;
            return Ok(ApplyDisposition::Reconnect {
                outcome: AttemptOutcome::Rejected,
                detail,
            });
        }
    }

    // This durable write is the crash-loop fuse. Never start unattended
    // package-manager work if the attempt cannot be persisted first.
    write_attempt(
        config_dir,
        &AttemptRecord {
            target_version: target_version.into(),
            attempted_at_ms,
            outcome: AttemptOutcome::Started,
            detail: "idle-safe connection handoff complete".into(),
        },
    )?;

    match upgrade() {
        Ok(UpgradeOutcome::Upgraded { from, to }) => {
            let record_error = write_attempt(
                config_dir,
                &AttemptRecord {
                    target_version: target_version.into(),
                    attempted_at_ms,
                    outcome: AttemptOutcome::Upgraded,
                    detail: format!("{from} -> {to}"),
                },
            )
            .err()
            .map(|e| format!("{e:#}"));
            // A completed binary swap is authoritative. Failure to update the
            // observation ledger must never make the old process reconnect.
            Ok(ApplyDisposition::Restart {
                from,
                to,
                record_error,
            })
        }
        Ok(UpgradeOutcome::AlreadyCurrent(current)) => {
            let detail = format!("channel and installed binary are {current}");
            write_attempt(
                config_dir,
                &AttemptRecord {
                    target_version: target_version.into(),
                    attempted_at_ms,
                    outcome: AttemptOutcome::AlreadyCurrent,
                    detail: detail.clone(),
                },
            )?;
            Ok(ApplyDisposition::Reconnect {
                outcome: AttemptOutcome::AlreadyCurrent,
                detail,
            })
        }
        Err(error) => {
            let detail = format!("{error:#}");
            // If this write fails, the Started record remains and still acts
            // as the suppression fuse.
            let _ = write_attempt(
                config_dir,
                &AttemptRecord {
                    target_version: target_version.into(),
                    attempted_at_ms,
                    outcome: AttemptOutcome::Failed,
                    detail: detail.clone(),
                },
            );
            Ok(ApplyDisposition::Reconnect {
                outcome: AttemptOutcome::Failed,
                detail,
            })
        }
    }
}

pub fn flag_path(config_dir: &Path) -> PathBuf {
    config_dir.join(FLAG_FILE)
}

pub fn state_path(config_dir: &Path) -> PathBuf {
    config_dir.join(STATE_FILE)
}

pub fn is_enabled(config_dir: &Path) -> bool {
    flag_path(config_dir).is_file()
}

pub fn set_enabled(config_dir: &Path, enabled: bool) -> anyhow::Result<()> {
    let path = flag_path(config_dir);
    if enabled {
        atomic_private_write(&path, b"enabled\n")
    } else {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

pub fn read_attempt(config_dir: &Path) -> Option<AttemptRecord> {
    let bytes = std::fs::read(state_path(config_dir)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub fn write_attempt(config_dir: &Path, record: &AttemptRecord) -> anyhow::Result<()> {
    let mut bytes = serde_json::to_vec_pretty(record)?;
    bytes.push(b'\n');
    atomic_private_write(&state_path(config_dir), &bytes)
}

/// A started attempt suppresses too: if the process or machine died between
/// writing Started and recording the outcome, immediately trying the same
/// release again is the least safe response.
pub fn suppression(config_dir: &Path, now_ms: u64) -> Option<(String, std::time::Duration)> {
    let record = read_attempt(config_dir)?;
    let suppresses = matches!(
        record.outcome,
        AttemptOutcome::Started
            | AttemptOutcome::Failed
            | AttemptOutcome::Rejected
            | AttemptOutcome::AlreadyCurrent
    );
    let age = now_ms.saturating_sub(record.attempted_at_ms);
    (suppresses && age < RETRY_SUPPRESS_MS).then(|| {
        (
            record.target_version,
            std::time::Duration::from_millis(RETRY_SUPPRESS_MS - age),
        )
    })
}

fn atomic_private_write(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("config file has no parent: {}", path.display()))?;
    std::fs::create_dir_all(parent)?;
    set_mode(parent, 0o700);

    let tmp = parent.join(format!(
        ".{}.tmp-{}",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("decent"),
        std::process::id()
    ));
    let result = (|| -> anyhow::Result<()> {
        std::fs::write(&tmp, bytes)?;
        set_mode(&tmp, 0o600);
        std::fs::rename(&tmp, path)?;
        set_mode(path, 0o600);
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(tmp);
    }
    result
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "decent-auto-upgrade-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn apply_success_records_upgrade_and_requests_restart() {
        let dir = temp_dir("apply-success");
        let result = apply(&dir, "rust-0.0.12", "0.0.11", 42, || {
            Ok(UpgradeOutcome::Upgraded {
                from: "0.0.11".into(),
                to: "0.0.12".into(),
            })
        })
        .unwrap();
        assert!(matches!(
            result,
            ApplyDisposition::Restart {
                from,
                to,
                record_error: None
            } if from == "0.0.11" && to == "0.0.12"
        ));
        let record = read_attempt(&dir).unwrap();
        assert_eq!(record.outcome, AttemptOutcome::Upgraded);
        assert_eq!(record.attempted_at_ms, 42);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn apply_failure_records_fuse_and_reconnects() {
        let dir = temp_dir("apply-failure");
        let result = apply(&dir, "0.0.12", "0.0.11", 43, || {
            anyhow::bail!("fake channel unavailable")
        })
        .unwrap();
        assert!(matches!(
            result,
            ApplyDisposition::Reconnect {
                outcome: AttemptOutcome::Failed,
                ..
            }
        ));
        let record = read_attempt(&dir).unwrap();
        assert_eq!(record.outcome, AttemptOutcome::Failed);
        assert!(record.detail.contains("fake channel unavailable"));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn malformed_or_non_newer_target_never_calls_install_channel() {
        for (tag, target, expected) in [
            ("malformed", "dev", AttemptOutcome::Rejected),
            ("equal", "rust-0.0.11", AttemptOutcome::AlreadyCurrent),
            ("older", "0.0.10", AttemptOutcome::AlreadyCurrent),
        ] {
            let dir = temp_dir(tag);
            let result = apply(&dir, target, "0.0.11", 44, || {
                panic!("install channel must not run for {target}")
            })
            .unwrap();
            assert!(matches!(
                result,
                ApplyDisposition::Reconnect {outcome, ..} if outcome == expected
            ));
            assert_eq!(read_attempt(&dir).unwrap().outcome, expected);
            std::fs::remove_dir_all(dir).ok();
        }
    }

    #[test]
    fn opt_in_defaults_off_and_round_trips() {
        let dir = temp_dir("flag");
        assert!(!is_enabled(&dir));
        set_enabled(&dir, true).unwrap();
        assert!(is_enabled(&dir));
        set_enabled(&dir, false).unwrap();
        assert!(!is_enabled(&dir));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn failed_and_interrupted_attempts_suppress_for_one_day() {
        let dir = temp_dir("suppress");
        let now = 2 * RETRY_SUPPRESS_MS;
        for outcome in [
            AttemptOutcome::Started,
            AttemptOutcome::Failed,
            AttemptOutcome::Rejected,
        ] {
            write_attempt(
                &dir,
                &AttemptRecord {
                    target_version: "0.0.12".into(),
                    attempted_at_ms: now,
                    outcome,
                    detail: "test".into(),
                },
            )
            .unwrap();
            assert_eq!(
                suppression(&dir, now),
                Some((
                    "0.0.12".into(),
                    std::time::Duration::from_millis(RETRY_SUPPRESS_MS)
                ))
            );
            assert_eq!(
                suppression(&dir, now + RETRY_SUPPRESS_MS),
                None,
                "suppression boundary is exclusive"
            );
        }
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn successful_upgrade_never_suppresses_the_new_daemon() {
        let dir = temp_dir("success");
        write_attempt(
            &dir,
            &AttemptRecord {
                target_version: "0.0.12".into(),
                attempted_at_ms: 10,
                outcome: AttemptOutcome::Upgraded,
                detail: "0.0.11 -> 0.0.12".into(),
            },
        )
        .unwrap();
        assert_eq!(suppression(&dir, 11), None);
        std::fs::remove_dir_all(dir).ok();
    }
}
