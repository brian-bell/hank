//! Refresh pipeline: export every roster repo, sync the hub once, and build a
//! prefix→repo attribution map — collecting per-repo failures instead of
//! aborting on the first bad repo.
//!
//! A process-level advisory lock on `<hub>/.hank.lock` serializes refreshes
//! across concurrent hank instances so two cannot run `repo sync` against the
//! same embedded-Dolt hub at once. See `plans/slices/slice-4.md`.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fs2::FileExt;
use serde::Deserialize;

use crate::bd::{BdClient, BdError, RepoSyncReport};
use crate::config::{Config, Paths, RepoEntry};
use crate::hub::hub_dir;

/// Advisory lock file, inside the hub dir.
const LOCK_FILE: &str = ".hank.lock";
const GENERATION_FILE: &str = ".hank-generation";

/// A completed refresh. Individual repos may still appear in `errors`; a
/// completed refresh with per-repo errors is still a success (the hub was
/// synced from whatever exported cleanly).
#[derive(Debug)]
pub struct RefreshOutcome {
    /// Id-prefix → source repo attribution built from each repo's metadata.
    pub prefix_map: PrefixMap,
    /// Per-repo operational failures surfaced but not fatal.
    pub errors: Vec<RepoError>,
    /// Wall-clock time the hub sync completed.
    pub synced_at: SystemTime,
    /// Diagnostic classification of the single sync invocation.
    pub sync_report: RepoSyncReport,
    /// Production-neutral phase observations for benchmarks and diagnostics.
    pub metrics: RefreshMetrics,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct RefreshMetrics {
    pub source_wall: Duration,
    pub sync: Duration,
    pub export_calls: usize,
    pub issue_prefix_calls: usize,
}

/// A source prefix that a non-empty, freshly exported JSONL file proved still
/// matches every exported issue id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedRepoPrefix {
    pub normalized_path: PathBuf,
    pub prefix: String,
}

/// Opaque provenance id pairing attributed rows with the immutable prefix map
/// used to build them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AttributionGeneration(u64);

impl AttributionGeneration {
    pub(crate) fn new(value: u64) -> Self {
        Self(value)
    }
}

/// Cross-process equality token pairing the hub contents with an in-process
/// attribution generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HubGenerationToken(String);

impl HubGenerationToken {
    pub(crate) fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub(crate) fn fresh() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        Self::new(format!(
            "{}-{nanos}-{}",
            std::process::id(),
            GENERATION_COUNTER.fetch_add(1, Ordering::Relaxed)
        ))
    }
}

/// Prefix attribution assembled for one successfully synced hub generation.
#[derive(Debug, Clone)]
pub struct AttributionCandidate {
    pub roster_paths: Vec<PathBuf>,
    pub repos: Vec<VerifiedRepoPrefix>,
    pub prefix_map: Arc<PrefixMap>,
}

/// A successfully synced refresh whose advisory hub lock is still held.
#[derive(Debug)]
pub struct SyncedRefresh {
    outcome: RefreshOutcome,
    candidate: AttributionCandidate,
    _lock: HubLock,
}

impl SyncedRefresh {
    pub fn outcome(&self) -> &RefreshOutcome {
        &self.outcome
    }

    pub fn candidate(&self) -> &AttributionCandidate {
        &self.candidate
    }

    pub fn into_outcome(self) -> RefreshOutcome {
        self.outcome
    }
}

/// A per-repo failure during refresh: surfaced to the user but never aborts the
/// whole refresh (other repos still export and the hub still syncs).
#[derive(Debug, Clone, thiserror::Error)]
pub enum RepoError {
    /// This repo's `bd export` failed; the hub still synced without its latest
    /// data, and other repos still hydrate.
    #[error("export failed for {repo}: {source}")]
    Export { repo: PathBuf, source: BdError },
    /// This repo's effective prefix could not be read or verified, so its issues
    /// cannot be attributed.
    #[error("cannot read prefix for {repo}: {detail}")]
    Metadata { repo: PathBuf, detail: String },
    /// Preparing, comparing, cleaning, or atomically publishing an export
    /// failed. The original JSONL is never opened for writing by hank.
    #[error("export {operation} failed for {repo} at {path}: {detail}")]
    ExportFile {
        repo: PathBuf,
        operation: &'static str,
        path: PathBuf,
        detail: String,
    },
}

/// A fatal refresh failure, or a declined refresh.
#[derive(Debug, thiserror::Error)]
pub enum RefreshError {
    /// Another hank instance holds the hub lock; this refresh declined to run and
    /// performed no exports or sync. The caller retries on the next refresh.
    #[error("another hank instance is refreshing this hub")]
    AlreadyRefreshing,
    /// The single `bd repo sync` failed, so the hub was not updated at all.
    #[error("hub sync failed: {0}")]
    Sync(#[source] BdError),
    /// A lock-file IO error (open or `flock`).
    #[error("hub lock error at {path}: {source}")]
    Lock {
        path: PathBuf,
        source: std::io::Error,
    },
    /// Preparing the hub directory failed.
    #[error("preparing hub dir {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// A scoped source worker panicked. All workers have been joined before
    /// this is returned, so no source job remains detached.
    #[error("a source refresh worker panicked")]
    WorkerPanic,
    /// Publishing or reading the derived hub-generation marker failed.
    #[error("hub generation marker error at {path}: {source}")]
    Generation {
        path: PathBuf,
        source: std::io::Error,
    },
}

/// Two or more roster repos declared the same id prefix. Ids under a collided
/// prefix are ambiguous and resolve to `None` (the "unknown" bucket).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Collision {
    pub prefix: String,
    pub repos: Vec<PathBuf>,
}

/// Maps an issue id to its source repo by longest configured prefix followed by
/// `-`. A prefix claimed by more than one repo resolves to `None` (ambiguous)
/// but stays in the lookup table so it can still win the longest-match contest —
/// a shorter unique prefix must never mask a longer, collided one.
#[derive(Debug, Default, Clone)]
pub struct PrefixMap {
    /// Every configured prefix → its resolution: `Some(repo)` when a single repo
    /// claims it, `None` when it collided. Lookup scans for the longest match.
    entries: Vec<(String, Option<RepoEntry>)>,
    /// Prefixes claimed by more than one repo, for surfacing to the user.
    collisions: Vec<Collision>,
}

impl PrefixMap {
    /// Build the map from `(prefix, repo)` pairs. A prefix claimed by more than
    /// one repo becomes a [`Collision`] and a `None` entry; a unique prefix maps
    /// to its repo. First-seen order is preserved for deterministic reporting.
    ///
    /// Public so consumers (e.g. `snapshot`'s tests) can construct a populated
    /// map without running a whole refresh; `run` builds it from the prefixes it
    /// reads from each repo's metadata.
    pub fn from_pairs(pairs: Vec<(String, RepoEntry)>) -> PrefixMap {
        let mut order: Vec<String> = Vec::new();
        let mut grouped: HashMap<String, Vec<RepoEntry>> = HashMap::new();
        for (prefix, repo) in pairs {
            if !grouped.contains_key(&prefix) {
                order.push(prefix.clone());
            }
            grouped.entry(prefix).or_default().push(repo);
        }

        let mut entries = Vec::new();
        let mut collisions = Vec::new();
        for prefix in order {
            let mut repos = grouped.remove(&prefix).expect("prefix was inserted");
            if repos.len() == 1 {
                entries.push((prefix, Some(repos.pop().expect("length checked to be 1"))));
            } else {
                collisions.push(Collision {
                    prefix: prefix.clone(),
                    repos: repos.into_iter().map(|r| r.path).collect(),
                });
                // Keep the collided prefix in the lookup table (as `None`) so it
                // still participates in longest-match; otherwise a shorter unique
                // prefix could wrongly claim an id under the longer collided one.
                entries.push((prefix, None));
            }
        }
        PrefixMap {
            entries,
            collisions,
        }
    }

    /// The repo whose configured prefix, followed by `-`, is the longest prefix
    /// of `id`. `None` when nothing matches, or when the longest matching prefix
    /// is a collided (ambiguous) one.
    pub fn repo_for(&self, id: &str) -> Option<&RepoEntry> {
        self.attribution(id).map(|(_, repo)| repo)
    }

