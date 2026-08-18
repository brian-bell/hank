//! A local, on-disk cache of the last successful [`Snapshot`], so launch can
//! paint the ready list instantly from disk while a real refresh runs in the
//! background instead of sitting in `Loading` until `bd ready` returns.
//!
//! Freshness is judged by the snapshot's own embedded `fetched_at`, not the
//! file's mtime, so the cache and the "last refreshed" age the UI renders
//! always agree. A cache miss (missing file, corrupt JSON, stale data, or a
//! roster mismatch) is never an error to the caller — it just means the
//! ordinary `Loading` boot runs, exactly as if no cache module existed.

use std::fs::{self, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::snapshot::{Snapshot, UNKNOWN_REPO};

/// A cached snapshot older than this is not loaded at startup — better to wait
/// for one real refresh than paint a half-day-stale ready list.
pub const MAX_AGE: Duration = Duration::from_secs(12 * 60 * 60);

/// Cache schema carrying stable repository identities on attributed rows.
const CACHE_VERSION: u64 = 1;

/// The on-disk cache payload: the snapshot plus the roster it was fetched
/// under. `roster` lets [`load`] reject a cache written before a `repos add`/
/// `remove` — otherwise a cache from before a removed repo's entry would show
/// that repo's rows for up to [`MAX_AGE`] regardless of the roster change.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheFile {
    version: u64,
    roster: Config,
    snapshot: Snapshot,
    repo_ids: Vec<Option<String>>,
}

/// Load the snapshot cached at `path` if it exists, parses, was written under
/// `roster` (bytewise `==`, so a `repos add`/`remove`/reorder invalidates it),
/// and its embedded `fetched_at` is within [`MAX_AGE`] of `now`. Any failure —
/// missing file, corrupt JSON, a roster mismatch, a legacy attributed row that
/// predates stable repository identities, or a `fetched_at` more than `MAX_AGE`
/// before (or after, guarding against clock skew) `now` — yields `None`
/// silently.
pub fn load(path: &Path, now: SystemTime, roster: &Config) -> Option<Snapshot> {
    let cached = read(path)?;
    if cached.version != CACHE_VERSION {
        return None;
    }
    if &cached.roster != roster {
        return None;
    }
    if cached
        .snapshot
        .rows
        .iter()
        .any(|row| row.repo_id.is_none() && row.repo_name != UNKNOWN_REPO)
    {
        return None;
    }
    let age = now.duration_since(cached.snapshot.fetched_at).ok()?;
    if age > MAX_AGE {
        return None;
    }
    Some(cached.snapshot)
}

/// Persist `snapshot` (fetched under `roster`) to `path` as JSON, creating
/// parent directories as needed. Best-effort: the caller treats a write
/// failure (e.g. a read-only data dir) as non-fatal to the refresh cycle that
/// produced the snapshot.
///
/// Writes to a same-directory temp file and renames it over `path`, so a
/// reader always sees either the old or the new cache in full, never a
/// partial/interleaved write — the same atomic-replace pattern
/// [`crate::config::Config::save`] uses for `config.toml`.
///
/// Two hank instances can each hold the hub lock only during their own sync
/// (see `refresh::run`), so their later, lock-free `bd ready` reads and cache
/// writes can finish out of order. An exclusive OS lock (mirroring
/// `refresh::HubLock`, but blocking rather than declining) is held across the
/// read-compare-write sequence below, and the write is skipped as a no-op
/// when an existing on-disk cache's `fetched_at` is already at or after
/// `snapshot`'s, so the cache stays monotonic in `fetched_at` even under a
/// racing writer.
pub fn save(path: &Path, snapshot: &Snapshot, roster: &Config) -> io::Result<()> {
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
    if let Some(parent) = parent {
        fs::create_dir_all(parent)?;
    }

    let lock_file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(lock_path(path))?;
    lock_file.lock_exclusive()?;

    if let Some(existing) = read(path)
        && existing.version == CACHE_VERSION
        && existing.snapshot.fetched_at >= snapshot.fetched_at
    {
        return Ok(());
    }

    let bytes = serde_json::to_vec(&CacheFile {
        version: CACHE_VERSION,
        roster: roster.clone(),
        snapshot: snapshot.clone(),
        repo_ids: snapshot
            .rows
            .iter()
            .map(|row| row.repo_id.clone())
            .collect(),
    })?;

    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "snapshot_cache.json".to_string());
    let tmp_name = format!(".{}.tmp.{}", file_name, std::process::id());
    let tmp_path = match parent {
        Some(parent) => parent.join(tmp_name),
        None => PathBuf::from(tmp_name),
    };

    fs::write(&tmp_path, bytes)?;
    fs::rename(&tmp_path, path)
    // `lock_file` drops here, releasing the flock.
}

