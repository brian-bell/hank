//! Hub lifecycle: create the bd aggregation workspace on first run and
//! reconcile the roster into it on every run.
//!
//! The hub is a bd "hub" workspace under hank's data dir
//! (`<data_dir>/hub`). [`ensure_hub`] initializes it once, then registers each
//! roster repo the hub does not already track; missing roster paths warn rather
//! than fail. [`reset`] deletes the hub dir, guarded so it can only ever remove
//! a path inside the data dir.
//!
//! Reading the hub's current roster goes through `<hub>/.beads/config.yaml`
//! `repos.additional`, not `bd repo list --json`: bd 1.1.0 ignores `--json` for
//! `repo list` and prints human-readable text (see `plans/slices/slice-3.md`).

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::bd::{BdClient, BdError};
use crate::config::{Config, Paths};

/// The bd prefix used for the hub workspace.
const HUB_PREFIX: &str = "hub";

/// Non-fatal outcome of [`ensure_hub`]: the hub is ready, but these roster
/// issues (e.g. a repo path that does not exist on disk) deserve surfacing.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct HubStatus {
    /// Human-readable warnings for display in the TUI status bar.
    pub warnings: Vec<String>,
}

/// One desired roster entry as observed for reconciliation. Reachability is
/// part of the identity so a path becoming present invalidates retained state.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum DesiredRepo {
    Missing {
        absolute_lexical_path: PathBuf,
    },
    Unavailable {
        absolute_lexical_path: PathBuf,
        detail: String,
    },
    Reachable {
        canonical_path: PathBuf,
    },
}

/// Cheap desired/actual hub state used to decide whether a TUI refresh may
/// safely skip `ensure_hub`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconcileWitness {
    pub desired: Vec<DesiredRepo>,
    pub hub_initialized: bool,
    pub actual_repos: Vec<PathBuf>,
}

impl ReconcileWitness {
    /// Whether the observed hub is fully initialized and contains every
    /// currently reachable desired repository. Missing desired paths are
    /// intentionally allowed: `ensure_hub` can only warn about them.
    pub fn is_satisfied(&self) -> bool {
        self.hub_initialized
            && self.desired.iter().all(|desired| match desired {
                DesiredRepo::Missing { .. } | DesiredRepo::Unavailable { .. } => true,
                DesiredRepo::Reachable { canonical_path } => {
                    self.actual_repos.contains(canonical_path)
                }
            })
    }

    pub fn warnings(&self) -> Vec<String> {
        self.desired
            .iter()
            .filter_map(|desired| match desired {
                DesiredRepo::Missing {
                    absolute_lexical_path,
                } => Some(format!(
                    "roster path does not exist: {}",
                    absolute_lexical_path.display()
                )),
                DesiredRepo::Unavailable {
                    absolute_lexical_path,
                    detail,
                } => Some(format!(
                    "roster path is unavailable: {}: {detail}",
                    absolute_lexical_path.display()
                )),
                DesiredRepo::Reachable { .. } => None,
            })
            .collect()
    }
}

