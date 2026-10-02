//! Opt-in live refresh from bd's events journal (`hank --watch`, or
//! `watch = true` in `config.toml`).
//!
//! One follower thread per roster repo runs `bd -C <repo> events tail --since
//! <seq> --follow --json` and reports every record it reads. A batcher thread
//! coalesces those reports for [`DEBOUNCE`] and sends the UI one
//! [`Msg::WatchChanged`] naming only the repos that changed, so the runtime
//! re-exports just those repos before the single hub sync. Hank never applies
//! journal records itself: a record only says "this repo changed", and the
//! refresh re-reads current state through `bd` like any other refresh.
//!
//! The highest `seq` (and its `ts`) seen per repo is saved to
//! `<data_dir>/events_checkpoints.json`. On the next launch each follower first
//! re-reads its saved record to prove the checkpoint still names the same
//! journal (each clone has its own sequence space, and a checkpoint above a new
//! clone's head would read as "caught up" forever), then resumes after it.
//!
//! A checkpoint below the retained window (`events_journal_truncated`) falls
//! back to a full refresh and resumes from the reported head. A repo whose
//! journal is off gets it turned on (`bd config set events-journal true`, which
//! edits the repo's git-tracked `.beads/config.yaml`), announced once in the
//! status bar, unless the roster marks it `unwatched` (`hank repos unwatch`).
//! An opted-out repo, or one whose enable failed, is reported once and
//! re-checked on the backoff, so a journal turned on later is followed without
//! a restart. Syncs (`bd dolt pull`) are not journaled, so `r` (and every
//! launch) remains a full refresh.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::app::{Msg, RefreshScope};
use crate::bd::{BdCli, BdClient};
use crate::cli::sanitize;

/// How long the batcher keeps collecting after the first report of a burst, so
/// a multi-record write (a close that unblocks three beads) yields one refresh.
pub const DEBOUNCE: Duration = Duration::from_millis(300);

/// Delay before a follower retries after `bd` failed or its stream ended
/// unexpectedly. Doubles per consecutive failure up to [`MAX_BACKOFF`].
const INITIAL_BACKOFF: Duration = Duration::from_secs(2);
const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// How often a sleeping follower re-checks the stop flag.
const STOP_POLL: Duration = Duration::from_millis(100);

/// The note `bd events tail` prints on stderr when the workspace's journal is
/// off. Matched loosely so wording tweaks around it do not break detection.
const DISABLED_NOTE: &str = "events journal is disabled";

/// The error code `bd events tail` reports for a checkpoint below the retained
/// window, before the stream opens (pretty JSON) or mid-`--follow` (one line).
const TRUNCATED_CODE: &str = "events_journal_truncated";

/// The last journal record Hank processed for one repo.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkpoint {
    pub seq: u64,
    /// The record's commit timestamp, kept to prove on resume that `seq` still
    /// names the same record (and so the same clone's journal).
    #[serde(default)]
    pub ts: String,
}

/// Saved checkpoints, keyed by the repo's normalized path.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkpoints {
    #[serde(default)]
    pub repos: BTreeMap<PathBuf, Checkpoint>,
}

impl Checkpoints {
    /// Load saved checkpoints. A missing or unreadable file is an empty set:
    /// every follower then starts from the beginning of its retained journal.
    pub fn load(path: &Path) -> Checkpoints {
        fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    /// Atomically replace the checkpoint file (same-directory temp + rename).
    pub fn save(&self, path: &Path) -> Result<()> {
        let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
        if let Some(parent) = parent {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating data directory {}", parent.display()))?;
        }
        let bytes = serde_json::to_vec_pretty(self).context("serializing checkpoints")?;
        let file_name = path
            .file_name()
            .context("checkpoint path has no file name")?
            .to_string_lossy();
        let temp_name = format!(".{file_name}.tmp.{}", std::process::id());
        let temp_path = match parent {
            Some(parent) => parent.join(temp_name),
            None => PathBuf::from(temp_name),
        };
        fs::write(&temp_path, bytes)
            .with_context(|| format!("writing temporary checkpoints {}", temp_path.display()))?;
        fs::rename(&temp_path, path)
            .with_context(|| format!("replacing checkpoints {}", path.display()))?;
        Ok(())
    }
}

/// One line of `bd events tail --json` output, as far as Hank cares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JournalLine {
    /// A journal record: the repo changed.
    Record(Checkpoint),
    /// The checkpoint fell below the retained window; resume from `head`.
    Truncated { head: u64 },
    /// Anything else (blank lines, fragments of a pretty-printed error).
    Other,
}

#[derive(Deserialize)]
struct RawRecord {
    seq: u64,
    #[serde(default)]
    ts: String,
}

#[derive(Deserialize)]
struct RawFailure {
    code: String,
    #[serde(default)]
    head: u64,
}

/// Classify one JSON document from `bd events tail --json`. Records carry
/// `seq`; the truncation failure carries `code` (and `since`, never `seq`).
pub fn parse_line(text: &str) -> JournalLine {
    let text = text.trim();
    if text.is_empty() {
        return JournalLine::Other;
    }
    if let Ok(record) = serde_json::from_str::<RawRecord>(text) {
        return JournalLine::Record(Checkpoint {
            seq: record.seq,
            ts: record.ts,
        });
    }
    match serde_json::from_str::<RawFailure>(text) {
        Ok(failure) if failure.code == TRUNCATED_CODE => {
            JournalLine::Truncated { head: failure.head }
        }
        _ => JournalLine::Other,
    }
}

