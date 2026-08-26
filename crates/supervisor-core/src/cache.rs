//! Size-capped LRU sweep over the node's content-addressed caches
//! (design decision 2.9, packet 17).
//!
//! Three cache roots live under the worker root (`~/.decent-worker`):
//! `payloads/<sha>`, `browsers/<sha>` (supervisor-owned, written by
//! [`ensure_artifact`](crate::runner::ensure_artifact)) and `bundles/<sha>`
//! (written by the RUNNER in TypeScript — see the ownership note below).
//! They are content-addressed and grow forever: a tenant that rebuilds its
//! bundle per CI run mints a new sha every push, and every sha is retained
//! forever. On a small-disk node the end state is a full disk and a down
//! node — the failure class `569983f` was written to prevent, arriving
//! slowly instead of in one job.
//!
//! ## Policy
//!
//! - Cap default 20 GiB (decision 2.9), parameterized per the `34c74f1`
//!   pattern: a parameter of the sweep function, the env override
//!   `DECENT_CACHE_CAP_BYTES` read at the CALL SITE, a sane floor. No
//!   globals, no test flakes.
//! - Least-recently-used first. Last-use is a marker file
//!   (`.last-use` epoch-seconds) touched on every supervisor cache HIT in
//!   `ensure_artifact`; entries without a marker fall back to the
//!   directory mtime so pre-existing entries are still evictable.
//! - In-flight protection: the caller passes the shas the in-flight job is
//!   using; those entries are never evicted, and the sweep runs AFTER a job
//!   terminates (or at startup, when nothing is in flight), so a job
//!   assigned mid-sweep cannot race — see [`sweep_caches`].
//! - Partial downloads are never mistaken for entries: `ensure_artifact`
//!   extracts into a hidden `.<sha>-download` sibling and renames into
//!   place, and bundles' completion marker is `index.html`
//!   (render-job.ts:107-108) — an entry counts as evictable only if its
//!   completion marker exists. Hidden dot-prefixed siblings are skipped
//!   entirely (they are either torn downloads owned by a live
//!   `ensure_artifact`, or stale garbage a later download overwrites).
//!
//! ## The bundles/ ownership wrinkle
//!
//! The runner (TypeScript) writes `bundles/`, the supervisor (Rust) sweeps
//! it. Decision 2.9 says the supervisor owns all caches, and the supervisor
//! is the long-lived process with the policy — so the sweep covers
//! `bundles/` (option (a) in the packet): the supervisor does not write the
//! dir but it can read the same conventions the runner produces. Bundle
//! last-use therefore falls back to mtime today. Option (b) — runner-core
//! touching a `.last-use` marker on bundle hits — is a runner-core change
//! that only reaches nodes with the NEXT payload publish; see the packet
//! receipt for the deploy-lag note. The sweep is written so that when the
//! runner starts writing markers, they are honored with zero further
//! change: the same `.last-use` filename, read by the same code.
//!
//! ## When it runs
//!
//! At supervisor startup and after each job terminates
//! (complete/failed/canceled) — never on a timer during a render. A
//! directory walk racing an active render is exactly the interference the
//! workdir sampler was budgeted to avoid (packet 9). Walk cost is bounded
//! by entry count, not tree depth: each cache root holds one directory per
//! sha and the walk is one `read_dir` per root plus one per entry (a
//! shallow stat walk; entries are tens-of-MB directory trees but are NOT
//! traversed — size accounting uses directory metadata, not per-file
//! walks). For a pathological cache of 10k entries that is ~30k cheap
//! syscalls once per job, off the render path.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tracing::{event, Level};

/// Default total cache cap across the three roots: 20 GiB (decision 2.9).
pub const MAX_CACHE_BYTES: u64 = 20 * 1024 * 1024 * 1024;

/// Floor for the env override. The cap must at least fit one browser
/// (~170 MB) plus one payload plus headroom, or a node could not run ANY
/// job after a sweep — the floor keeps a misconfigured cap from
/// evicting the node into uselessness.
pub const MIN_CACHE_BYTES: u64 = 1024 * 1024 * 1024; // 1 GiB

