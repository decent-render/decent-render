//! The purge rule — the reason this crate is open source.
//!
//! `jobAssign` carries `purgeAfter: true` on every job: the supervisor MUST
//! wipe the job's working directory when the job ends, success or failure.
//! [`WorkDir`] makes that structural — the directory is removed on `Drop`, so
//! there is no code path (panic included) that leaves user content on disk.
//!
//! Platform bundles are exempt (they are platform content, cached across jobs
//! elsewhere); [`WorkDir`] is for per-job transient data only.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A per-job working directory that is recursively deleted when dropped.
#[derive(Debug)]
pub struct WorkDir {
    path: PathBuf,
}

impl WorkDir {
    /// Create a fresh, unique working directory under the OS temp dir,
    /// e.g. `/tmp/job-spike-1-1719999999-0`.
    pub fn new(prefix: &str) -> std::io::Result<Self> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "{prefix}-{pid}-{nanos}-{unique}",
            pid = std::process::id()
        ));
        std::fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    /// The directory path. Valid until the guard is dropped.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        match std::fs::remove_dir_all(&self.path) {
            Ok(()) => tracing::debug!(path = %self.path.display(), "workdir purged"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                // Never panic in Drop; loudly report a purge failure instead.
                tracing::error!(path = %self.path.display(), error = %e, "workdir purge FAILED");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_and_purges_recursively() {
        let dir = WorkDir::new("job-test").unwrap();
        let path = dir.path().to_path_buf();
        assert!(path.is_dir());

        let nested = path.join("frames").join("deep");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("out.mp4"), b"user content").unwrap();

        drop(dir);
        assert!(!path.exists(), "workdir must be gone after drop");
    }

    #[test]
    fn purges_on_panic_unwind() {
        let path = {
            let dir = WorkDir::new("job-panic").unwrap();
            let path = dir.path().to_path_buf();
            std::fs::write(path.join("secret.json"), b"{}").unwrap();
            let result = std::panic::catch_unwind(move || {
                let _held = dir; // moved into the panicking scope
                panic!("render exploded");
            });
            assert!(result.is_err());
            path
        };
        assert!(!path.exists(), "workdir must be purged even on panic");
    }

    #[test]
    fn two_workdirs_are_distinct() {
        let a = WorkDir::new("job-x").unwrap();
        let b = WorkDir::new("job-x").unwrap();
        assert_ne!(a.path(), b.path());
    }
}