/// Captured output of one bounded `bd events tail` call.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProbeOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Where a follower starts, decided by re-reading its saved record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartPoint {
    /// Follow from this seq (the verified checkpoint, or 0 for a new repo).
    Resume(u64),
    /// The saved record is gone or names a different record: the checkpoint is
    /// from another clone (or a reset journal). Follow from the beginning; the
    /// replayed records trigger one refresh of this repo.
    Restart,
    /// The checkpoint was pruned away: fall back to a full refresh, then follow
    /// from the reported head.
    Rebaseline { head: u64 },
    /// The journal is off for this workspace; leave the repo unwatched.
    Disabled,
    /// `bd` failed (too old to know `events`, missing workspace, ...).
    Failed(String),
}

/// Decide a follower's start point from `bd events tail --since <seq-1>
/// --limit 1 --json` (or `--since 0` with no checkpoint).
pub fn classify_probe(saved: Option<&Checkpoint>, probe: &ProbeOutput) -> StartPoint {
    if probe.stderr.contains(DISABLED_NOTE) {
        return StartPoint::Disabled;
    }
    // The truncation failure is pretty-printed JSON on stdout with exit 1.
    if let JournalLine::Truncated { head } = parse_line(&probe.stdout) {
        return StartPoint::Rebaseline { head };
    }
    if !probe.success {
        let detail = if probe.stderr.trim().is_empty() {
            probe.stdout.trim()
        } else {
            probe.stderr.trim()
        };
        return StartPoint::Failed(detail.to_string());
    }
    let Some(saved) = saved else {
        return StartPoint::Resume(0);
    };
    let first = probe
        .stdout
        .lines()
        .map(parse_line)
        .find(|line| matches!(line, JournalLine::Record(_)));
    match first {
        Some(JournalLine::Record(record)) if &record == saved => StartPoint::Resume(saved.seq),
        _ => StartPoint::Restart,
    }
}

/// What a follower reports to the batcher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Report {
    /// A record was read for `repo`.
    Changed {
        repo: PathBuf,
        checkpoint: Checkpoint,
    },
    /// `repo`'s checkpoint was pruned, so its journal can't say what changed
    /// since: refresh everything.
    Rebaseline { repo: PathBuf },
    /// A user-facing problem (journal off, `bd` failure), shown once.
    Warning(String),
}

/// Reports coalesced over one debounce window.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Batch {
    pub full: bool,
    pub repos: BTreeSet<PathBuf>,
    pub checkpoints: BTreeMap<PathBuf, Checkpoint>,
    /// Repos whose saved checkpoint was pruned away and must be forgotten.
    pub cleared: BTreeSet<PathBuf>,
    pub warnings: Vec<String>,
}

impl Batch {
    pub fn add(&mut self, report: Report) {
        match report {
            Report::Changed { repo, checkpoint } => {
                let newer = self
                    .checkpoints
                    .get(&repo)
                    .is_none_or(|current| checkpoint.seq > current.seq);
                if newer {
                    self.cleared.remove(&repo);
                    self.checkpoints.insert(repo.clone(), checkpoint);
                }
                self.repos.insert(repo);
            }
            Report::Rebaseline { repo } => {
                self.full = true;
                // The follower resumes at the reported head, which has no
                // record whose timestamp could verify it on the next launch, so
                // forget the pruned checkpoint instead of saving a new one.
                self.checkpoints.remove(&repo);
                self.cleared.insert(repo);
            }
            Report::Warning(warning) => {
                if !self.warnings.contains(&warning) {
                    self.warnings.push(warning);
                }
            }
        }
    }

    /// The refresh this batch asks for, if any.
    pub fn scope(&self) -> Option<RefreshScope> {
        if self.full {
            Some(RefreshScope::Full)
        } else if self.repos.is_empty() {
            None
        } else {
            Some(RefreshScope::Repos(self.repos.clone()))
        }
    }
}

/// The `bd events tail` calls a follower makes, behind a seam so the follower
/// loop is testable without spawning `bd`.
pub trait JournalSource: Send + Sync + 'static {
    /// `bd -C <repo> events tail --since <since> --limit 1 --json`.
    fn probe(&self, repo: &Path, since: u64) -> ProbeOutput;
    /// `bd -C <repo> events tail --since <since> --follow --json`, calling
    /// `on_line` for each stdout line until the stream ends. Must return
    /// promptly once `stop` is set or [`JournalSource::interrupt`] is called.
    fn follow(&self, repo: &Path, since: u64, stop: &AtomicBool, on_line: &mut dyn FnMut(&str));
    /// Interrupt every running `follow` (shutdown).
    fn interrupt(&self);
    /// `bd -C <repo> config set events-journal true`, returning bd's error
    /// text on failure.
    fn enable(&self, repo: &Path) -> Result<(), String>;
}

/// The real [`JournalSource`]: spawns `bd` from PATH.
#[derive(Default)]
pub struct BdJournal {
    children: Mutex<Vec<(u32, Arc<Mutex<Child>>)>>,
}

impl BdJournal {
    pub fn new() -> Self {
        Self::default()
    }

    fn command(repo: &Path, since: u64) -> Command {
        let mut cmd = Command::new("bd");
        cmd.arg("-C")
            .arg(repo)
            .args(["events", "tail", "--since", &since.to_string(), "--json"])
            .stdin(Stdio::null());
        cmd
    }
}