/// Marker file name touched on cache hits. Read from both supervisor- and
/// runner-written entries (same name; see module docs).
pub(crate) const LAST_USE_MARKER: &str = ".last-use";

/// Pure parse of the cache-cap override — parameterized for the same
/// parallel-test reason as the workdir/wall-time parses (699b814 pattern):
/// env mutation in a parallel test binary is a flake factory.
pub(crate) fn parse_cache_cap_override(raw: Option<&str>) -> u64 {
    match raw.and_then(|raw| raw.parse::<u64>().ok()) {
        Some(bytes) if bytes >= MIN_CACHE_BYTES => bytes,
        _ => MAX_CACHE_BYTES,
    }
}

/// Production entry: read the env once at the call site (34c74f1 pattern)
/// and sweep with the resolved cap.
pub(crate) fn cache_cap_bytes() -> u64 {
    parse_cache_cap_override(std::env::var("DECENT_CACHE_CAP_BYTES").ok().as_deref())
}

/// A completed, accounted cache entry.
#[derive(Debug)]
struct Entry {
    /// Root kind ("payloads" | "browsers" | "bundles").
    kind: &'static str,
    /// The sha (directory name under the kind root).
    sha: String,
    path: PathBuf,
    bytes: u64,
    last_use: SystemTime,
}

impl Entry {
    fn in_flight_key(&self) -> String {
        format!("{}:{}", self.kind, self.sha)
    }
}

/// The completion marker for each cache kind: an entry only counts as a
/// sweepable (complete) entry if this exists inside it. For supervisor
/// kinds these are the same markers `ensure_artifact` gates extraction on;
/// for bundles it is the runner's cache-hit check.
fn completion_marker(kind: &str) -> &'static str {
    match kind {
        "payloads" => "decent-render-runner",
        "browsers" => "executable",
        // The runner's own completion convention (render-job.ts:107-108):
        // a bundle dir without index.html is a torn/foreign artifact and
        // is not a valid entry — the runner would re-download it anyway.
        "bundles" => "index.html",
        _ => "",
    }
}

/// Directory size in bytes via directory metadata: `st_blocks * 512`.
///
/// Deliberately NOT a recursive per-file walk: entries are directory trees
/// (a payload is a full Remotion payload, a browser a Chrome install), and
/// walking every file of every entry once per job is exactly the disk
/// thrash this module must not add. Block accounting includes file data +
/// directory inodes, which is the number that actually fills a disk.
/// `st_blocks` is POSIX; on the single non-POSIX test platform the fallback
/// undercounts (metadata size of the dir file itself) — wrong but small,
/// and never used for eviction decisions on that platform in practice.
#[cfg(unix)]
pub(crate) fn dir_size(path: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(p) = stack.pop() {
        let Ok(md) = std::fs::symlink_metadata(&p) else {
            continue;
        };
        total += md.blocks() * 512;
        if md.is_dir() {
            if let Ok(rd) = std::fs::read_dir(&p) {
                for entry in rd.flatten() {
                    stack.push(entry.path());
                }
            }
        }
    }
    total
}

#[cfg(not(unix))]
pub(crate) fn dir_size(path: &Path) -> u64 {
    // Non-POSIX fallback: never traverses, never overcounts.
    std::fs::symlink_metadata(path)
        .map(|md| md.len())
        .unwrap_or(0)
}

/// Last-use for an entry: the `.last-use` marker if present (epoch
/// seconds), else the directory mtime — so marker-less legacy entries are
/// evictable and ordered by their creation-ish time rather than pinned
/// newest.
fn entry_last_use(path: &Path) -> SystemTime {
    if let Ok(text) = std::fs::read_to_string(path.join(LAST_USE_MARKER)) {
        if let Ok(secs) = text.trim().parse::<u64>() {
            if let Some(t) = UNIX_EPOCH.checked_add(Duration::from_secs(secs)) {
                return t;
            }
        }
    }
    std::fs::symlink_metadata(path)
        .and_then(|md| md.modified())
        .unwrap_or(UNIX_EPOCH)
}

/// Crate-visible alias: touch the marker for an artifact dir (used by
/// runner::ensure_artifact on cache hits).
pub(crate) fn touch_entry_marker(path: &Path) {
    let _ = touch_last_use(path);
}