    /// Like [`repo_for`](Self::repo_for) but also yields the matched prefix. The
    /// prefix is a unique, short, non-sensitive repo identity (a collided prefix
    /// resolves to `None` here), useful to disambiguate repos that share a
    /// directory basename without exposing a filesystem path.
    pub fn attribution(&self, id: &str) -> Option<(&str, &RepoEntry)> {
        let (prefix, repo) = self
            .entries
            .iter()
            .filter(|(prefix, _)| {
                id.strip_prefix(prefix.as_str())
                    .is_some_and(|rest| rest.starts_with('-'))
            })
            .max_by_key(|(prefix, _)| prefix.len())?;
        repo.as_ref().map(|repo| (prefix.as_str(), repo))
    }

    /// Prefixes claimed by more than one roster repo.
    pub fn collisions(&self) -> &[Collision] {
        &self.collisions
    }
}

/// An acquired advisory lock on `<hub>/.hank.lock`. The OS lock is released when
/// the held `File` drops (closing the fd releases the `flock`).
#[derive(Debug)]
pub struct HubLock {
    _file: File,
}

impl HubLock {
    /// Try to acquire the hub lock without blocking: `Ok(Some(lock))` on
    /// success, `Ok(None)` when another holder has it, `Err` on a real IO error.
    pub fn try_acquire(hub: &Path) -> Result<Option<HubLock>, RefreshError> {
        let path = hub.join(LOCK_FILE);
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(|source| RefreshError::Lock {
                path: path.clone(),
                source,
            })?;
        match file.try_lock_exclusive() {
            Ok(()) => Ok(Some(HubLock { _file: file })),
            // Contended: another holder (possibly this process via a separate
            // open) has the exclusive lock. Decline rather than block.
            Err(e) if e.kind() == ErrorKind::WouldBlock => Ok(None),
            Err(source) => Err(RefreshError::Lock { path, source }),
        }
    }
}

static GENERATION_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Atomically publish the token for the hub contents while the caller holds the
/// hub lock.
pub(crate) fn publish_hub_generation(
    paths: &Paths,
    token: &HubGenerationToken,
) -> Result<(), RefreshError> {
    publish_hub_generation_with_counter(paths, token, &GENERATION_COUNTER)
}