impl JournalSource for BdJournal {
    fn probe(&self, repo: &Path, since: u64) -> ProbeOutput {
        let mut cmd = Self::command(repo, since);
        cmd.args(["--limit", "1"]);
        match cmd.output() {
            Ok(output) => ProbeOutput {
                success: output.status.success(),
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            },
            Err(error) => ProbeOutput {
                success: false,
                stdout: String::new(),
                stderr: format!("could not run bd: {error}"),
            },
        }
    }

    fn follow(&self, repo: &Path, since: u64, stop: &AtomicBool, on_line: &mut dyn FnMut(&str)) {
        let mut cmd = Self::command(repo, since);
        cmd.arg("--follow")
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let Ok(mut child) = cmd.spawn() else {
            return;
        };
        let Some(stdout) = child.stdout.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return;
        };
        let pid = child.id();
        let child = Arc::new(Mutex::new(child));
        self.children
            .lock()
            .expect("journal children poisoned")
            .push((pid, Arc::clone(&child)));
        // Registered before this check, so a stop that raced the spawn still
        // reaches this child: either `interrupt` saw it or we see the flag.
        if stop.load(Ordering::SeqCst) {
            let _ = child.lock().expect("journal child poisoned").kill();
        }
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => on_line(&line),
            }
        }
        // Drain anything left so the child never blocks on a full pipe.
        let _ = reader.read_to_end(&mut Vec::new());
        self.children
            .lock()
            .expect("journal children poisoned")
            .retain(|(other, _)| *other != pid);
        let _ = child.lock().expect("journal child poisoned").wait();
    }

    fn interrupt(&self) {
        for (_, child) in self
            .children
            .lock()
            .expect("journal children poisoned")
            .iter()
        {
            let _ = child.lock().expect("journal child poisoned").kill();
        }
    }

    fn enable(&self, repo: &Path) -> Result<(), String> {
        BdCli::new()
            .set_events_journal(repo, true)
            .map_err(|error| error.to_string())
    }
}

/// Follow one repo's journal until `stop` is set, sending [`Report`]s.
/// `may_enable` is asked, each time the journal is found off, whether Hank may
/// turn it on (false once the roster marks the repo `unwatched`).
pub fn follow_repo(
    source: &dyn JournalSource,
    repo: &Path,
    saved: Option<Checkpoint>,
    may_enable: &dyn Fn() -> bool,
    reports: &Sender<Report>,
    stop: &AtomicBool,
) {
    follow_repo_with_backoff(
        source,
        repo,
        saved,
        may_enable,
        reports,
        stop,
        INITIAL_BACKOFF,
    );
}