/// A fatal hub-lifecycle failure.
#[derive(Debug, thiserror::Error)]
pub enum HubError {
    /// A `bd` invocation failed.
    #[error(transparent)]
    Bd(#[from] BdError),
    /// A filesystem operation failed.
    #[error("filesystem error at {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// `config.yaml` was present but could not be parsed.
    #[error("parsing hub config {path}: {source}")]
    Config {
        path: PathBuf,
        source: serde_yaml::Error,
    },
    /// A reset target escaped the data dir; refused as unsafe.
    #[error("refusing to reset {target}: not inside data dir {data_dir}")]
    UnsafeResetPath { target: PathBuf, data_dir: PathBuf },
}

/// The hub workspace directory: `<data_dir>/hub`.
pub fn hub_dir(paths: &Paths) -> PathBuf {
    paths.data_dir().join("hub")
}

/// Observe both the desired roster and the hub's actual initialized roster.
pub fn reconcile_witness(paths: &Paths, roster: &Config) -> Result<ReconcileWitness, HubError> {
    let mut desired = Vec::new();
    let mut seen = HashSet::new();
    for entry in &roster.repos {
        let resolved_path = paths.resolve_roster_path(&entry.path);
        let observed = match fs::canonicalize(&resolved_path) {
            Ok(canonical_path) => DesiredRepo::Reachable { canonical_path },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => DesiredRepo::Missing {
                absolute_lexical_path: resolved_path,
            },
            Err(source) => DesiredRepo::Unavailable {
                absolute_lexical_path: resolved_path,
                detail: source.to_string(),
            },
        };
        if seen.insert(observed.clone()) {
            desired.push(observed);
        }
    }

    let hub = hub_dir(paths);
    let hub_initialized = is_initialized(&hub);
    let mut actual_repos = if hub_initialized {
        read_hub_roster(&hub)?
            .into_iter()
            .map(|path| normalize(&resolve_against(&hub, &path)))
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    actual_repos.sort();
    actual_repos.dedup();
    Ok(ReconcileWitness {
        desired,
        hub_initialized,
        actual_repos,
    })
}

/// Ensure the hub exists and tracks every reachable roster repo.
///
/// Initializes the hub once when missing, then `repo add`s each roster entry the
/// hub does not already track. A roster path that does not exist on disk yields
/// a warning and is skipped, never a hard error.
pub fn ensure_hub(
    bd: &impl BdClient,
    paths: &Paths,
    roster: &Config,
) -> Result<HubStatus, HubError> {
    let hub = hub_dir(paths);
    mkdir_all(&hub)?;

    // Single-process reconciliation: this check-then-init (and the roster
    // read/add below) is not guarded against a second hank running concurrently.
    // Cross-process safety is the Slice 4 hub lock's job (see the master plan's
    // process-level locking design); ensure_hub stays simple here and surfaces a
    // genuine init failure rather than masking it behind a partial `.beads`.
    if !is_initialized(&hub) {
        bd.init(&hub, HUB_PREFIX)?;
    }

    // bd stores additional repos relative to the hub (`bd -C <hub>`), so resolve
    // any relative stored entry against `hub` — not hank's cwd — before comparing.
    let mut existing: HashSet<PathBuf> = read_hub_roster(&hub)?
        .iter()
        .map(|p| normalize(&resolve_against(&hub, p)))
        .collect();

    let mut warnings = Vec::new();
    for entry in &roster.repos {
        let resolved_path = paths.resolve_roster_path(&entry.path);
        if !resolved_path.exists() {
            warnings.push(format!(
                "roster path does not exist: {}",
                resolved_path.display()
            ));
            continue;
        }
        let canonical = normalize(&resolved_path);
        // Skip repos the hub already tracks — including ones added earlier in
        // this same run, so duplicate/aliased roster entries add exactly once.
        if !existing.insert(canonical.clone()) {
            continue;
        }
        bd.repo_add(&hub, &canonical)?;
    }

    Ok(HubStatus { warnings })
}

/// Delete the hub directory, but only after proving it is inside the data dir.
///
/// A no-op (still `Ok`) when the hub does not exist.
pub fn reset(paths: &Paths) -> Result<(), HubError> {
    let hub = hub_dir(paths);
    ensure_within(paths.data_dir(), &hub)?;
    // `symlink_metadata` does not follow symlinks, so a *dangling* symlink at the
    // hub path is still detected (unlike `Path::exists`) and removed — otherwise
    // reset would silently no-op and the stale link would block the next
    // `ensure_hub` from recreating the directory.
    let meta = match fs::symlink_metadata(&hub) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => return Err(HubError::Io { path: hub, source }),
    };
    // Remove a real directory recursively; unlink a symlink (removing the link,
    // never its target) or a stray file squatting the path.
    let result = if meta.is_dir() {
        fs::remove_dir_all(&hub)
    } else {
        fs::remove_file(&hub)
    };
    result.map_err(|source| HubError::Io {
        path: hub.clone(),
        source,
    })?;
    Ok(())
}

/// The hub's registered additional repos, read from `<hub>/.beads/config.yaml`
/// `repos.additional`. Absent file or absent `repos:` key ⇒ empty (not an error).
pub fn read_hub_roster(hub: &Path) -> Result<Vec<PathBuf>, HubError> {
    let config_path = hub.join(".beads").join("config.yaml");
    let text = match fs::read_to_string(&config_path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(HubError::Io {
                path: config_path,
                source,
            });
        }
    };
    // A fresh `bd init` writes an all-comment template (empty YAML document);
    // parse as Option so that deserializes to None rather than erroring.
    let parsed: Option<HubConfig> =
        serde_yaml::from_str(&text).map_err(|source| HubError::Config {
            path: config_path,
            source,
        })?;
    let additional = parsed
        .and_then(|c| c.repos)
        .map(|r| r.additional)
        .unwrap_or_default();
    Ok(additional.into_iter().map(PathBuf::from).collect())
}