fn publish_hub_generation_with_counter(
    paths: &Paths,
    token: &HubGenerationToken,
    counter: &AtomicU64,
) -> Result<(), RefreshError> {
    let hub = hub_dir(paths);
    let marker = hub.join(GENERATION_FILE);
    let (mut file, temp) = loop {
        let candidate = hub.join(format!(
            ".hank-generation.{}.{}.tmp",
            std::process::id(),
            counter.fetch_add(1, Ordering::Relaxed)
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => break (file, candidate),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(source) => {
                return Err(RefreshError::Generation {
                    path: marker,
                    source,
                });
            }
        }
    };
    let result = (|| {
        file.write_all(token.0.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temp, &marker)
    })();
    if let Err(source) = result {
        let _ = fs::remove_file(&temp);
        return Err(RefreshError::Generation {
            path: marker,
            source,
        });
    }
    Ok(())
}

/// Read the currently published cross-process hub token.
pub(crate) fn read_hub_generation(
    paths: &Paths,
) -> Result<Option<HubGenerationToken>, RefreshError> {
    let marker = hub_dir(paths).join(GENERATION_FILE);
    match fs::read_to_string(&marker) {
        Ok(value) => Ok(Some(HubGenerationToken(value))),
        Err(source) if source.kind() == ErrorKind::NotFound => Ok(None),
        Err(source) => Err(RefreshError::Generation {
            path: marker,
            source,
        }),
    }
}

/// Run one refresh: export every roster repo with bounded concurrency, sync the hub once,
/// and build the prefix map. Declines with [`RefreshError::AlreadyRefreshing`]
/// if another instance holds the hub lock.
pub fn run(
    bd: &impl BdClient,
    roster: &Config,
    paths: &Paths,
) -> Result<RefreshOutcome, RefreshError> {
    let synced = run_with_state(bd, roster, paths, None, 4)?;
    publish_hub_generation(paths, &HubGenerationToken::fresh())?;
    Ok(synced.into_outcome())
}

/// Internal deterministic seam for the bounded scheduler's tests.
#[cfg(test)]
pub(crate) fn run_with_worker_limit(
    bd: &impl BdClient,
    roster: &Config,
    paths: &Paths,
    worker_limit: usize,
) -> Result<RefreshOutcome, RefreshError> {
    Ok(run_with_state(bd, roster, paths, None, worker_limit)?.into_outcome())
}

/// State-aware refresh entry point. A prior candidate is only a hint: every
/// reused prefix must be re-proved against the current non-empty export.
pub(crate) fn run_with_state(
    bd: &impl BdClient,
    roster: &Config,
    paths: &Paths,
    previous: Option<&AttributionCandidate>,
    worker_limit: usize,
) -> Result<SyncedRefresh, RefreshError> {
    let hub = hub_dir(paths);
    fs::create_dir_all(&hub).map_err(|source| RefreshError::Io {
        path: hub.clone(),
        source,
    })?;
    let lock = match HubLock::try_acquire(&hub)? {
        Some(lock) => lock,
        None => return Err(RefreshError::AlreadyRefreshing),
    };
    let previous_prefixes: HashMap<PathBuf, String> = previous
        .into_iter()
        .flat_map(|candidate| candidate.repos.iter())
        .map(|repo| (repo.normalized_path.clone(), repo.prefix.clone()))
        .collect();
    let jobs = normalized_jobs(roster, paths);
    let roster_paths = jobs
        .iter()
        .map(|job| normalize_path(&job.entry.path))
        .collect();
    let source_started = Instant::now();
    let outcomes = run_source_jobs(bd, jobs, &previous_prefixes, worker_limit)?;
    let source_wall = source_started.elapsed();
    let export_calls = outcomes
        .iter()
        .filter(|outcome| outcome.export_called)
        .count();
    let issue_prefix_calls = outcomes
        .iter()
        .filter(|outcome| outcome.issue_prefix_called)
        .count();
    let mut errors = Vec::new();
    let mut pairs: Vec<(String, RepoEntry)> = Vec::new();
    let mut verified_repos = Vec::new();
    for outcome in outcomes {
        errors.extend(outcome.errors);
        if let Some(prefix) = outcome.prefix {
            pairs.push((prefix, outcome.entry));
        }
        if let Some(verified) = outcome.verified_prefix {
            verified_repos.push(verified);
        }
    }

    // One sync hydrates the hub from every repo's fresh export. A sync failure
    // is fatal: the hub was not updated, so the whole refresh failed.
    let sync_started = Instant::now();
    let sync_report = bd.repo_sync(&hub).map_err(RefreshError::Sync)?;
    let sync = sync_started.elapsed();

    let prefix_map = Arc::new(PrefixMap::from_pairs(pairs));
    let outcome = RefreshOutcome {
        prefix_map: (*prefix_map).clone(),
        errors,
        synced_at: SystemTime::now(),
        sync_report,
        metrics: RefreshMetrics {
            source_wall,
            sync,
            export_calls,
            issue_prefix_calls,
        },
    };
    Ok(SyncedRefresh {
        outcome,
        candidate: AttributionCandidate {
            roster_paths,
            repos: verified_repos,
            prefix_map,
        },
        _lock: lock,
    })
}

#[derive(Clone)]
struct SourceJob {
    roster_index: usize,
    entry: RepoEntry,
}

struct SourceOutcome {
    roster_index: usize,
    entry: RepoEntry,
    prefix: Option<String>,
    verified_prefix: Option<VerifiedRepoPrefix>,
    export_called: bool,
    issue_prefix_called: bool,
    errors: Vec<RepoError>,
}

fn normalized_jobs(roster: &Config, paths: &Paths) -> Vec<SourceJob> {
    let mut seen = HashSet::new();
    roster
        .repos
        .iter()
        .enumerate()
        .filter_map(|(roster_index, entry)| {
            let resolved_path = paths.resolve_roster_path(&entry.path);
            seen.insert(normalize_path(&resolved_path))
                .then_some(SourceJob {
                    roster_index,
                    entry: RepoEntry {
                        path: resolved_path,
                    },
                })
        })
        .collect()
}

fn run_source_jobs(
    bd: &impl BdClient,
    jobs: Vec<SourceJob>,
    previous_prefixes: &HashMap<PathBuf, String>,
    worker_limit: usize,
) -> Result<Vec<SourceOutcome>, RefreshError> {
    if jobs.is_empty() {
        return Ok(Vec::new());
    }
    let workers = jobs.len().min(worker_limit.max(1));
    let next = Mutex::new(0usize);
    let mut joined = Vec::with_capacity(workers);
    thread::scope(|scope| {
        let mut handles = Vec::with_capacity(workers);
        for _ in 0..workers {
            handles.push(scope.spawn(|| {
                let mut local = Vec::new();
                loop {
                    let job = {
                        let mut next = next.lock().expect("source queue mutex poisoned");
                        if *next == jobs.len() {
                            None
                        } else {
                            let job = jobs[*next].clone();
                            *next += 1;
                            Some(job)
                        }
                    };
                    let Some(job) = job else { break };
                    local.push(run_source_job(bd, job, previous_prefixes));
                }
                local
            }));
        }
        let mut panicked = false;
        for handle in handles {
            match handle.join() {
                Ok(outcomes) => joined.extend(outcomes),
                Err(_) => panicked = true,
            }
        }
        if panicked {
            Err(RefreshError::WorkerPanic)
        } else {
            Ok(())
        }
    })?;
    joined.sort_by_key(|outcome| outcome.roster_index);
    Ok(joined)
}

fn run_source_job(
    bd: &impl BdClient,
    job: SourceJob,
    previous_prefixes: &HashMap<PathBuf, String>,
) -> SourceOutcome {
    let mut errors = stable_export(bd, &job.entry.path);
    let export_called = job.entry.path.exists();
    let normalized_path = normalize_path(&job.entry.path);
    let canonical = job.entry.path.join(".beads/issues.jsonl");
    let fresh_export_available =
        job.entry.path.exists() && errors.is_empty() && canonical.is_file();

    if let Some(cached) = previous_prefixes.get(&normalized_path)
        && fresh_export_available
    {
        match exported_prefix_evidence(&canonical, cached) {
            Ok(ExportPrefixEvidence::NonEmptyAllMatch) => {
                return SourceOutcome {
                    roster_index: job.roster_index,
                    entry: job.entry,
                    prefix: Some(cached.clone()),
                    verified_prefix: Some(VerifiedRepoPrefix {
                        normalized_path,
                        prefix: cached.clone(),
                    }),
                    export_called,
                    issue_prefix_called: false,
                    errors,
                };
            }
            Ok(
                ExportPrefixEvidence::Empty
                | ExportPrefixEvidence::NonEmptyDescendantMatch
                | ExportPrefixEvidence::Mismatch
                | ExportPrefixEvidence::Malformed,
            ) => {}
            Err(error) => errors.push(file_error(
                &job.entry.path,
                "validate exported ids",
                &canonical,
                error,
            )),
        }
    }

    let (prefix, verified_prefix) = match bd.issue_prefix(&job.entry.path) {
        Ok(prefix) => {
            let evidence = canonical
                .is_file()
                .then(|| exported_prefix_evidence(&canonical, &prefix))
                .transpose();
            match evidence {
                Ok(Some(
                    ExportPrefixEvidence::NonEmptyAllMatch
                    | ExportPrefixEvidence::NonEmptyDescendantMatch,
                )) => {
                    let verified_prefix = fresh_export_available.then_some(VerifiedRepoPrefix {
                        normalized_path,
                        prefix: prefix.clone(),
                    });
                    (Some(prefix), verified_prefix)
                }
                Ok(Some(ExportPrefixEvidence::Mismatch)) => {
                    errors.push(RepoError::Metadata {
                        repo: job.entry.path.clone(),
                        detail: format!(
                            "authoritative prefix `{prefix}` does not match freshly exported ids"
                        ),
                    });
                    (None, None)
                }
                Ok(Some(ExportPrefixEvidence::Empty | ExportPrefixEvidence::Malformed))
                | Ok(None) => (Some(prefix), None),
                Err(error) => {
                    errors.push(file_error(
                        &job.entry.path,
                        "validate exported ids",
                        &canonical,
                        error,
                    ));
                    (Some(prefix), None)
                }
            }
        }
        Err(source) => {
            errors.push(RepoError::Metadata {
                repo: job.entry.path.clone(),
                detail: source.to_string(),
            });
            (None, None)
        }
    };
    SourceOutcome {
        roster_index: job.roster_index,
        entry: job.entry,
        prefix,
        verified_prefix,
        export_called,
        issue_prefix_called: true,
        errors,
    }
}

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Export to a unique sibling, compare it with the canonical JSONL, then either
/// discard it or install it with one same-directory rename. This is deliberately
/// the only source-artifact publication path in hank.
fn stable_export(bd: &impl BdClient, repo: &Path) -> Vec<RepoError> {
    if !repo.exists() {
        return Vec::new();
    }
    let canonical = repo.join(".beads/issues.jsonl");
    let parent = match canonical.parent() {
        Some(parent) => parent,
        None => return Vec::new(),
    };
    let temp = match reserve_temp_export(parent, std::process::id(), &TEMP_COUNTER) {
        Ok(temp) => temp,
        Err((candidate, error)) => {
            return vec![file_error(
                repo,
                "reserve temporary export",
                &candidate,
                error,
            )];
        }
    };
    if let Err(source) = bd.export_to(repo, &temp) {
        let mut errors = vec![RepoError::Export {
            repo: repo.to_path_buf(),
            source,
        }];
        cleanup_temp(repo, &temp, &mut errors);
        return errors;
    }
    match files_equal(&temp, &canonical) {
        Ok(true) => {
            let mut errors = Vec::new();
            cleanup_temp(repo, &temp, &mut errors);
            errors
        }
        Ok(false) => {
            if let Err(error) = preserve_supported_metadata(&canonical, &temp) {
                let mut errors = vec![file_error(repo, "preserve export metadata", &temp, error)];
                cleanup_temp(repo, &temp, &mut errors);
                return errors;
            }
            match fs::rename(&temp, &canonical) {
                Ok(()) => Vec::new(),
                Err(error) => {
                    let mut errors = vec![file_error(repo, "publish export", &canonical, error)];
                    cleanup_temp(repo, &temp, &mut errors);
                    errors
                }
            }
        }
        Err(error) => {
            let mut errors = vec![file_error(repo, "compare export", &temp, error)];
            cleanup_temp(repo, &temp, &mut errors);
            errors
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExportPrefixEvidence {
    /// Every id has exactly one `-` delimiter after the candidate prefix.
    NonEmptyAllMatch,
    /// Every id starts with the candidate prefix, but at least one has another
    /// `-` in its suffix, so the candidate may only be an ancestor prefix.
    NonEmptyDescendantMatch,
    Empty,
    Mismatch,
    Malformed,
}

#[derive(Deserialize)]
struct ExportedId {
    id: String,
}

fn exported_prefix_evidence(export: &Path, prefix: &str) -> std::io::Result<ExportPrefixEvidence> {
    let reader = BufReader::new(File::open(export)?);
    let mut saw_id = false;
    let mut saw_descendant = false;
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let exported: ExportedId = match serde_json::from_str(&line) {
            Ok(exported) => exported,
            Err(_) => return Ok(ExportPrefixEvidence::Malformed),
        };
        saw_id = true;
        let Some(suffix) = exported
            .id
            .strip_prefix(prefix)
            .and_then(|rest| rest.strip_prefix('-'))
        else {
            return Ok(ExportPrefixEvidence::Mismatch);
        };
        if suffix.contains('-') {
            saw_descendant = true;
        }
    }
    Ok(if saw_descendant {
        ExportPrefixEvidence::NonEmptyDescendantMatch
    } else if saw_id {
        ExportPrefixEvidence::NonEmptyAllMatch
    } else {
        ExportPrefixEvidence::Empty
    })
}

fn reserve_temp_export(
    parent: &Path,
    process_id: u32,
    counter: &AtomicU64,
) -> Result<PathBuf, (PathBuf, std::io::Error)> {
    loop {
        let candidate = parent.join(format!(
            ".issues.jsonl.hank.{process_id}.{}.tmp",
            counter.fetch_add(1, Ordering::Relaxed)
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(_) => return Ok(candidate),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => return Err((candidate, error)),
        }
    }
}

/// Apply hank's supported metadata contract to a replacement export.
///
/// An unchanged export leaves the canonical inode untouched, preserving all of
/// its metadata. A changed export must use a new inode for atomic rename, so the
/// portable contract preserves `std::fs::Permissions` (Unix mode bits, Windows
/// readonly state). Ownership remains that of the hank-created sibling file;
/// ACLs, xattrs, SELinux labels, file flags, and inode identity are explicitly
/// outside the cross-platform contract. Attempting to preserve the old inode
/// would weaken the crash-safe atomic publication invariant.
fn preserve_supported_metadata(canonical: &Path, temp: &Path) -> std::io::Result<()> {
    match fs::metadata(canonical) {
        Ok(metadata) => fs::set_permissions(temp, metadata.permissions()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn files_equal(a: &Path, b: &Path) -> std::io::Result<bool> {
    let a_meta = fs::metadata(a)?;
    let b_meta = match fs::metadata(b) {
        Ok(meta) => meta,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    if a_meta.len() != b_meta.len() {
        return Ok(false);
    }
    let (mut a, mut b) = (File::open(a)?, File::open(b)?);
    let (mut left, mut right) = ([0u8; 8192], [0u8; 8192]);
    loop {
        let n = a.read(&mut left)?;
        if n != b.read(&mut right)? || left[..n] != right[..n] {
            return Ok(false);
        }
        if n == 0 {
            return Ok(true);
        }
    }
}

fn cleanup_temp(repo: &Path, temp: &Path, errors: &mut Vec<RepoError>) {
    if let Err(error) = fs::remove_file(temp)
        && error.kind() != ErrorKind::NotFound
    {
        errors.push(file_error(repo, "remove temporary export", temp, error));
    }
}

fn file_error(
    repo: &Path,
    operation: &'static str,
    path: &Path,
    error: std::io::Error,
) -> RepoError {
    RepoError::ExportFile {
        repo: repo.to_path_buf(),
        operation,
        path: path.to_path_buf(),
        detail: error.to_string(),
    }
}

/// Build only the id-prefix → repo attribution map from the roster, without
/// exporting, syncing, or taking the hub lock. Reads each repo's authoritative,
/// hyphen-preserving prefix with [`BdClient::issue_prefix`] — the *same* source
/// [`run`] uses — so attribution is identical to a full refresh's.
///
/// This backs cross-repo search (Slice 11), whose worker needs a `PrefixMap` to
/// attribute `bd search` results the same way ready rows are attributed, without
/// re-running the whole export+sync pipeline. Deliberately standalone rather than
/// factored out of [`run`], so `run`'s interleaved export/prefix call order (an
/// asserted Slice 4 contract) is not disturbed. A per-repo prefix-read failure is
/// a non-fatal [`RepoError`]; that repo's ids simply fall to the `unknown` bucket.
#[cfg(test)]
pub(crate) fn attribution_map(bd: &impl BdClient, roster: &Config) -> (PrefixMap, Vec<RepoError>) {
    let mut pairs: Vec<(String, RepoEntry)> = Vec::new();
    let mut errors = Vec::new();
    // Dedupe aliased/duplicate roster entries, mirroring `run`, so one repo
    // listed twice is not mistaken for a self-collision.
    let mut seen: HashSet<PathBuf> = HashSet::new();
    for entry in &roster.repos {
        if !seen.insert(normalize_path(&entry.path)) {
            continue;
        }
        match bd.issue_prefix(&entry.path) {
            Ok(prefix) => pairs.push((prefix, entry.clone())),
            Err(source) => errors.push(RepoError::Metadata {
                repo: entry.path.clone(),
                detail: source.to_string(),
            }),
        }
    }
    (PrefixMap::from_pairs(pairs), errors)
}

/// The subset of `<repo>/.beads/metadata.json` hank reads: the id prefix, stored
/// under `dolt_database`. Tolerant (no `deny_unknown_fields`) — bd writes other
/// keys hank ignores.
#[derive(Debug, Deserialize)]
struct Metadata {
    dolt_database: String,
}

/// Canonicalize `p` if it exists on disk; otherwise return it unchanged. Used to
/// dedupe roster entries that name the same repo via different (aliased) paths.
pub(crate) fn normalize_path(p: &Path) -> PathBuf {
    fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// Read a repo's underscore-sanitized Dolt database name from
/// `<repo>/.beads/metadata.json`'s `dolt_database`.
///
/// This is **not** the attribution prefix: bd sanitizes `-`→`_` for the Dolt DB
/// name while issue ids keep their hyphens, so a hyphenated repo's
/// `dolt_database` (`reading_lite`) does not match its ids (`reading-lite-…`).
/// Attribution instead uses `BdClient::issue_prefix`, which reports bd's
/// authoritative, hyphen-preserving prefix. This helper backs only the
/// [`FakeBdClient`](crate::bd::FakeBdClient)'s default `issue_prefix` (mirroring a
/// real repo whose prefix has no hyphens, where prefix == `dolt_database`), so
/// metadata-seeded test fixtures keep working without programming a prefix.
pub fn read_prefix(repo: &Path) -> Result<String, String> {
    let path = repo.join(".beads").join("metadata.json");
    let text = fs::read_to_string(&path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    let meta: Metadata =
        serde_json::from_str(&text).map_err(|e| format!("parsing {}: {e}", path.display()))?;
    Ok(meta.dolt_database)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bd::{BdErrorKind, Call, FakeBdClient};
    use crate::config::RepoEntry;
    use std::sync::Arc;
    use std::sync::Barrier;
    use std::sync::Condvar;
    use std::sync::atomic::AtomicUsize;

    /// A repo dir under `base` with a seeded `.beads/metadata.json` prefix.
    fn seed_repo(base: &Path, name: &str, prefix: &str) -> PathBuf {
        let repo = base.join(name);
        let beads = repo.join(".beads");
        fs::create_dir_all(&beads).unwrap();
        fs::write(
            beads.join("metadata.json"),
            format!(r#"{{"database":"dolt","dolt_database":"{prefix}"}}"#),
        )
        .unwrap();
        repo
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

    fn bd_err() -> BdError {
        BdError {
            command: "bd ...".into(),
            stderr: "boom".into(),
            kind: BdErrorKind::NonZeroExit { code: Some(1) },
        }
    }

    #[test]
    fn exports_all_then_syncs_once() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let a = seed_repo(tmp.path(), "a", "ra");
        let b = seed_repo(tmp.path(), "b", "rb");
        let fake = FakeBdClient::new();

        run(&fake, &roster(&[&a, &b]), &paths).unwrap();

        let calls = fake.calls();
        assert_eq!(calls.len(), 5);
        assert!(matches!(calls.last(), Some(Call::RepoSync(hub)) if hub == &hub_dir(&paths)));
        for repo in [&a, &b] {
            assert!(calls.iter().any(|call| matches!(call, Call::Export(actual, output) if actual == repo && output.parent() == Some(repo.join(".beads").as_path()))));
            assert!(
                calls
                    .iter()
                    .any(|call| matches!(call, Call::IssuePrefix(actual) if actual == repo))
            );
        }
    }

    #[test]
    fn refresh_resolves_relative_roster_paths_against_config_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let config_dir = paths
            .config_file()
            .parent()
            .expect("config file has a parent");
        let repo = seed_repo(config_dir, "repo", "repo");
        let config = Config {
            repos: vec![RepoEntry {
                path: PathBuf::from("repo"),
            }],
        };
        let fake = FakeBdClient::new();

        run(&fake, &config, &paths).unwrap();

        assert!(
            fake.calls()
                .iter()
                .any(|call| matches!(call, Call::Export(actual, _) if actual == &repo))
        );
    }

    #[test]
    fn source_jobs_overlap() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let a = seed_repo(tmp.path(), "a", "ra");
        let b = seed_repo(tmp.path(), "b", "rb");
        let barrier = Arc::new(Barrier::new(2));
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let fake = FakeBdClient::new().with_call_hook({
            let barrier = Arc::clone(&barrier);
            let active = Arc::clone(&active);
            let maximum = Arc::clone(&maximum);
            move |call| {
                if matches!(call, Call::Export(..)) {
                    let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                    maximum.fetch_max(now, Ordering::SeqCst);
                    barrier.wait();
                    active.fetch_sub(1, Ordering::SeqCst);
                }
            }
        });

        run_with_worker_limit(&fake, &roster(&[&a, &b]), &paths, 2).unwrap();

        assert_eq!(maximum.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn source_jobs_never_exceed_bound_and_sync_waits_for_all_outcomes() {
        #[derive(Default)]
        struct Gate {
            active: usize,
            maximum: usize,
            release: bool,
        }

        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let repos = (0..5)
            .map(|index| seed_repo(tmp.path(), &format!("r{index}"), &format!("r{index}")))
            .collect::<Vec<_>>();
        let config = roster(&repos.iter().map(PathBuf::as_path).collect::<Vec<_>>());
        let gate = Arc::new((Mutex::new(Gate::default()), Condvar::new()));
        let fake = Arc::new(FakeBdClient::new().with_call_hook({
            let gate = Arc::clone(&gate);
            move |call| {
                if matches!(call, Call::Export(..)) {
                    let (mutex, changed) = &*gate;
                    let mut state = mutex.lock().unwrap();
                    state.active += 1;
                    state.maximum = state.maximum.max(state.active);
                    changed.notify_all();
                    while !state.release {
                        state = changed.wait(state).unwrap();
                    }
                    state.active -= 1;
                }
            }
        }));
        let worker_fake = Arc::clone(&fake);
        let handle =
            thread::spawn(move || run_with_worker_limit(worker_fake.as_ref(), &config, &paths, 2));

        let (mutex, changed) = &*gate;
        let mut state = mutex.lock().unwrap();
        while state.active < 2 {
            state = changed.wait(state).unwrap();
        }
        assert_eq!(state.maximum, 2);
        assert!(
            fake.calls()
                .iter()
                .all(|call| !matches!(call, Call::RepoSync(_))),
            "sync cannot start while source outcomes remain blocked"
        );
        state.release = true;
        changed.notify_all();
        drop(state);

        handle.join().unwrap().unwrap();
        assert_eq!(
            fake.calls()
                .iter()
                .filter(|call| matches!(call, Call::RepoSync(_)))
                .count(),
            1
        );
        assert_eq!(mutex.lock().unwrap().maximum, 2);
    }

    #[test]
    fn panicked_worker_joins_every_worker_and_skips_sync() {
        #[derive(Default)]
        struct Gate {
            final_entered: bool,
            release: bool,
        }

        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let a = seed_repo(tmp.path(), "a", "a");
        let b = seed_repo(tmp.path(), "b", "b");
        let c = seed_repo(tmp.path(), "c", "c");
        let gate = Arc::new((Mutex::new(Gate::default()), Condvar::new()));
        let fake = Arc::new(FakeBdClient::new().with_call_hook({
            let b = b.clone();
            let c = c.clone();
            let gate = Arc::clone(&gate);
            move |call| match call {
                Call::Export(repo, _) if repo == &b => panic!("injected source panic"),
                Call::Export(repo, _) if repo == &c => {
                    let (mutex, changed) = &*gate;
                    let mut state = mutex.lock().unwrap();
                    state.final_entered = true;
                    changed.notify_all();
                    while !state.release {
                        state = changed.wait(state).unwrap();
                    }
                }
                _ => {}
            }
        }));
        let worker_fake = Arc::clone(&fake);
        let config = roster(&[&a, &b, &c]);
        let worker_paths = paths.clone();
        let handle = thread::spawn(move || {
            run_with_worker_limit(worker_fake.as_ref(), &config, &worker_paths, 2)
        });

        let (mutex, changed) = &*gate;
        let mut state = mutex.lock().unwrap();
        while !state.final_entered {
            state = changed.wait(state).unwrap();
        }
        state.release = true;
        changed.notify_all();
        drop(state);

        let error = handle.join().unwrap().unwrap_err();
        assert!(matches!(error, RefreshError::WorkerPanic));
        assert!(
            fake.calls()
                .iter()
                .all(|call| !matches!(call, Call::RepoSync(_)))
        );
        assert!(
            HubLock::try_acquire(&hub_dir(&paths)).unwrap().is_some(),
            "the outer refresh returns only after every worker joins and the lock releases"
        );
    }

    #[test]
    fn warnings_and_collisions_follow_roster_order_not_completion_order() {
        #[derive(Default)]
        struct Gate {
            second_started: bool,
        }

        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let first = seed_repo(tmp.path(), "first", "dup");
        let second = seed_repo(tmp.path(), "second", "dup");
        let gate = Arc::new((Mutex::new(Gate::default()), Condvar::new()));
        let fake = FakeBdClient::new()
            .with_export_err(first.clone(), bd_err())
            .with_export_err(second.clone(), bd_err())
            .with_issue_prefix(first.clone(), "dup")
            .with_issue_prefix(second.clone(), "dup")
            .with_call_hook({
                let first = first.clone();
                let second = second.clone();
                let gate = Arc::clone(&gate);
                move |call| match call {
                    Call::Export(repo, _) if repo == &first => {
                        let (mutex, changed) = &*gate;
                        let mut state = mutex.lock().unwrap();
                        while !state.second_started {
                            state = changed.wait(state).unwrap();
                        }
                    }
                    Call::Export(repo, _) if repo == &second => {
                        let (mutex, changed) = &*gate;
                        mutex.lock().unwrap().second_started = true;
                        changed.notify_all();
                    }
                    _ => {}
                }
            });

        let outcome = run_with_worker_limit(&fake, &roster(&[&first, &second]), &paths, 2).unwrap();

        let warning_repos = outcome
            .errors
            .iter()
            .filter_map(|error| match error {
                RepoError::Export { repo, .. } => Some(repo),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(warning_repos, vec![&first, &second]);
        assert_eq!(
            outcome.prefix_map.collisions()[0].repos,
            vec![first, second]
        );
    }

    #[test]
    fn unchanged_export_preserves_canonical_bytes_and_mtime() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = seed_repo(tmp.path(), "a", "ra");
        let canonical = repo.join(".beads/issues.jsonl");
        fs::write(&canonical, b"same\n").unwrap();
        let original = fs::metadata(&canonical).unwrap().modified().unwrap();
        let fake = FakeBdClient::new().with_export_content(&repo, b"same\n".to_vec());

        assert!(stable_export(&fake, &repo).is_empty());
        assert_eq!(fs::read(&canonical).unwrap(), b"same\n");
        assert_eq!(
            fs::metadata(&canonical).unwrap().modified().unwrap(),
            original
        );
        assert_eq!(
            fs::read_dir(canonical.parent().unwrap()).unwrap().count(),
            2,
            "only metadata and canonical export remain"
        );
    }

    #[test]
    fn temporary_export_reservation_retries_stale_name_collisions() {
        let tmp = tempfile::tempdir().unwrap();
        let counter = AtomicU64::new(0);
        let stale = tmp.path().join(".issues.jsonl.hank.7.0.tmp");
        fs::write(&stale, b"stale").unwrap();

        let reserved = reserve_temp_export(tmp.path(), 7, &counter).unwrap();

        assert_eq!(
            reserved,
            tmp.path().join(".issues.jsonl.hank.7.1.tmp"),
            "the next unused counter value is reserved"
        );
        assert_eq!(
            fs::read(&stale).unwrap(),
            b"stale",
            "a colliding stale file is never consumed or overwritten"
        );
        assert!(reserved.exists());
    }

    #[test]
    fn changed_export_publishes_new_canonical_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = seed_repo(tmp.path(), "a", "ra");
        let canonical = repo.join(".beads/issues.jsonl");
        fs::write(&canonical, b"old\n").unwrap();
        let fake = FakeBdClient::new().with_export_content(&repo, b"new\n".to_vec());

        assert!(stable_export(&fake, &repo).is_empty());
        assert_eq!(fs::read(&canonical).unwrap(), b"new\n");
        assert!(
            fs::read_dir(canonical.parent().unwrap())
                .unwrap()
                .all(|entry| {
                    !entry
                        .unwrap()
                        .file_name()
                        .to_string_lossy()
                        .contains(".hank.")
                })
        );
    }

    #[cfg(unix)]
    #[test]
    fn changed_export_preserves_canonical_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let repo = seed_repo(tmp.path(), "a", "ra");
        let canonical = repo.join(".beads/issues.jsonl");
        fs::write(&canonical, b"old\n").unwrap();
        fs::set_permissions(&canonical, fs::Permissions::from_mode(0o600)).unwrap();
        let fake = FakeBdClient::new().with_export_content(&repo, b"new\n".to_vec());

        assert!(stable_export(&fake, &repo).is_empty());

        assert_eq!(
            fs::metadata(&canonical).unwrap().permissions().mode() & 0o777,
            0o600,
            "publishing changed bytes must not widen access to the canonical export"
        );
    }

    #[test]
    fn changed_export_preserves_cross_platform_readonly_permission() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = seed_repo(tmp.path(), "a", "ra");
        let canonical = repo.join(".beads/issues.jsonl");
        fs::write(&canonical, b"old\n").unwrap();
        let mut permissions = fs::metadata(&canonical).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&canonical, permissions).unwrap();
        let fake = FakeBdClient::new().with_export_content(&repo, b"new\n".to_vec());

        assert!(stable_export(&fake, &repo).is_empty());
        assert!(fs::metadata(&canonical).unwrap().permissions().readonly());
    }

    #[test]
    fn failed_export_leaves_canonical_untouched_and_removes_temporary_file() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = seed_repo(tmp.path(), "a", "ra");
        let canonical = repo.join(".beads/issues.jsonl");
        fs::write(&canonical, b"old\n").unwrap();
        let fake = FakeBdClient::new().with_export_err(repo.clone(), bd_err());

        let errors = stable_export(&fake, &repo);
        assert!(matches!(errors.as_slice(), [RepoError::Export { .. }]));
        assert_eq!(fs::read(&canonical).unwrap(), b"old\n");
        assert!(
            fs::read_dir(canonical.parent().unwrap())
                .unwrap()
                .all(|entry| {
                    !entry
                        .unwrap()
                        .file_name()
                        .to_string_lossy()
                        .contains(".hank.")
                })
        );
    }

    #[test]
    fn collects_per_repo_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let a = seed_repo(tmp.path(), "a", "ra");
        let b = seed_repo(tmp.path(), "b", "rb");
        let fake = FakeBdClient::new().with_export_err(b.clone(), bd_err());

        let outcome = run(&fake, &roster(&[&a, &b]), &paths).unwrap();

        assert!(
            outcome
                .errors
                .iter()
                .any(|e| matches!(e, RepoError::Export { repo, .. } if repo == &b)),
            "b's export failure is recorded: {:?}",
            outcome.errors
        );
        assert!(
            fake.calls().iter().any(|c| matches!(c, Call::RepoSync(_))),
            "sync still runs despite a per-repo export failure"
        );
        assert!(
            outcome.prefix_map.repo_for("ra-2hc").is_some(),
            "the healthy repo still hydrates and is attributed"
        );
    }

    #[test]
    fn reads_prefix_from_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let a = seed_repo(tmp.path(), "a", "ra");
        let fake = FakeBdClient::new();

        let outcome = run(&fake, &roster(&[&a]), &paths).unwrap();

        assert_eq!(
            outcome.prefix_map.repo_for("ra-2hc").map(|r| &r.path),
            Some(&a),
            "prefix comes from metadata.json dolt_database"
        );
    }

    #[test]
    fn builds_prefix_map() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let a = seed_repo(tmp.path(), "a", "ra");
        let b = seed_repo(tmp.path(), "b", "rb");
        let fake = FakeBdClient::new();

        let outcome = run(&fake, &roster(&[&a, &b]), &paths).unwrap();
        let map = outcome.prefix_map;

        assert_eq!(map.repo_for("ra-2hc").map(|r| &r.path), Some(&a));
        assert_eq!(map.repo_for("rb-9zz").map(|r| &r.path), Some(&b));
        assert!(map.repo_for("zz-1").is_none(), "unknown prefix -> None");
    }

    #[test]
    fn flags_prefix_collisions() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let a = seed_repo(tmp.path(), "a", "dup");
        let b = seed_repo(tmp.path(), "b", "dup");
        let fake = FakeBdClient::new();

        let outcome = run(&fake, &roster(&[&a, &b]), &paths).unwrap();

        let collisions = outcome.prefix_map.collisions();
        assert_eq!(collisions.len(), 1, "one collided prefix");
        assert_eq!(collisions[0].prefix, "dup");
        assert!(collisions[0].repos.contains(&a) && collisions[0].repos.contains(&b));
        assert!(
            outcome.prefix_map.repo_for("dup-x").is_none(),
            "a collided prefix is ambiguous -> None"
        );
    }

    #[test]
    fn longest_prefix_wins() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let a = seed_repo(tmp.path(), "a", "app");
        let b = seed_repo(tmp.path(), "b", "app2");
        let fake = FakeBdClient::new();

        let outcome = run(&fake, &roster(&[&a, &b]), &paths).unwrap();
        let map = outcome.prefix_map;

        assert_eq!(
            map.repo_for("app2-xyz").map(|r| &r.path),
            Some(&b),
            "app2-xyz must attribute to app2, never app"
        );
        assert_eq!(map.repo_for("app-xyz").map(|r| &r.path), Some(&a));
    }

    #[test]
    fn metadata_read_failure_is_a_repo_error() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        // `bad` exists (so export is attempted) but has no metadata.json.
        let bad = tmp.path().join("bad");
        fs::create_dir_all(&bad).unwrap();
        let good = seed_repo(tmp.path(), "good", "rg");
        let fake = FakeBdClient::new();

        let outcome = run(&fake, &roster(&[&bad, &good]), &paths).unwrap();

        assert!(
            outcome
                .errors
                .iter()
                .any(|e| matches!(e, RepoError::Metadata { repo, .. } if repo == &bad)),
            "unreadable metadata -> RepoError::Metadata: {:?}",
            outcome.errors
        );
        assert!(
            outcome.prefix_map.repo_for("rg-1").is_some(),
            "the readable repo is still attributed"
        );
    }

    #[test]
    fn sync_failure_is_fatal() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let a = seed_repo(tmp.path(), "a", "ra");
        let fake = FakeBdClient::new().with_repo_sync_err(bd_err());

        let err = run(&fake, &roster(&[&a]), &paths).unwrap_err();

        assert!(matches!(err, RefreshError::Sync(_)), "got {err:?}");
    }

    #[test]
    fn collided_longer_prefix_is_not_masked_by_shorter() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        // A unique short prefix `app`, plus two repos both claiming `app-foo`.
        let a = seed_repo(tmp.path(), "a", "app");
        let b = seed_repo(tmp.path(), "b", "app-foo");
        let c = seed_repo(tmp.path(), "c", "app-foo");
        let fake = FakeBdClient::new();

        let outcome = run(&fake, &roster(&[&a, &b, &c]), &paths).unwrap();
        let map = outcome.prefix_map;

        // The longest match for `app-foo-123` is the collided `app-foo`, so it is
        // ambiguous — the shorter unique `app` must not claim it.
        assert!(
            map.repo_for("app-foo-123").is_none(),
            "a collided longer prefix must not fall through to a shorter one"
        );
        // The unique `app` still resolves ids that only it matches.
        assert_eq!(map.repo_for("app-xyz").map(|r| &r.path), Some(&a));
    }

    #[test]
    fn duplicate_roster_entry_is_not_a_collision() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let a = seed_repo(tmp.path(), "a", "ra");
        let fake = FakeBdClient::new();

        // The same repo listed twice must dedupe, not self-collide.
        let outcome = run(&fake, &roster(&[&a, &a]), &paths).unwrap();

        assert!(
            outcome.prefix_map.collisions().is_empty(),
            "an aliased duplicate is not a collision: {:?}",
            outcome.prefix_map.collisions()
        );
        assert_eq!(
            outcome.prefix_map.repo_for("ra-1").map(|r| &r.path),
            Some(&a),
            "the deduped repo still attributes its ids"
        );
        let exports = fake
            .calls()
            .into_iter()
            .filter(|c| matches!(c, Call::Export(..)))
            .count();
        assert_eq!(exports, 1, "a duplicate roster entry exports once");
    }

    #[test]
    fn hyphen_extended_authoritative_prefix_does_not_collide_with_shorter_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let extended = seed_repo(tmp.path(), "extended", "foo_bar");
        let base = seed_repo(tmp.path(), "base", "foo");
        let fake = FakeBdClient::new()
            .with_issue_prefix(extended.clone(), "foo-bar")
            .with_issue_prefix(base.clone(), "foo")
            .with_export_content(&extended, b"{\"id\":\"foo-bar-1\"}\n".to_vec())
            .with_export_content(&base, b"{\"id\":\"foo-1\"}\n".to_vec());
        let outcome =
            run_with_worker_limit(&fake, &roster(&[&extended, &base]), &paths, 1).unwrap();

        assert_eq!(
            outcome
                .prefix_map
                .repo_for("foo-bar-1")
                .map(|entry| &entry.path),
            Some(&extended),
            "the live foo-bar prefix must not collide with a different repo's foo prefix"
        );
        assert!(
            fake.calls()
                .iter()
                .any(|call| matches!(call, Call::IssuePrefix(repo) if repo == &extended)),
            "the authoritative prefix is read for every source repo"
        );
    }

    #[test]
    fn unchanged_nonempty_export_reuses_verified_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let repo = seed_repo(tmp.path(), "repo", "reading_lite");
        let fake = FakeBdClient::new()
            .with_issue_prefix(repo.clone(), "reading-lite")
            .with_export_content(&repo, b"{\"id\":\"reading-lite-1\"}\n".to_vec());

        let first =
            run_with_state(&fake, &roster(&[&repo]), &paths, None, 1).expect("first refresh");
        let verified = first.candidate().clone();
        let _ = first.into_outcome();

        let second = run_with_state(&fake, &roster(&[&repo]), &paths, Some(&verified), 1)
            .expect("warm refresh");
        let _ = second.into_outcome();

        assert_eq!(
            fake.calls()
                .iter()
                .filter(|call| matches!(call, Call::IssuePrefix(path) if path == &repo))
                .count(),
            1,
            "the second valid export proves the retained prefix without reopening the source database"
        );
    }

    #[test]
    fn renamed_exported_ids_reread_only_changed_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let renamed = seed_repo(tmp.path(), "renamed", "old");
        let stable = seed_repo(tmp.path(), "stable", "stable");
        let first_bd = FakeBdClient::new()
            .with_issue_prefix(renamed.clone(), "old")
            .with_issue_prefix(stable.clone(), "stable")
            .with_export_content(&renamed, b"{\"id\":\"old-1\"}\n".to_vec())
            .with_export_content(&stable, b"{\"id\":\"stable-1\"}\n".to_vec());
        let first =
            run_with_state(&first_bd, &roster(&[&renamed, &stable]), &paths, None, 1).unwrap();
        let verified = first.candidate().clone();
        let _ = first.into_outcome();

        let second_bd = FakeBdClient::new()
            .with_issue_prefix(renamed.clone(), "new")
            .with_issue_prefix(stable.clone(), "stable")
            .with_export_content(&renamed, b"{\"id\":\"new-1\"}\n".to_vec())
            .with_export_content(&stable, b"{\"id\":\"stable-1\"}\n".to_vec());
        let second = run_with_state(
            &second_bd,
            &roster(&[&renamed, &stable]),
            &paths,
            Some(&verified),
            1,
        )
        .unwrap();

        assert_eq!(
            second
                .outcome()
                .prefix_map
                .repo_for("new-1")
                .map(|entry| &entry.path),
            Some(&renamed)
        );
        assert_eq!(
            second_bd
                .calls()
                .iter()
                .filter_map(|call| match call {
                    Call::IssuePrefix(path) => Some(path),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            vec![&renamed],
            "the changed export invalidates only its own cached prefix"
        );
    }

    #[test]
    fn descendant_prefix_rename_rereads_metadata_and_reports_collision() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let renamed = seed_repo(tmp.path(), "renamed", "foo");
        let existing = seed_repo(tmp.path(), "existing", "foo-bar");
        let first_bd = FakeBdClient::new()
            .with_issue_prefix(renamed.clone(), "foo")
            .with_issue_prefix(existing.clone(), "foo-bar")
            .with_export_content(&renamed, b"{\"id\":\"foo-1\"}\n".to_vec())
            .with_export_content(&existing, b"{\"id\":\"foo-bar-2\"}\n".to_vec());
        let first =
            run_with_state(&first_bd, &roster(&[&renamed, &existing]), &paths, None, 1).unwrap();
        let verified = first.candidate().clone();
        let _ = first.into_outcome();

        let second_bd = FakeBdClient::new()
            .with_issue_prefix(renamed.clone(), "foo-bar")
            .with_issue_prefix(existing.clone(), "foo-bar")
            .with_export_content(&renamed, b"{\"id\":\"foo-bar-1\"}\n".to_vec())
            .with_export_content(&existing, b"{\"id\":\"foo-bar-2\"}\n".to_vec());
        let second = run_with_state(
            &second_bd,
            &roster(&[&renamed, &existing]),
            &paths,
            Some(&verified),
            1,
        )
        .unwrap();

        assert!(
            second.outcome().prefix_map.repo_for("foo-bar-1").is_none(),
            "the renamed repo must collide with the existing foo-bar owner"
        );
        assert!(
            second_bd
                .calls()
                .iter()
                .any(|call| matches!(call, Call::IssuePrefix(path) if path == &renamed)),
            "foo-bar-1 cannot prove that cached ancestor prefix foo is still exact"
        );
    }

    #[test]
    fn empty_export_rereads_prefix_every_refresh() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let repo = seed_repo(tmp.path(), "empty", "empty");
        let fake = FakeBdClient::new()
            .with_issue_prefix(repo.clone(), "empty")
            .with_export_content(&repo, Vec::new());

        let first = run_with_state(&fake, &roster(&[&repo]), &paths, None, 1).unwrap();
        let candidate = first.candidate().clone();
        assert!(candidate.repos.is_empty(), "empty output proves no prefix");
        let _ = first.into_outcome();
        let second = run_with_state(&fake, &roster(&[&repo]), &paths, Some(&candidate), 1).unwrap();
        let _ = second.into_outcome();

        assert_eq!(
            fake.calls()
                .iter()
                .filter(|call| matches!(call, Call::IssuePrefix(path) if path == &repo))
                .count(),
            2
        );
    }

    #[test]
    fn failed_export_rereads_prefix_and_does_not_retain_it() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let repo = seed_repo(tmp.path(), "repo", "ra");
        let healthy = FakeBdClient::new()
            .with_issue_prefix(repo.clone(), "ra")
            .with_export_content(&repo, b"{\"id\":\"ra-1\"}\n".to_vec());
        let first = run_with_state(&healthy, &roster(&[&repo]), &paths, None, 1).unwrap();
        let previous = first.candidate().clone();
        let _ = first.into_outcome();

        let failing = FakeBdClient::new()
            .with_issue_prefix(repo.clone(), "new")
            .with_export_err(repo.clone(), bd_err());
        let second =
            run_with_state(&failing, &roster(&[&repo]), &paths, Some(&previous), 1).unwrap();

        assert!(second.candidate().repos.is_empty());
        assert!(
            second.outcome().prefix_map.repo_for("ra-1").is_none(),
            "a changed authoritative prefix cannot attribute stale canonical ids after export failure"
        );
        assert_eq!(
            failing
                .calls()
                .iter()
                .filter(|call| matches!(call, Call::IssuePrefix(path) if path == &repo))
                .count(),
            1
        );
    }

    #[test]
    fn malformed_export_rereads_prefix_and_does_not_retain_it() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let repo = seed_repo(tmp.path(), "repo", "ra");
        let fake = FakeBdClient::new()
            .with_issue_prefix(repo.clone(), "ra")
            .with_export_content(&repo, b"not-json\n".to_vec());

        let refresh = run_with_state(&fake, &roster(&[&repo]), &paths, None, 1).unwrap();

        assert!(refresh.candidate().repos.is_empty());
        assert!(refresh.outcome().prefix_map.repo_for("ra-1").is_some());
        assert_eq!(
            fake.calls()
                .iter()
                .filter(|call| matches!(call, Call::IssuePrefix(path) if path == &repo))
                .count(),
            1
        );
    }

    #[test]
    fn authoritative_prefix_mismatching_export_is_unattributed() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let repo = seed_repo(tmp.path(), "repo", "wrong");
        let fake = FakeBdClient::new()
            .with_issue_prefix(repo.clone(), "authoritative")
            .with_export_content(&repo, b"{\"id\":\"different-1\"}\n".to_vec());

        let outcome = run(&fake, &roster(&[&repo]), &paths).unwrap();

        assert!(outcome.prefix_map.repo_for("different-1").is_none());
        assert!(outcome.errors.iter().any(
            |error| matches!(error, RepoError::Metadata { repo: actual, detail } if actual == &repo && detail.contains("does not match"))
        ));
    }

    #[test]
    fn declines_when_lock_already_held() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let a = seed_repo(tmp.path(), "a", "ra");
        let hub = hub_dir(&paths);
        fs::create_dir_all(&hub).unwrap();

        // Hold the lock, then a refresh must decline without doing any work.
        let held = HubLock::try_acquire(&hub).unwrap();
        assert!(held.is_some(), "precondition: acquired the lock");
        let fake = FakeBdClient::new();

        let err = run(&fake, &roster(&[&a]), &paths).unwrap_err();

        assert!(
            matches!(err, RefreshError::AlreadyRefreshing),
            "got {err:?}"
        );
        assert!(
            fake.calls().is_empty(),
            "a declined refresh performs no exports or sync: {:?}",
            fake.calls()
        );
    }

    #[test]
    fn attributes_hyphenated_repo_from_bd_prefix() {
        // The bug: metadata.json's dolt_database is underscore-sanitized
        // (`reading_lite`) while ids keep the hyphen (`reading-lite-…`), so the
        // repo landed in the unknown bucket. Attribution now uses bd's
        // authoritative, hyphen-preserving prefix instead.
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        // dolt_database is the sanitized `reading_lite`; bd reports `reading-lite`.
        let repo = seed_repo(tmp.path(), "reading-lite", "reading_lite");
        let fake = FakeBdClient::new().with_issue_prefix(repo.clone(), "reading-lite");

        let outcome = run(&fake, &roster(&[&repo]), &paths).unwrap();
        let map = outcome.prefix_map;

        assert_eq!(
            map.repo_for("reading-lite-hck.1").map(|r| &r.path),
            Some(&repo),
            "a hyphenated id attributes to its repo, not the unknown bucket"
        );
        assert!(
            map.repo_for("reading_lite-hck.1").is_none(),
            "the underscored (dolt_database) form must not attribute"
        );
    }

    #[test]
    fn underscore_prefix_is_not_remapped() {
        // A repo whose prefix genuinely contains an underscore keeps it: bd
        // reports the exact prefix, so there is no `_`→`-` guessing either way.
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let repo = seed_repo(tmp.path(), "r", "foo_bar");
        let fake = FakeBdClient::new().with_issue_prefix(repo.clone(), "foo_bar");

        let outcome = run(&fake, &roster(&[&repo]), &paths).unwrap();
        let map = outcome.prefix_map;

        assert_eq!(map.repo_for("foo_bar-abc").map(|r| &r.path), Some(&repo));
        assert!(
            map.repo_for("foo-bar-abc").is_none(),
            "the hyphen form must not attribute to an underscore-prefixed repo"
        );
    }

    #[test]
    fn custom_prefix_unrelated_to_dir_name_attributes() {
        // Attribution is prefix-driven, never dir-name-driven: a repo dir named
        // `whatever` whose prefix is `ready-fix` still attributes correctly.
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let repo = seed_repo(tmp.path(), "whatever", "ready_fix");
        let fake = FakeBdClient::new().with_issue_prefix(repo.clone(), "ready-fix");

        let outcome = run(&fake, &roster(&[&repo]), &paths).unwrap();

        assert_eq!(
            outcome.prefix_map.repo_for("ready-fix-1").map(|r| &r.path),
            Some(&repo)
        );
    }

    #[test]
    fn two_hyphenated_repos_attribute_independently() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let a = seed_repo(tmp.path(), "reading-lite", "reading_lite");
        let b = seed_repo(tmp.path(), "session-tui", "session_tui");
        let fake = FakeBdClient::new()
            .with_issue_prefix(a.clone(), "reading-lite")
            .with_issue_prefix(b.clone(), "session-tui");

        let outcome = run(&fake, &roster(&[&a, &b]), &paths).unwrap();
        let map = outcome.prefix_map;

        assert!(
            map.collisions().is_empty(),
            "distinct prefixes don't collide"
        );
        assert_eq!(map.repo_for("reading-lite-1").map(|r| &r.path), Some(&a));
        assert_eq!(map.repo_for("session-tui-9").map(|r| &r.path), Some(&b));
    }

    #[test]
    fn read_prefix_returns_sanitized_dolt_database() {
        // The metadata helper (the fake's default prefix source) returns the
        // underscore-sanitized DB name verbatim.
        let tmp = tempfile::tempdir().unwrap();
        let repo = seed_repo(tmp.path(), "a", "reading_lite");

        assert_eq!(read_prefix(&repo).unwrap(), "reading_lite");
    }

    #[test]
    fn attribution_map_reads_prefixes() {
        // The search path's map builder: attributes each repo's ids, and a
        // metadata-read failure is a non-fatal RepoError (the other repo stands).
        let tmp = tempfile::tempdir().unwrap();
        let a = seed_repo(tmp.path(), "a", "ra");
        // `bad` has no metadata.json, so its prefix read fails.
        let bad = tmp.path().join("bad");
        fs::create_dir_all(&bad).unwrap();
        let fake = FakeBdClient::new();

        let (map, errors) = attribution_map(&fake, &roster(&[&a, &bad]));

        assert_eq!(
            map.repo_for("ra-2hc").map(|r| &r.path),
            Some(&a),
            "the readable repo is attributed"
        );
        assert!(
            errors
                .iter()
                .any(|e| matches!(e, RepoError::Metadata { repo, .. } if repo == &bad)),
            "an unreadable prefix is a non-fatal RepoError: {errors:?}"
        );
    }

    #[test]
    fn lock_releases_after_refresh() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let a = seed_repo(tmp.path(), "a", "ra");
        let fake = FakeBdClient::new();

        run(&fake, &roster(&[&a]), &paths).unwrap();

        // The refresh released the lock, so it can be re-acquired now.
        let reacquired = HubLock::try_acquire(&hub_dir(&paths)).unwrap();
        assert!(reacquired.is_some(), "lock must be released after refresh");
    }

    #[test]
    fn hub_generation_marker_round_trips_atomically() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        fs::create_dir_all(hub_dir(&paths)).unwrap();
        let token = HubGenerationToken::new("test-generation");

        publish_hub_generation(&paths, &token).unwrap();

        assert_eq!(read_hub_generation(&paths).unwrap(), Some(token));
        assert!(fs::read_dir(hub_dir(&paths)).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".tmp")
        }));
    }

    #[test]
    fn hub_generation_temp_collision_is_preserved_and_retried() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let hub = hub_dir(&paths);
        fs::create_dir_all(&hub).unwrap();
        let counter = AtomicU64::new(0);
        let collision = hub.join(format!(".hank-generation.{}.0.tmp", std::process::id()));
        fs::write(&collision, "not ours").unwrap();
        let token = HubGenerationToken::new("next-generation");

        publish_hub_generation_with_counter(&paths, &token, &counter).unwrap();

        assert_eq!(read_hub_generation(&paths).unwrap(), Some(token));
        assert_eq!(fs::read_to_string(collision).unwrap(), "not ours");
        assert!(
            !hub.join(format!(".hank-generation.{}.1.tmp", std::process::id()))
                .exists()
        );
    }
}