fn follow_repo_with_backoff(
    source: &dyn JournalSource,
    repo: &Path,
    saved: Option<Checkpoint>,
    may_enable: &dyn Fn() -> bool,
    reports: &Sender<Report>,
    stop: &AtomicBool,
    initial_backoff: Duration,
) {
    let mut current = saved;
    let mut backoff = initial_backoff;
    let mut warned = false;
    // One enable attempt per follower: if bd still reports the journal off
    // after it (or the write failed), re-checking is all that is left.
    let mut enable_tried = false;
    let mut warned_disabled = false;
    // Set when the previous stream ended on a truncation: we already know the
    // head to resume from, so skip the probe.
    let mut resume_at: Option<u64> = None;
    while !stop.load(Ordering::SeqCst) {
        let since = match resume_at.take() {
            Some(head) => head,
            None => {
                let probe_since = current.as_ref().map_or(0, |c| c.seq.saturating_sub(1));
                let probe = source.probe(repo, probe_since);
                match classify_probe(current.as_ref(), &probe) {
                    StartPoint::Resume(seq) => seq,
                    StartPoint::Restart => {
                        current = None;
                        0
                    }
                    StartPoint::Rebaseline { head } => {
                        current = None;
                        let _ = reports.send(Report::Rebaseline {
                            repo: repo.to_path_buf(),
                        });
                        head
                    }
                    StartPoint::Disabled => {
                        // An `unwatched` repo is left alone, quietly: the user
                        // opted it out, and `hank doctor` still lists it.
                        if may_enable() && !enable_tried {
                            enable_tried = true;
                            match source.enable(repo) {
                                Ok(()) => {
                                    let _ = reports.send(Report::Warning(format!(
                                        "live refresh turned on the events journal for {} (.beads/config.yaml; `hank repos unwatch` undoes it)",
                                        repo_label(repo)
                                    )));
                                    continue;
                                }
                                Err(detail) => {
                                    warned_disabled = true;
                                    let _ = reports.send(Report::Warning(format!(
                                        "live refresh off for {}: couldn't turn on its events journal: {}",
                                        repo_label(repo),
                                        first_line(&detail)
                                    )));
                                }
                            }
                        } else if may_enable() && !warned_disabled {
                            // Turned on, yet bd still reports it off (for one,
                            // `BD_EVENTS_JOURNAL=false` overrides the file).
                            warned_disabled = true;
                            let _ = reports.send(Report::Warning(format!(
                                "live refresh off for {}: bd still reports its events journal off",
                                repo_label(repo)
                            )));
                        }
                        sleep_unless_stopped(backoff, stop);
                        backoff = (backoff * 2).min(MAX_BACKOFF);
                        continue;
                    }
                    StartPoint::Failed(detail) => {
                        if !warned {
                            warned = true;
                            let _ = reports.send(Report::Warning(format!(
                                "live refresh unavailable for {}: {}",
                                repo_label(repo),
                                first_line(&detail)
                            )));
                        }
                        sleep_unless_stopped(backoff, stop);
                        backoff = (backoff * 2).min(MAX_BACKOFF);
                        continue;
                    }
                }
            }
        };

        let started = Instant::now();
        let mut unparsed = String::new();
        let mut truncated_at: Option<u64> = None;
        let mut on_line = |line: &str| match parse_line(line) {
            JournalLine::Record(checkpoint) => {
                current = Some(checkpoint.clone());
                let _ = reports.send(Report::Changed {
                    repo: repo.to_path_buf(),
                    checkpoint,
                });
            }
            JournalLine::Truncated { head } => truncated_at = Some(head),
            JournalLine::Other => unparsed.push_str(line),
        };
        source.follow(repo, since, stop, &mut on_line);
        if stop.load(Ordering::SeqCst) {
            return;
        }
        // A pre-stream truncation arrives pretty-printed across several lines.
        if truncated_at.is_none()
            && let JournalLine::Truncated { head } = parse_line(&unparsed)
        {
            truncated_at = Some(head);
        }
        if let Some(head) = truncated_at {
            current = None;
            let _ = reports.send(Report::Rebaseline {
                repo: repo.to_path_buf(),
            });
            resume_at = Some(head);
            continue;
        }
        // `--follow` only ends on its own when bd fails or is killed. Retry
        // from the checkpoint; a stream that stayed up a while resets backoff.
        if started.elapsed() > MAX_BACKOFF {
            backoff = initial_backoff;
        }
        sleep_unless_stopped(backoff, stop);
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// Collect reports into debounced batches until every sender is gone. Each
/// batch saves its checkpoints, then hands the UI its warnings and its refresh.
pub fn batch_reports(
    reports: &Receiver<Report>,
    checkpoints_file: &Path,
    mut saved: Checkpoints,
    deliver: &mut dyn FnMut(Msg),
) {
    while let Ok(first) = reports.recv() {
        let mut batch = Batch::default();
        batch.add(first);
        let deadline = Instant::now() + DEBOUNCE;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match reports.recv_timeout(left) {
                Ok(report) => batch.add(report),
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        let mut dirty = false;
        for (repo, checkpoint) in &batch.checkpoints {
            if saved.repos.get(repo) != Some(checkpoint) {
                saved.repos.insert(repo.clone(), checkpoint.clone());
                dirty = true;
            }
        }
        for repo in &batch.cleared {
            dirty |= saved.repos.remove(repo).is_some();
        }
        if dirty && let Err(error) = saved.save(checkpoints_file) {
            deliver(Msg::WatchWarning(sanitize(&format!(
                "couldn't save live-refresh checkpoints: {error:#}"
            ))));
        }
        for warning in &batch.warnings {
            deliver(Msg::WatchWarning(sanitize(warning)));
        }
        if let Some(scope) = batch.scope() {
            deliver(Msg::WatchChanged(scope));
        }
    }
}

/// The roster as the watcher sees it: the resolved repos to follow, and those
/// the user opted out (`unwatched`), whose journal Hank must not turn on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WatchList {
    pub repos: Vec<PathBuf>,
    pub opted_out: BTreeSet<PathBuf>,
}

impl From<Vec<PathBuf>> for WatchList {
    /// Every repo followed, none opted out.
    fn from(repos: Vec<PathBuf>) -> Self {
        WatchList {
            repos,
            opted_out: BTreeSet::new(),
        }
    }
}

/// A running watcher: one follower per repo plus the batcher. Repos can join
/// after start ([`Watcher::follow`]), so a roster that grows mid-session is
/// watched without a restart.
pub struct Watcher {
    stop: Arc<AtomicBool>,
    source: Arc<dyn JournalSource>,
    checkpoints_file: PathBuf,
    followers: Mutex<Followers>,
    /// The repos whose journal Hank may turn on: those in the latest roster
    /// and not opted out. Read by each follower when it finds its journal off,
    /// so an `unwatch` or `repos remove` run mid-session is honored even though
    /// the follower itself lives on until exit.
    enable_allowed: Arc<Mutex<BTreeSet<PathBuf>>>,
}

/// The mutable half of a [`Watcher`], behind one lock so a late
/// [`Watcher::follow`] can never race [`Watcher::stop`].
struct Followers {
    /// Cloned into each new follower; `None` once stopped. The batcher exits
    /// when this and every follower's clone are gone.
    reports: Option<Sender<Report>>,
    followed: BTreeSet<PathBuf>,
    handles: Vec<thread::JoinHandle<()>>,
}

impl Watcher {
    /// Start following `repos` (resolved, deduplicated roster paths), saving
    /// checkpoints to `checkpoints_file` and delivering messages via `deliver`.
    pub fn start(
        source: Arc<dyn JournalSource>,
        repos: impl Into<WatchList>,
        checkpoints_file: PathBuf,
        deliver: impl FnMut(Msg) + Send + 'static,
    ) -> Watcher {
        let saved = Checkpoints::load(&checkpoints_file);
        let (tx, rx) = mpsc::channel::<Report>();
        let batcher = {
            let checkpoints_file = checkpoints_file.clone();
            thread::spawn(move || {
                let mut deliver = deliver;
                batch_reports(&rx, &checkpoints_file, saved, &mut deliver);
            })
        };
        let watcher = Watcher {
            stop: Arc::new(AtomicBool::new(false)),
            source,
            checkpoints_file,
            followers: Mutex::new(Followers {
                reports: Some(tx),
                followed: BTreeSet::new(),
                handles: vec![batcher],
            }),
            enable_allowed: Arc::default(),
        };
        watcher.follow(repos);
        watcher
    }

    /// Start a follower for each of `repos` not already followed, and adopt
    /// its membership and opt-outs for every follower's enable decision. Each resumes from its saved checkpoint,
    /// so a repo watched in an earlier session picks up where it left off. A
    /// no-op once stopped. Repos are never unfollowed: a repo dropped from the
    /// roster keeps its follower until shutdown, and its reports only ask for a
    /// refresh the reloaded roster no longer runs.
    pub fn follow(&self, repos: impl Into<WatchList>) {
        let WatchList { repos, opted_out } = repos.into();
        *self
            .enable_allowed
            .lock()
            .expect("watch enable set poisoned") = repos
            .iter()
            .filter(|repo| !opted_out.contains(*repo))
            .cloned()
            .collect();
        let mut followers = self.followers.lock().expect("watch followers poisoned");
        let Followers {
            reports,
            followed,
            handles,
        } = &mut *followers;
        let Some(reports) = reports.as_ref() else {
            return;
        };
        let new: Vec<PathBuf> = repos
            .into_iter()
            .filter(|repo| followed.insert(repo.clone()))
            .collect();
        if new.is_empty() {
            return;
        }
        // Re-read rather than reuse the launch copy: the batcher has saved
        // newer checkpoints since, though none for a repo it never followed.
        let saved = Checkpoints::load(&self.checkpoints_file);
        handles.retain(|handle| !handle.is_finished());
        for repo in new {
            let source = Arc::clone(&self.source);
            let tx = reports.clone();
            let stop = Arc::clone(&self.stop);
            let checkpoint = saved.repos.get(&repo).cloned();
            let enable_allowed = Arc::clone(&self.enable_allowed);
            handles.push(thread::spawn(move || {
                let may_enable = || {
                    enable_allowed
                        .lock()
                        .expect("watch enable set poisoned")
                        .contains(&repo)
                };
                follow_repo(source.as_ref(), &repo, checkpoint, &may_enable, &tx, &stop);
            }));
        }
    }

    /// Stop every follower (killing its `bd` child) and join all threads.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        let handles = {
            let mut followers = self.followers.lock().expect("watch followers poisoned");
            followers.reports = None;
            std::mem::take(&mut followers.handles)
        };
        self.source.interrupt();
        for handle in handles {
            let _ = handle.join();
        }
    }
}