/// The subset of `<hub>/.beads/config.yaml` hank reads. Every field is optional so
/// both the commented template (fresh init) and the minimal active block
/// (post `repo add`) parse.
#[derive(Debug, Deserialize)]
struct HubConfig {
    #[serde(default)]
    repos: Option<HubRepos>,
}

#[derive(Debug, Default, Deserialize)]
struct HubRepos {
    #[serde(default)]
    additional: Vec<String>,
}

/// Reject any reset `target` that is not a strict descendant of `parent`.
/// Component-wise (`Path::starts_with`), so `/data-evil` does not match `/data`.
fn ensure_within(parent: &Path, target: &Path) -> Result<(), HubError> {
    if target != parent && target.starts_with(parent) {
        return Ok(());
    }
    Err(HubError::UnsafeResetPath {
        target: target.to_path_buf(),
        data_dir: parent.to_path_buf(),
    })
}

/// True if `hub` is a fully initialized beads workspace.
///
/// Keys on the embedded Dolt database dir (`.beads/embeddeddolt`), not merely
/// `.beads/`, to mirror bd's own "already initialized" criterion: a re-run of
/// `bd init` aborts citing the existing `.beads/embeddeddolt/<db>`. A bare or
/// partial `.beads/` left by an interrupted `bd init` therefore correctly reads
/// as uninitialized, so `ensure_hub` re-inits instead of reporting a broken hub
/// as ready. (hank always uses the default embedded backend — see the plan.)
fn is_initialized(hub: &Path) -> bool {
    hub.join(".beads").join("embeddeddolt").is_dir()
}

/// Canonicalize `p` if it exists on disk; otherwise return it unchanged. Used to
/// compare roster entries against the hub's stored (absolute) paths robustly.
fn normalize(p: &Path) -> PathBuf {
    fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// Resolve a possibly-relative path against `base`. Absolute paths pass through;
/// relative ones are joined onto `base`. bd stores hub-roster entries relative to
/// the hub dir, so stored entries must be resolved against it before comparison.
fn resolve_against(base: &Path, p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    }
}