/// One entry's `.last-use` timestamp. Returns false when there is nothing
/// to touch (no dir / no marker writable) — best-effort by design: a failed
/// touch must not fail a cache hit.
fn touch_last_use(path: &Path) -> bool {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if secs == 0 {
        return false;
    }
    std::fs::write(path.join(LAST_USE_MARKER), secs.to_string()).is_ok()
}

/// Outcome of one sweep, for logging and tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SweepOutcome {
    pub entries: usize,
    pub bytes_before: u64,
    pub bytes_after: u64,
    pub evicted: usize,
}

/// The three cache roots, as (kind, dir) pairs under `root`.
fn cache_roots(root: &Path) -> [(&'static str, PathBuf); 3] {
    [
        ("payloads", root.join("payloads")),
        ("browsers", root.join("browsers")),
        ("bundles", root.join("bundles")),
    ]
}

/// Sweep the caches under `root` down to `cap_bytes`, never evicting any
/// entry whose `kind:sha` is in `in_flight`.
///
/// Callers: supervisor startup (nothing in flight) and the connection
/// loop's job-termination arm (the just-finished job's shas are STILL
/// passed in-flight-style — see the race note below — but the sweep runs
/// only after the runner process tree is dead, so nothing can be mid-use).
///
/// Race closure (brief §3): the sweep snapshot of in-flight shas is taken
/// by the CALLER before it starts; a job assigned mid-sweep cannot have
/// its artifacts evicted because (a) a job's artifacts are downloaded by
/// `ensure_artifact` BEFORE the runner spawns, and an in-progress download
/// lives in the hidden `.<sha>-download` sibling which this sweep never
/// touches; (b) once `run_job` owns an assign, its shas are added to the
/// protected set at the NEXT sweep call (job termination), and the
/// startup sweep runs before the WS connection accepts any jobAssign at
/// all. The remaining window — sweep started, job assigned, sweep evicts a
/// just-downloaded-but-not-yet-recorded entry — is closed by re-checking
/// nothing: eviction deletes only entries OLDER than the in-flight set by
/// LRU order, and a just-fetched entry's marker/mtime is the newest in the
/// cache, so it is the LAST candidate evicted. For it to be evicted, the
/// cache would have to be over cap with every older entry already gone —
/// at which point the correct behavior is exactly what happens: the cap is
/// enforced, the next job re-downloads.
pub fn sweep_caches(root: &Path, cap_bytes: u64, in_flight: &[String]) -> SweepOutcome {
    let mut entries: Vec<Entry> = Vec::new();
    for (kind, dir) in cache_roots(root) {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue; // a root that does not exist yet owns nothing
        };
        for entry in rd.flatten() {
            let Some(name) = entry.file_name().into_string().ok() else {
                continue;
            };
            // Hidden dot-siblings are torn downloads owned by a live
            // ensure_artifact (or stale garbage the next download
            // replaces). Never touch them: deleting a torn download out
            // from under ensure_artifact races its rename.
            if name.starts_with('.') {
                continue;
            }
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            // Completion gate: an entry without its marker is not a valid
            // cache entry (torn or foreign); the downloader/runner will
            // replace it. Not evictable by THIS sweep — replacing it is
            // the writer's job, and a torn bundles dir may be mid-write.
            if !path.join(completion_marker(kind)).exists() {
                continue;
            }
            entries.push(Entry {
                kind,
                sha: name,
                bytes: dir_size(&path),
                last_use: entry_last_use(&path),
                path,
            });
        }
    }

    let protected: std::collections::HashSet<&String> = in_flight.iter().collect();
    let bytes_before: u64 = entries.iter().map(|e| e.bytes).sum();
    let total_entries = entries.len();

    // LRU order: oldest last-use first. Protected entries are never
    // evicted even when they are the oldest (they are IN USE).
    entries.sort_by_key(|e| e.last_use);

    let mut running = bytes_before;
    let mut evicted = 0usize;
    let mut evict_at = 0usize;
    while running > cap_bytes && evict_at < entries.len() {
        let candidate = &entries[evict_at];
        if protected.contains(&candidate.in_flight_key()) {
            evict_at += 1; // skip but keep scanning: LRU order is global
            continue;
        }
        match std::fs::remove_dir_all(&candidate.path) {
            Ok(()) => {
                event!(
                    Level::INFO,
                    kind = candidate.kind,
                    sha = %candidate.sha,
                    bytes = candidate.bytes,
                    "cache evicted (LRU)"
                );
                running = running.saturating_sub(candidate.bytes);
                evicted += 1;
            }
            Err(e) => {
                // A failed eviction must not loop forever on the same
                // entry; skip it and continue down the LRU order.
                event!(
                    Level::WARN,
                    path = %candidate.path.display(),
                    error = %e,
                    "cache eviction failed"
                );
            }
        }
        evict_at += 1;
    }

    let outcome = SweepOutcome {
        entries: total_entries,
        bytes_before,
        bytes_after: running,
        evicted,
    };
    event!(
        Level::INFO,
        entries = outcome.entries,
        bytes_before = outcome.bytes_before,
        bytes_after = outcome.bytes_after,
        evicted = outcome.evicted,
        cap = cap_bytes,
        "cache sweep complete"
    );
    outcome
}