fn sleep_unless_stopped(duration: Duration, stop: &AtomicBool) {
    let deadline = Instant::now() + duration;
    while !stop.load(Ordering::SeqCst) {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return;
        }
        thread::sleep(left.min(STOP_POLL));
    }
}

/// A repo's directory name for the status bar, where a full path would push
/// the actionable part of a warning off-screen.
fn repo_label(repo: &Path) -> String {
    repo.file_name()
        .unwrap_or(repo.as_os_str())
        .to_string_lossy()
        .into_owned()
}

fn first_line(text: &str) -> &str {
    text.lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
        .trim()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    fn cp(seq: u64, ts: &str) -> Checkpoint {
        Checkpoint {
            seq,
            ts: ts.to_string(),
        }
    }

    fn record(seq: u64, ts: &str) -> String {
        format!(
            r#"{{"seq":{seq},"ts":"{ts}","op":"update","issue_id":"r-1","issue":{{"id":"r-1"}}}}"#
        )
    }

    const PRETTY_TRUNCATED: &str = r#"{
  "code": "events_journal_truncated",
  "error": "events journal truncated: checkpoint 0 is below the retained window [5..6]",
  "floor": 5,
  "head": 6,
  "schema_version": 1,
  "since": 0
}
"#;

    const COMPACT_TRUNCATED: &str =
        r#"{"code":"events_journal_truncated","error":"x","floor":41,"head":980,"since":12}"#;

    fn ok(stdout: &str) -> ProbeOutput {
        ProbeOutput {
            success: true,
            stdout: stdout.to_string(),
            stderr: String::new(),
        }
    }

    #[test]
    fn parses_records_and_both_truncation_shapes() {
        assert_eq!(
            parse_line(&record(7, "t7")),
            JournalLine::Record(cp(7, "t7"))
        );
        assert_eq!(
            parse_line(COMPACT_TRUNCATED),
            JournalLine::Truncated { head: 980 }
        );
        assert_eq!(
            parse_line(PRETTY_TRUNCATED),
            JournalLine::Truncated { head: 6 }
        );
        assert_eq!(parse_line("  \n"), JournalLine::Other);
        assert_eq!(parse_line("{"), JournalLine::Other);
        assert_eq!(parse_line(r#"{"code":"other"}"#), JournalLine::Other);
    }

    #[test]
    fn probe_without_checkpoint_follows_from_zero() {
        assert_eq!(classify_probe(None, &ok("")), StartPoint::Resume(0));
        assert_eq!(
            classify_probe(None, &ok(&record(1, "t1"))),
            StartPoint::Resume(0)
        );
    }

    #[test]
    fn probe_resumes_only_when_the_saved_record_matches() {
        let saved = cp(5, "t5");
        assert_eq!(
            classify_probe(Some(&saved), &ok(&record(5, "t5"))),
            StartPoint::Resume(5)
        );
        assert_eq!(
            classify_probe(Some(&saved), &ok(&record(5, "other"))),
            StartPoint::Restart,
            "same seq, different record: another clone's journal"
        );
        assert_eq!(
            classify_probe(Some(&saved), &ok("")),
            StartPoint::Restart,
            "a checkpoint above this journal's head would stall forever"
        );
    }

    #[test]
    fn probe_detects_truncation_disabled_journal_and_failure() {
        let truncated = ProbeOutput {
            success: false,
            stdout: PRETTY_TRUNCATED.to_string(),
            stderr: String::new(),
        };
        assert_eq!(
            classify_probe(Some(&cp(1, "t1")), &truncated),
            StartPoint::Rebaseline { head: 6 }
        );
        let disabled = ProbeOutput {
            success: true,
            stdout: String::new(),
            stderr: "note: the events journal is disabled for this workspace (enable with ...)"
                .to_string(),
        };
        assert_eq!(classify_probe(None, &disabled), StartPoint::Disabled);
        let failed = ProbeOutput {
            success: false,
            stdout: String::new(),
            stderr: "Error: unknown command \"events\" for \"bd\"\n".to_string(),
        };
        assert_eq!(
            classify_probe(None, &failed),
            StartPoint::Failed("Error: unknown command \"events\" for \"bd\"".to_string())
        );
    }

    #[test]
    fn batch_coalesces_repos_and_keeps_the_highest_checkpoint() {
        let mut batch = Batch::default();
        for (repo, seq) in [("/a", 3), ("/b", 1), ("/a", 5), ("/a", 4)] {
            batch.add(Report::Changed {
                repo: PathBuf::from(repo),
                checkpoint: cp(seq, &format!("t{seq}")),
            });
        }
        batch.add(Report::Warning("w".into()));
        batch.add(Report::Warning("w".into()));
        assert_eq!(
            batch.scope(),
            Some(RefreshScope::Repos(
                [PathBuf::from("/a"), PathBuf::from("/b")].into()
            ))
        );
        assert_eq!(batch.checkpoints[&PathBuf::from("/a")], cp(5, "t5"));
        assert_eq!(batch.warnings, vec!["w".to_string()]);
    }

    #[test]
    fn rebaseline_widens_the_batch_and_forgets_the_pruned_checkpoint() {
        let mut batch = Batch::default();
        batch.add(Report::Changed {
            repo: PathBuf::from("/a"),
            checkpoint: cp(3, "t3"),
        });
        batch.add(Report::Rebaseline {
            repo: PathBuf::from("/a"),
        });
        assert_eq!(batch.scope(), Some(RefreshScope::Full));
        assert!(batch.checkpoints.is_empty());
        assert!(batch.cleared.contains(Path::new("/a")));
        assert_eq!(
            Batch::default().scope(),
            None,
            "warnings alone refresh nothing"
        );
    }

    #[test]
    fn checkpoints_round_trip_and_tolerate_a_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("nested/events_checkpoints.json");
        assert_eq!(Checkpoints::load(&file), Checkpoints::default());
        let mut saved = Checkpoints::default();
        saved.repos.insert(PathBuf::from("/a"), cp(9, "t9"));
        saved.save(&file).unwrap();
        assert_eq!(Checkpoints::load(&file), saved);
        fs::write(&file, b"not json").unwrap();
        assert_eq!(Checkpoints::load(&file), Checkpoints::default());
    }

    /// A scripted journal: probes and follow streams are consumed in order. An
    /// unscripted probe reports an empty journal; an unscripted follow stops
    /// the follower.
    #[derive(Default)]
    struct FakeJournal {
        probes: Mutex<VecDeque<ProbeOutput>>,
        streams: Mutex<VecDeque<Vec<String>>>,
        calls: Mutex<Vec<String>>,
        enable_error: Option<String>,
    }

    impl FakeJournal {
        fn probe(self, output: ProbeOutput) -> Self {
            self.probes.lock().unwrap().push_back(output);
            self
        }
        fn stream(self, lines: &[&str]) -> Self {
            self.streams
                .lock()
                .unwrap()
                .push_back(lines.iter().map(|l| format!("{l}\n")).collect());
            self
        }
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
        fn failing_enable(mut self, error: &str) -> Self {
            self.enable_error = Some(error.to_string());
            self
        }
    }

    impl JournalSource for FakeJournal {
        fn probe(&self, _repo: &Path, since: u64) -> ProbeOutput {
            self.calls.lock().unwrap().push(format!("probe {since}"));
            self.probes
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| ok(""))
        }

        fn follow(
            &self,
            _repo: &Path,
            since: u64,
            stop: &AtomicBool,
            on_line: &mut dyn FnMut(&str),
        ) {
            self.calls.lock().unwrap().push(format!("follow {since}"));
            match self.streams.lock().unwrap().pop_front() {
                Some(lines) => lines.iter().for_each(|line| on_line(line)),
                None => stop.store(true, Ordering::SeqCst),
            }
        }

        fn interrupt(&self) {}

        fn enable(&self, _repo: &Path) -> Result<(), String> {
            self.calls.lock().unwrap().push("enable".to_string());
            match &self.enable_error {
                Some(error) => Err(error.clone()),
                None => Ok(()),
            }
        }
    }

    fn run(source: &FakeJournal, saved: Option<Checkpoint>) -> Vec<Report> {
        run_with(source, saved, true)
    }

    fn run_with(source: &FakeJournal, saved: Option<Checkpoint>, may_enable: bool) -> Vec<Report> {
        let (tx, rx) = mpsc::channel();
        let stop = AtomicBool::new(false);
        follow_repo_with_backoff(
            source,
            Path::new("/r"),
            saved,
            &|| may_enable,
            &tx,
            &stop,
            Duration::ZERO,
        );
        drop(tx);
        rx.into_iter().collect()
    }

    fn changed(seq: u64) -> Report {
        Report::Changed {
            repo: PathBuf::from("/r"),
            checkpoint: cp(seq, &format!("t{seq}")),
        }
    }

    #[test]
    fn follower_resumes_from_a_verified_checkpoint_and_reports_each_record() {
        let source = FakeJournal::default()
            .probe(ok(&record(4, "t4")))
            .stream(&[&record(5, "t5"), &record(6, "t6")]);
        let reports = run(&source, Some(cp(4, "t4")));
        assert_eq!(reports, vec![changed(5), changed(6)]);
        assert_eq!(source.calls()[..3], ["probe 3", "follow 4", "probe 5"]);
    }

    #[test]
    fn follower_restarts_from_zero_when_the_checkpoint_names_another_journal() {
        let source = FakeJournal::default()
            .probe(ok(""))
            .stream(&[&record(1, "t1")]);
        let reports = run(&source, Some(cp(40, "t40")));
        assert_eq!(reports, vec![changed(1)]);
        assert_eq!(source.calls()[..2], ["probe 39", "follow 0"]);
    }

    #[test]
    fn mid_stream_truncation_rebaselines_and_resumes_from_head_without_probing() {
        let source = FakeJournal::default()
            .probe(ok(""))
            .stream(&[&record(1, "t1"), COMPACT_TRUNCATED])
            .stream(&[&record(981, "t981")]);
        let reports = run(&source, None);
        assert_eq!(
            reports,
            vec![
                changed(1),
                Report::Rebaseline {
                    repo: PathBuf::from("/r")
                },
                changed(981),
            ]
        );
        assert_eq!(source.calls()[..3], ["probe 0", "follow 0", "follow 980"]);
    }

    #[test]
    fn pruned_checkpoint_on_resume_rebaselines_from_head() {
        let source = FakeJournal::default().probe(ProbeOutput {
            success: false,
            stdout: PRETTY_TRUNCATED.to_string(),
            stderr: String::new(),
        });
        let reports = run(&source, Some(cp(2, "t2")));
        assert_eq!(
            reports,
            vec![Report::Rebaseline {
                repo: PathBuf::from("/r")
            }]
        );
        assert_eq!(source.calls()[..2], ["probe 1", "follow 6"]);
    }

    fn disabled() -> ProbeOutput {
        ProbeOutput {
            success: true,
            stdout: String::new(),
            stderr: "note: the events journal is disabled for this workspace".to_string(),
        }
    }

    #[test]
    fn disabled_journal_is_turned_on_announced_and_then_followed() {
        let source = FakeJournal::default()
            .probe(disabled())
            .probe(ok(""))
            .stream(&[&record(1, "t1")]);
        let reports = run(&source, None);
        assert_eq!(reports.len(), 2, "{reports:?}");
        assert!(
            matches!(&reports[0], Report::Warning(w)
                if w.contains("turned on the events journal") && w.contains(".beads/config.yaml")),
            "{reports:?}"
        );
        assert_eq!(reports[1], changed(1));
        assert_eq!(
            source.calls()[..4],
            ["probe 0", "enable", "probe 0", "follow 0"]
        );
    }

    #[test]
    fn opted_out_journal_is_left_off_quietly_and_rechecked() {
        // Off twice, then on: someone ran `hank repos watch` mid-session.
        let source = FakeJournal::default()
            .probe(disabled())
            .probe(disabled())
            .probe(ok(""))
            .stream(&[&record(1, "t1")]);
        let reports = run_with(&source, None, false);
        assert_eq!(reports, vec![changed(1)], "no enable, no nagging");
        assert_eq!(
            source.calls()[..4],
            ["probe 0", "probe 0", "probe 0", "follow 0"]
        );
    }

    #[test]
    fn failed_enable_warns_once_and_keeps_rechecking() {
        let source = FakeJournal::default()
            .failing_enable("config.yaml is read-only")
            .probe(disabled())
            .probe(disabled())
            .probe(disabled())
            .probe(ok(""));
        let reports = run(&source, None);
        assert_eq!(reports.len(), 1, "{reports:?}");
        assert!(
            matches!(&reports[0], Report::Warning(w) if w.contains("read-only")),
            "{reports:?}"
        );
        let calls = source.calls();
        assert_eq!(
            calls.iter().filter(|call| *call == "enable").count(),
            1,
            "one enable attempt per follower: {calls:?}"
        );
        assert_eq!(calls.last().map(String::as_str), Some("follow 0"));
    }

    #[test]
    fn journal_still_off_after_enable_warns_once() {
        let source = FakeJournal::default()
            .probe(disabled())
            .probe(disabled())
            .probe(disabled());
        let reports = run(&source, None);
        assert_eq!(reports.len(), 2, "{reports:?}");
        assert!(
            matches!(&reports[1], Report::Warning(w) if w.contains("still reports")),
            "{reports:?}"
        );
    }

    #[test]
    fn batcher_saves_checkpoints_and_requests_one_scoped_refresh() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("events_checkpoints.json");
        let (tx, rx) = mpsc::channel();
        tx.send(changed(1)).unwrap();
        tx.send(changed(2)).unwrap();
        tx.send(Report::Warning("journal off".into())).unwrap();
        drop(tx);
        let mut delivered = Vec::new();
        batch_reports(&rx, &file, Checkpoints::default(), &mut |msg| {
            delivered.push(msg)
        });
        assert_eq!(
            delivered,
            vec![
                Msg::WatchWarning("journal off".into()),
                Msg::WatchChanged(RefreshScope::Repos([PathBuf::from("/r")].into())),
            ]
        );
        assert_eq!(
            Checkpoints::load(&file).repos[&PathBuf::from("/r")],
            cp(2, "t2")
        );
    }

    #[test]
    fn batcher_forgets_a_pruned_checkpoint_and_requests_a_full_refresh() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("events_checkpoints.json");
        let mut saved = Checkpoints::default();
        saved.repos.insert(PathBuf::from("/r"), cp(2, "t2"));
        saved.save(&file).unwrap();
        let (tx, rx) = mpsc::channel();
        tx.send(Report::Rebaseline {
            repo: PathBuf::from("/r"),
        })
        .unwrap();
        drop(tx);
        let mut delivered = Vec::new();
        batch_reports(&rx, &file, saved, &mut |msg| delivered.push(msg));
        assert_eq!(delivered, vec![Msg::WatchChanged(RefreshScope::Full)]);
        assert!(Checkpoints::load(&file).repos.is_empty());
    }

    /// A journal whose follows stay open until the watcher stops, recording
    /// which repo each one was for and where it resumed.
    struct ParkedJournal {
        follows: Mutex<Sender<String>>,
    }

    impl JournalSource for ParkedJournal {
        fn probe(&self, _repo: &Path, _since: u64) -> ProbeOutput {
            ok(&record(7, "t7"))
        }

        fn follow(
            &self,
            repo: &Path,
            since: u64,
            stop: &AtomicBool,
            _on_line: &mut dyn FnMut(&str),
        ) {
            let _ = self
                .follows
                .lock()
                .unwrap()
                .send(format!("{} {since}", repo.display()));
            while !stop.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(5));
            }
        }

        fn interrupt(&self) {}

        fn enable(&self, _repo: &Path) -> Result<(), String> {
            Ok(())
        }
    }

    #[test]
    fn watcher_follows_repos_added_after_start_once_each() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("events_checkpoints.json");
        // /b was watched in an earlier session: it resumes, not replays.
        let mut saved = Checkpoints::default();
        saved.repos.insert(PathBuf::from("/b"), cp(7, "t7"));
        saved.save(&file).unwrap();
        let (tx, follows) = mpsc::channel();
        let source = Arc::new(ParkedJournal {
            follows: Mutex::new(tx),
        });
        let watcher = Watcher::start(source, vec![PathBuf::from("/a")], file, |_| {});

        watcher.follow(vec![PathBuf::from("/a"), PathBuf::from("/b")]);
        let mut seen: Vec<String> = (0..2)
            .map(|_| follows.recv_timeout(Duration::from_secs(5)).unwrap())
            .collect();
        seen.sort();
        assert_eq!(seen, ["/a 0", "/b 7"]);

        watcher.stop();
        watcher.follow(vec![PathBuf::from("/c")]);
        assert!(
            follows.try_recv().is_err(),
            "no second /a follower, and nothing new after stop"
        );
    }

    /// A journal that is always off, whose first probe waits for the test's
    /// go-ahead, recording every enable.
    struct GatedOffJournal {
        gate: Mutex<Option<Receiver<()>>>,
        enables: Mutex<Vec<PathBuf>>,
    }

    impl JournalSource for GatedOffJournal {
        fn probe(&self, _repo: &Path, _since: u64) -> ProbeOutput {
            if let Some(gate) = self.gate.lock().unwrap().take() {
                let _ = gate.recv_timeout(Duration::from_secs(5));
            }
            disabled()
        }

        fn follow(
            &self,
            _repo: &Path,
            _since: u64,
            _stop: &AtomicBool,
            _on_line: &mut dyn FnMut(&str),
        ) {
        }

        fn interrupt(&self) {}

        fn enable(&self, repo: &Path) -> Result<(), String> {
            self.enables.lock().unwrap().push(repo.to_path_buf());
            Ok(())
        }
    }

    #[test]
    fn watcher_never_enables_a_repo_dropped_from_the_roster() {
        let tmp = tempfile::tempdir().unwrap();
        let (go, gate) = mpsc::channel();
        let source = Arc::new(GatedOffJournal {
            gate: Mutex::new(Some(gate)),
            enables: Mutex::new(Vec::new()),
        });
        let watcher = Watcher::start(
            Arc::clone(&source) as Arc<dyn JournalSource>,
            vec![PathBuf::from("/a")],
            tmp.path().join("events_checkpoints.json"),
            |_| {},
        );

        // `hank repos remove /a` ran; its follower lives on until exit.
        watcher.follow(WatchList::default());
        go.send(()).unwrap();
        thread::sleep(Duration::from_millis(300));
        watcher.stop();

        assert!(
            source.enables.lock().unwrap().is_empty(),
            "a repo no longer on the roster is never written to"
        );
    }
}