fn mkdir_all(dir: &Path) -> Result<(), HubError> {
    fs::create_dir_all(dir).map_err(|source| HubError::Io {
        path: dir.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bd::{Call, FakeBdClient};
    use crate::config::RepoEntry;
    use std::path::Path;

    /// Write a minimal hub `config.yaml` under `hub` with the given additional
    /// repos, and create the embedded-Dolt dir so [`is_initialized`] sees a fully
    /// initialized workspace (as a real `bd init` would leave it).
    fn seed_hub_config(hub: &Path, additional: &[&Path]) {
        let beads = hub.join(".beads");
        fs::create_dir_all(beads.join("embeddeddolt")).unwrap();
        let mut yaml = String::from("repos:\n  primary: \".\"\n  additional:\n");
        for p in additional {
            yaml.push_str(&format!("    - \"{}\"\n", p.display()));
        }
        fs::write(beads.join("config.yaml"), yaml).unwrap();
    }

    /// A real, existing repo dir under `base` (so `path.exists()` is true and
    /// `canonicalize` succeeds).
    fn make_repo(base: &Path, name: &str) -> PathBuf {
        let p = base.join(name);
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn roster(paths: &[&Path]) -> Config {
        Config {
            repos: paths
                .iter()
                .map(|p| RepoEntry {
                    path: p.to_path_buf(),
                })
                .collect(),
        }
    }

    #[test]
    fn creates_hub_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let fake = FakeBdClient::new();

        ensure_hub(&fake, &paths, &Config::default()).unwrap();

        // Exactly one init, targeting the hub dir with the "hub" prefix.
        let inits: Vec<Call> = fake
            .calls()
            .into_iter()
            .filter(|c| matches!(c, Call::Init(_, _)))
            .collect();
        assert_eq!(inits.len(), 1);
        assert!(matches!(
            &inits[0],
            Call::Init(dir, prefix) if dir == &hub_dir(&paths) && prefix == HUB_PREFIX
        ));
    }

    #[test]
    fn skips_init_when_hub_already_initialized() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        seed_hub_config(&hub_dir(&paths), &[]);
        let fake = FakeBdClient::new();

        ensure_hub(&fake, &paths, &Config::default()).unwrap();

        assert!(
            !fake.calls().iter().any(|c| matches!(c, Call::Init(_, _))),
            "init must be skipped when the hub is already initialized"
        );
    }

    #[test]
    fn adds_missing_repos_only() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let ra = make_repo(tmp.path(), "ra");
        let rb = make_repo(tmp.path(), "rb");
        // Hub already tracks `ra` (canonicalized, as bd would store it).
        seed_hub_config(&hub_dir(&paths), &[&normalize(&ra)]);
        let fake = FakeBdClient::new();

        ensure_hub(&fake, &paths, &roster(&[&ra, &rb])).unwrap();

        let adds: Vec<PathBuf> = fake
            .calls()
            .into_iter()
            .filter_map(|c| match c {
                Call::RepoAdd(_, p) => Some(p),
                _ => None,
            })
            .collect();
        assert_eq!(
            adds,
            vec![normalize(&rb)],
            "only the untracked repo is added"
        );
    }

    #[test]
    fn tolerates_absent_repo_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        seed_hub_config(&hub_dir(&paths), &[]);
        let missing = tmp.path().join("gone");
        let fake = FakeBdClient::new();

        let status = ensure_hub(&fake, &paths, &roster(&[&missing])).unwrap();

        assert!(
            status.warnings.iter().any(|w| w.contains("gone")),
            "absent path should warn, got {:?}",
            status.warnings
        );
        assert!(
            !fake
                .calls()
                .iter()
                .any(|c| matches!(c, Call::RepoAdd(_, _))),
            "absent path must not be repo_add'd"
        );
    }

    #[test]
    fn ensure_hub_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let ra = make_repo(tmp.path(), "ra");
        // Hub already tracks `ra`; two runs must add nothing.
        seed_hub_config(&hub_dir(&paths), &[&normalize(&ra)]);
        let fake = FakeBdClient::new();

        ensure_hub(&fake, &paths, &roster(&[&ra])).unwrap();
        ensure_hub(&fake, &paths, &roster(&[&ra])).unwrap();

        assert!(
            !fake
                .calls()
                .iter()
                .any(|c| matches!(c, Call::RepoAdd(_, _))),
            "an already-tracked repo must never be re-added"
        );
    }

    #[test]
    fn dedupes_duplicate_roster_entries_within_one_run() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let ra = make_repo(tmp.path(), "ra");
        seed_hub_config(&hub_dir(&paths), &[]);
        let fake = FakeBdClient::new();

        // Same untracked repo listed twice must be added exactly once.
        ensure_hub(&fake, &paths, &roster(&[&ra, &ra])).unwrap();

        let adds = fake
            .calls()
            .into_iter()
            .filter(|c| matches!(c, Call::RepoAdd(_, _)))
            .count();
        assert_eq!(adds, 1, "duplicate roster entry must add only once");
    }

    #[test]
    fn treats_hub_relative_stored_entry_as_tracked() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let hub = hub_dir(&paths);
        // `ra` sits beside the hub; the hub stores it as the relative `../ra`,
        // which bd interprets against the hub dir.
        let ra = make_repo(paths.data_dir(), "ra");
        fs::create_dir_all(hub.join(".beads")).unwrap();
        fs::write(
            hub.join(".beads").join("config.yaml"),
            "repos:\n  primary: \".\"\n  additional:\n    - \"../ra\"\n",
        )
        .unwrap();
        let fake = FakeBdClient::new();

        // Roster names the absolute repo; it must be recognized as already
        // tracked despite the stored entry being hub-relative.
        ensure_hub(&fake, &paths, &roster(&[&ra])).unwrap();

        assert!(
            !fake
                .calls()
                .iter()
                .any(|c| matches!(c, Call::RepoAdd(_, _))),
            "a hub-relative stored entry must match the absolute roster path"
        );
    }

    #[test]
    fn reinitializes_when_beads_dir_is_partial() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        // A `.beads/` left by an interrupted init, with no embedded Dolt db.
        fs::create_dir_all(hub_dir(&paths).join(".beads")).unwrap();
        let fake = FakeBdClient::new();

        ensure_hub(&fake, &paths, &Config::default()).unwrap();

        assert_eq!(
            fake.calls()
                .iter()
                .filter(|c| matches!(c, Call::Init(_, _)))
                .count(),
            1,
            "a partial .beads dir (no db) must be treated as uninitialized"
        );
    }

    #[test]
    fn init_failure_propagates() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        // A genuine init failure must surface, not be silently swallowed.
        let fake = FakeBdClient::new().with_init_err(BdError {
            command: "bd init".into(),
            stderr: "disk full".into(),
            kind: crate::bd::BdErrorKind::NonZeroExit { code: Some(1) },
        });

        assert!(ensure_hub(&fake, &paths, &Config::default()).is_err());
    }

    #[test]
    fn reset_guard_rejects_path_outside_data_dir() {
        let data = Path::new("/data/hank");
        assert!(ensure_within(data, Path::new("/etc")).is_err());
        assert!(ensure_within(data, &data.join("hub")).is_ok());
        // Equal path is refused (would remove the data dir itself).
        assert!(ensure_within(data, data).is_err());
        // Sibling with a shared string prefix must not pass.
        assert!(ensure_within(data, Path::new("/data/hank-evil")).is_err());
    }

    #[test]
    fn reset_removes_hub_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let hub = hub_dir(&paths);
        fs::create_dir_all(hub.join(".beads")).unwrap();
        fs::write(hub.join(".beads").join("marker"), "x").unwrap();

        reset(&paths).unwrap();

        assert!(!hub.exists(), "hub dir removed");
        assert!(paths.data_dir().exists(), "data dir preserved");
        // A second reset on the now-absent hub is a no-op.
        reset(&paths).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn reset_removes_dangling_hub_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let hub = hub_dir(&paths);
        fs::create_dir_all(hub.parent().unwrap()).unwrap();
        // A symlink to a nonexistent target: `Path::exists()` reports false, but
        // it still occupies the hub path and must be cleared.
        std::os::unix::fs::symlink(tmp.path().join("nowhere"), &hub).unwrap();
        assert!(!hub.exists(), "precondition: dangling symlink");

        reset(&paths).unwrap();

        assert!(
            fs::symlink_metadata(&hub).is_err(),
            "dangling symlink at hub path must be removed"
        );
    }

    #[test]
    fn read_hub_roster_parses_additional() {
        let tmp = tempfile::tempdir().unwrap();
        let hub = tmp.path().join("hub");
        let ra = Path::new("/tmp/ra");
        let rb = Path::new("/tmp/rb");
        seed_hub_config(&hub, &[ra, rb]);

        let got = read_hub_roster(&hub).unwrap();

        assert_eq!(got, vec![ra.to_path_buf(), rb.to_path_buf()]);
    }

    #[test]
    fn read_hub_roster_empty_when_no_repos_key() {
        let tmp = tempfile::tempdir().unwrap();
        let hub = tmp.path().join("hub");
        let beads = hub.join(".beads");
        fs::create_dir_all(&beads).unwrap();
        // A commented-out template: valid YAML, but no active `repos:` key.
        fs::write(
            beads.join("config.yaml"),
            "# Beads Configuration File\n# repos:\n#   additional: []\n",
        )
        .unwrap();

        assert_eq!(read_hub_roster(&hub).unwrap(), Vec::<PathBuf>::new());
    }

    #[test]
    fn read_hub_roster_missing_file_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let hub = tmp.path().join("hub");
        assert_eq!(read_hub_roster(&hub).unwrap(), Vec::<PathBuf>::new());
    }

    #[test]
    fn reconcile_witness_resolves_relative_roster_paths_against_config_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let roster = Config {
            repos: vec![RepoEntry {
                path: PathBuf::from("later"),
            }],
        };

        let witness = reconcile_witness(&paths, &roster).unwrap();

        assert_eq!(
            witness.desired,
            vec![DesiredRepo::Missing {
                absolute_lexical_path: paths
                    .config_file()
                    .parent()
                    .expect("config file has a parent")
                    .join("later"),
            }]
        );
    }

    #[cfg(unix)]
    #[test]
    fn reconcile_witness_keeps_unavailable_roster_paths_nonfatal() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        seed_hub_config(&hub_dir(&paths), &[]);
        let repo = tmp.path().join("loop");
        std::os::unix::fs::symlink(&repo, &repo).unwrap();
        let roster = roster(&[&repo]);

        let witness = reconcile_witness(&paths, &roster).expect("unavailable repo is degraded");

        assert!(witness.is_satisfied());
        assert!(
            witness.warnings()[0].contains("unavailable"),
            "the reachability failure is surfaced as a warning"
        );
    }

    #[test]
    fn reconcile_witness_ignores_malformed_roster_for_partial_hub() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let beads = hub_dir(&paths).join(".beads");
        fs::create_dir_all(&beads).unwrap();
        fs::write(beads.join("config.yaml"), "repos: [not valid").unwrap();

        let witness =
            reconcile_witness(&paths, &Config::default()).expect("partial hub remains repairable");

        assert!(!witness.hub_initialized);
        assert!(witness.actual_repos.is_empty());
    }

    #[test]
    fn ensure_hub_resolves_relative_roster_paths_against_config_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        seed_hub_config(&hub_dir(&paths), &[]);
        let config_dir = paths
            .config_file()
            .parent()
            .expect("config file has a parent");
        let repo = make_repo(config_dir, "repo");
        let roster = Config {
            repos: vec![RepoEntry {
                path: PathBuf::from("repo"),
            }],
        };
        let fake = FakeBdClient::new();

        let status = ensure_hub(&fake, &paths, &roster).unwrap();

        assert!(status.warnings.is_empty());
        let canonical_repo = fs::canonicalize(repo).unwrap();
        assert!(
            fake.calls()
                .iter()
                .any(|call| matches!(call, Call::RepoAdd(_, actual) if actual == &canonical_repo))
        );
    }

    #[test]
    fn reconcile_witness_changes_when_missing_repo_becomes_present() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let repo = tmp.path().join("later");
        let roster = Config {
            repos: vec![RepoEntry { path: repo.clone() }],
        };

        let missing = reconcile_witness(&paths, &roster).unwrap();
        fs::create_dir_all(&repo).unwrap();
        let present = reconcile_witness(&paths, &roster).unwrap();

        assert_ne!(missing, present);
        assert!(!missing.is_satisfied());
    }
}