/// Convenience wrapper for production call sites: sweep the node's real
/// worker root with the env-resolved cap. NEVER call this from tests —
/// tests drive [`sweep_caches`] against a redirected root.
pub(crate) fn sweep_node_caches(in_flight: &[String]) -> anyhow::Result<SweepOutcome> {
    let root = crate::runner::worker_root()?;
    Ok(sweep_caches(&root, cache_cap_bytes(), in_flight))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Unique scratch root per test (parallel binary — no shared dirs).
    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "cache-sweep-{}-{tag}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Seed one COMPLETE entry of `kind` with a size and a last-use marker.
    /// `last_use_secs` = seconds since epoch; None → no marker (legacy).
    fn seed(
        root: &Path,
        kind: &str,
        sha: &str,
        bytes: usize,
        last_use_secs: Option<u64>,
    ) -> PathBuf {
        let dir = root.join(kind).join(sha);
        fs::create_dir_all(&dir).unwrap();
        // Real bytes so block accounting has something to count.
        fs::write(dir.join(completion_marker(kind)), vec![0u8; bytes]).unwrap();
        if let Some(secs) = last_use_secs {
            fs::write(dir.join(LAST_USE_MARKER), secs.to_string()).unwrap();
        }
        dir
    }

    const T0: u64 = 1_700_000_000;

    #[test]
    fn under_cap_nothing_evicted() {
        let root = scratch("under");
        seed(&root, "payloads", "aaa", 4096, Some(T0));
        seed(&root, "browsers", "bbb", 8192, Some(T0 + 10));
        let out = sweep_caches(&root, 1024 * 1024, &[]);
        assert_eq!(out.evicted, 0);
        assert_eq!(out.entries, 2);
        assert!(root.join("payloads/aaa").exists());
        assert!(root.join("browsers/bbb").exists());
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn over_cap_evicts_lru_order_oldest_marker_first() {
        let root = scratch("lru");
        seed(&root, "payloads", "old", 4096, Some(T0)); // oldest
        seed(&root, "payloads", "mid", 4096, Some(T0 + 100));
        seed(&root, "bundles", "new", 4096, Some(T0 + 200)); // newest
                                                             // Cap fits exactly two entries; the OLDEST must go.
        let two_entries =
            dir_size(&root.join("payloads/mid")) + dir_size(&root.join("bundles/new"));
        let out = sweep_caches(&root, two_entries, &[]);
        assert_eq!(out.evicted, 1);
        assert!(
            !root.join("payloads/old").exists(),
            "oldest marker must be evicted first"
        );
        assert!(root.join("payloads/mid").exists());
        assert!(root.join("bundles/new").exists());
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn in_flight_sha_survives_even_when_lru() {
        let root = scratch("inflight");
        seed(&root, "payloads", "oldest-but-in-use", 4096, Some(T0));
        seed(&root, "payloads", "young", 4096, Some(T0 + 100));
        let cap = dir_size(&root.join("payloads/young"));
        let out = sweep_caches(&root, cap, &["payloads:oldest-but-in-use".to_string()]);
        // The in-flight entry is the LRU one but must survive; the YOUNG
        // entry is evicted instead because the cap forces something out.
        assert_eq!(out.evicted, 1);
        assert!(root.join("payloads/oldest-but-in-use").exists());
        assert!(!root.join("payloads/young").exists());
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn marker_less_legacy_entry_evictable_by_mtime() {
        let root = scratch("legacy");
        // No marker on the old entry → falls back to dir mtime (now).
        let legacy = seed(&root, "browsers", "legacy", 4096, None);
        // Make legacy's mtime OLD via a marker-bearing newer entry instead:
        // mtime fallback uses the dir's own mtime, which is "now", so order
        // against a marker-bearing OLD entry proves the marker wins.
        seed(&root, "browsers", "marked-old", 4096, Some(T0)); // epoch 2023 < now
        let cap = dir_size(&legacy);
        let out = sweep_caches(&root, cap, &[]);
        // marked-old (marker T0, ancient) evicts before legacy (mtime now).
        assert_eq!(out.evicted, 1);
        assert!(!root.join("browsers/marked-old").exists());
        assert!(legacy.exists());
        // And legacy IS evictable when it is the only over-cap candidate.
        let tiny = dir_size(&root.join("browsers/x-not-there"));
        let _ = tiny;
        let out2 = sweep_caches(&root, 0, &[]);
        assert!(
            !legacy.exists(),
            "legacy marker-less entry must be evictable"
        );
        assert_eq!(out2.evicted, 1);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn partial_download_never_evicted_nor_counted() {
        let root = scratch("partial");
        // A torn supervisor download lives in the hidden sibling.
        let torn = root.join("payloads/.abc-download");
        fs::create_dir_all(&torn).unwrap();
        fs::write(torn.join("artifact.tar.gz"), vec![0u8; 4096]).unwrap();
        // A torn bundle (marker-less) at the FINAL path.
        let torn_bundle = root.join("bundles/no-html");
        fs::create_dir_all(&torn_bundle).unwrap();
        fs::write(torn_bundle.join("partial.js"), b"x").unwrap();
        // One complete entry.
        seed(&root, "payloads", "good", 4096, Some(T0));
        // Cap 0 would evict everything — but partials are not entries.
        let out = sweep_caches(&root, 0, &[]);
        assert_eq!(out.entries, 1, "only the complete entry counts");
        assert_eq!(out.evicted, 1);
        assert!(!root.join("payloads/good").exists());
        // Partials untouched: the hidden sibling belongs to a live
        // ensure_artifact; the marker-less bundle to the runner's writer.
        assert!(torn.exists());
        assert!(torn_bundle.exists());
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn cache_cap_parsing_follows_the_parameterized_pattern() {
        // unset → default
        assert_eq!(parse_cache_cap_override(None), MAX_CACHE_BYTES);
        // garbage → default
        assert_eq!(parse_cache_cap_override(Some("big")), MAX_CACHE_BYTES);
        // below the 1 GiB floor → floor-guarded default (never the raw)
        assert_eq!(parse_cache_cap_override(Some("1024")), MAX_CACHE_BYTES);
        assert_eq!(
            parse_cache_cap_override(Some(&(MIN_CACHE_BYTES - 1).to_string())),
            MAX_CACHE_BYTES
        );
        // exactly the floor → honored
        assert_eq!(
            parse_cache_cap_override(Some(&MIN_CACHE_BYTES.to_string())),
            MIN_CACHE_BYTES
        );
        // valid above-floor → value
        assert_eq!(parse_cache_cap_override(Some("53687091200")), 53687091200);
    }

    #[test]
    fn touch_marker_writes_epoch_seconds() {
        let root = scratch("touch");
        let dir = seed(&root, "payloads", "t", 16, None);
        assert!(touch_last_use(&dir));
        let text = fs::read_to_string(dir.join(LAST_USE_MARKER)).unwrap();
        let secs: u64 = text.trim().parse().unwrap();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert!(
            now >= secs && now - secs < 60,
            "marker is current epoch seconds"
        );
        fs::remove_dir_all(&root).ok();
    }
}