/// Parse whatever is currently at `path` into a [`CacheFile`], ignoring
/// [`MAX_AGE`]/roster matching (unlike [`load`]) — [`save`]'s monotonicity
/// check needs the raw stored snapshot regardless of staleness or roster.
/// `None` for a missing/corrupt file.
fn read(path: &Path) -> Option<CacheFile> {
    let bytes = fs::read(path).ok()?;
    let mut cached: CacheFile = serde_json::from_slice(&bytes).ok()?;
    if cached.repo_ids.len() != cached.snapshot.rows.len() {
        return None;
    }
    for (row, repo_id) in cached.snapshot.rows.iter_mut().zip(&cached.repo_ids) {
        row.repo_id = repo_id.clone();
    }
    Some(cached)
}

/// The sibling lock file [`save`] holds across its read-compare-write
/// sequence, named after `path` so concurrent writers to different cache
/// paths (e.g. under different injected test roots) never contend.
fn lock_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "snapshot_cache.json".to_string());
    let lock_name = format!("{file_name}.lock");
    match path.parent().filter(|p| !p.as_os_str().is_empty()) {
        Some(parent) => parent.join(lock_name),
        None => PathBuf::from(lock_name),
    }
}

/// Remove the cache file at `path`, if present. A missing file is not an
/// error — `reset` calls this unconditionally alongside deleting the hub, so
/// a launch just after `hank reset` never paints rows from the discarded hub
/// (see [`crate::hub::reset`]).
pub fn clear(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RepoEntry;
    use crate::snapshot::Row;
    use std::time::UNIX_EPOCH;

    fn snapshot_at(fetched_at: SystemTime) -> Snapshot {
        Snapshot {
            rows: vec![Row {
                issue: crate::bd::Issue {
                    id: "ra-1".into(),
                    title: "t".into(),
                    status: "open".into(),
                    priority: 1,
                    description: None,
                    issue_type: None,
                    owner: None,
                    labels: Vec::new(),
                    created_at: None,
                    created_by: None,
                    updated_at: None,
                    dependency_count: None,
                    dependent_count: None,
                    comment_count: None,
                },
                repo_id: Some("ra".into()),
                repo_name: "repo-a".into(),
                attribution_generation: None,
            }],
            fetched_at,
        }
    }

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn roster(paths: &[&str]) -> Config {
        Config {
            repos: paths.iter().map(|p| RepoEntry { path: p.into() }).collect(),
        }
    }

    #[test]
    fn round_trips_a_fresh_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("snapshot_cache.json");
        let snapshot = snapshot_at(at(1_000_000));
        let roster = roster(&["/dev/repo-a"]);

        save(&path, &snapshot, &roster).expect("save ok");
        let loaded =
            load(&path, at(1_000_000) + Duration::from_secs(60), &roster).expect("fresh load");

        assert_eq!(loaded, snapshot);
    }

    #[test]
    fn rejects_legacy_attributed_rows_without_stable_repo_identity() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("snapshot_cache.json");
        let mut snapshot = snapshot_at(at(1_000_000));
        snapshot.rows[0].repo_id = None;
        let roster = roster(&["/dev/repo-a"]);

        save(&path, &snapshot, &roster).expect("save legacy-shaped cache");

        assert!(
            load(&path, at(1_000_000) + Duration::from_secs(60), &roster).is_none(),
            "display-only attributed rows must not hydrate a persistable picker identity"
        );
    }

    #[test]
    fn rejects_versionless_legacy_cache_even_when_label_is_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("snapshot_cache.json");
        let mut snapshot = snapshot_at(at(1_000_000));
        snapshot.rows[0].repo_id = None;
        snapshot.rows[0].repo_name = UNKNOWN_REPO.into();
        let roster = roster(&["/dev/unknown"]);
        let legacy = serde_json::json!({
            "roster": roster,
            "snapshot": snapshot,
        });
        fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();

        assert!(
            load(&path, at(1_000_000) + Duration::from_secs(60), &roster).is_none(),
            "the sentinel label cannot substitute for cache schema provenance"
        );
    }

    #[test]
    fn accepts_unattributed_unknown_rows_without_repo_identity() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("snapshot_cache.json");
        let mut snapshot = snapshot_at(at(1_000_000));
        snapshot.rows[0].repo_id = None;
        snapshot.rows[0].repo_name = UNKNOWN_REPO.into();
        let roster = roster(&["/dev/repo-a"]);

        save(&path, &snapshot, &roster).expect("save unknown row");

        assert_eq!(
            load(&path, at(1_000_000) + Duration::from_secs(60), &roster),
            Some(snapshot),
            "new snapshots legitimately omit identities for unattributed rows"
        );
    }

    #[test]
    fn rejects_a_snapshot_older_than_max_age() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("snapshot_cache.json");
        let snapshot = snapshot_at(at(1_000_000));
        let roster = roster(&["/dev/repo-a"]);
        save(&path, &snapshot, &roster).expect("save ok");

        let just_stale = at(1_000_000) + MAX_AGE + Duration::from_secs(1);
        assert!(
            load(&path, just_stale, &roster).is_none(),
            "past MAX_AGE is a miss"
        );

        let just_fresh = at(1_000_000) + MAX_AGE;
        assert!(
            load(&path, just_fresh, &roster).is_some(),
            "exactly MAX_AGE is a hit"
        );
    }

    #[test]
    fn rejects_a_fetched_at_in_the_future() {
        // Clock skew (or a corrupted timestamp) should not be trusted as fresh.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("snapshot_cache.json");
        let snapshot = snapshot_at(at(1_000_000));
        let roster = roster(&["/dev/repo-a"]);
        save(&path, &snapshot, &roster).expect("save ok");

        assert!(load(&path, at(999_999), &roster).is_none());
    }

    #[test]
    fn rejects_a_cache_written_under_a_different_roster() {
        // A `repos add`/`remove` between the write and the load must miss,
        // so a launch never shows rows from a repo the roster no longer has.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("snapshot_cache.json");
        let snapshot = snapshot_at(at(1_000_000));
        save(&path, &snapshot, &roster(&["/dev/repo-a"])).expect("save ok");

        let now = at(1_000_000) + Duration::from_secs(60);
        assert!(
            load(&path, now, &roster(&["/dev/repo-a", "/dev/repo-b"])).is_none(),
            "an added repo invalidates the cache"
        );
        assert!(
            load(&path, now, &roster(&["/dev/repo-b"])).is_none(),
            "a swapped repo invalidates the cache"
        );
        assert!(
            load(&path, now, &roster(&["/dev/repo-a"])).is_some(),
            "an unchanged roster still hits"
        );
    }

    #[test]
    fn missing_file_is_a_silent_miss() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("does-not-exist.json");
        assert!(load(&path, at(0), &roster(&["/dev/repo-a"])).is_none());
    }

    #[test]
    fn corrupt_json_is_a_silent_miss() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("snapshot_cache.json");
        fs::write(&path, b"not json").unwrap();
        assert!(load(&path, at(0), &roster(&["/dev/repo-a"])).is_none());
    }

    #[test]
    fn save_creates_missing_parent_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nested").join("snapshot_cache.json");
        let snapshot = snapshot_at(at(0));

        save(&path, &snapshot, &roster(&["/dev/repo-a"])).expect("save creates parent dirs");
        assert!(path.exists());
    }

    #[test]
    fn save_leaves_no_leftover_temp_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("snapshot_cache.json");
        let roster = roster(&["/dev/repo-a"]);
        save(&path, &snapshot_at(at(0)), &roster).expect("save ok");

        let entries: Vec<_> = fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            entries.iter().all(|n| !n.contains(".tmp.")),
            "no leftover .tmp file: {entries:?}"
        );
    }

    #[test]
    fn save_never_regresses_a_newer_cache() {
        // Simulates two hank instances whose syncs finish in one order but
        // whose (lock-free) `bd ready` reads and cache writes land in the
        // other: the older snapshot's write must not clobber the newer one.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("snapshot_cache.json");
        let roster = roster(&["/dev/repo-a"]);
        let newer = snapshot_at(at(2_000_000));
        let older = snapshot_at(at(1_000_000));

        save(&path, &newer, &roster).expect("save ok");
        save(&path, &older, &roster).expect("save ok (no-op)");

        let on_disk = load(&path, at(2_000_000), &roster).expect("still a hit");
        assert_eq!(
            on_disk, newer,
            "the older write did not overwrite the newer cache"
        );
    }

    #[test]
    fn clear_removes_an_existing_cache_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("snapshot_cache.json");
        save(&path, &snapshot_at(at(0)), &roster(&["/dev/repo-a"])).expect("save ok");

        clear(&path).expect("clear ok");
        assert!(!path.exists());
    }

    #[test]
    fn clear_is_a_silent_no_op_when_the_file_is_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("does-not-exist.json");
        clear(&path).expect("missing file is not an error");
    }
}
