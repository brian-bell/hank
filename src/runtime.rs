//! The terminal runtime: the event loop that turns the pure Slice 8 [`App`] core
//! into a running TUI. A crossterm event thread and a refresh worker thread both
//! feed one `mpsc` channel of [`Msg`]; the UI thread `recv`s each message, calls
//! [`App::reduce`], executes the returned [`Effect`]s, and redraws via
//! [`crate::app::view::draw`]. Terminal setup/teardown installs a panic hook that
//! restores the terminal (the session-tui pattern). See `plans/slices/slice-9.md`.

use std::collections::{HashMap, HashSet};
use std::io::{self, Stdout, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Mutex, OnceLock, RwLock};
use std::thread;
use std::time::{Duration, SystemTime};

use crossterm::event::{self, Event, KeyEvent};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::app::{App, Effect, Msg, context, keys, view};
use crate::bd::{BdCli, BdClient, RepoSyncReport};
use crate::cache;
use crate::cli::{CliError, sanitize, version_gate};
use crate::config::{Config, Paths};
use crate::hub::{ReconcileWitness, ensure_hub, hub_dir, reconcile_witness};
use crate::refresh::{self, AttributionGeneration, HubGenerationToken, RefreshError};
use crate::snapshot::{self, Row, Snapshot};
use crate::ui_state;

/// How long the event thread blocks on `event::poll` before re-checking the stop
/// flag, so a quit is observed promptly without a busy loop.
const INPUT_POLL: Duration = Duration::from_millis(100);

/// How long the UI thread waits for a message before redrawing anyway, so the
/// status bar's last-refreshed age advances while the user is idle.
const TICK: Duration = Duration::from_secs(1);

/// The most characters of a copied command/block shown in the status-bar
/// confirmation before it is truncated with an ellipsis.
const COPY_SUMMARY_MAX: usize = 72;

type Tui = Terminal<CrosstermBackend<Stdout>>;

#[derive(Debug, Clone, Default)]
pub struct PipelineMetrics {
    pub total: Duration,
    pub version: Duration,
    pub reconcile: Duration,
    pub source_wall: Duration,
    pub sync: Duration,
    pub ready: Duration,
    pub calls: OperationCounts,
    pub sync_report: Option<RepoSyncReport>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct OperationCounts {
    pub version: usize,
    pub export: usize,
    pub issue_prefix: usize,
    pub repo_sync: usize,
    pub ready: usize,
    pub search: usize,
}

#[derive(Default)]
pub(crate) struct RuntimeRefreshState {
    compatibility: OnceLock<Result<(), String>>,
    reconciled: Mutex<Option<ReconcileWitness>>,
    hub_access: RwLock<HubGenerationState>,
    maps: Mutex<HashMap<AttributionGeneration, Arc<refresh::PrefixMap>>>,
}

#[derive(Debug, Clone)]
struct VerifiedAttribution {
    generation: AttributionGeneration,
    hub_token: HubGenerationToken,
    candidate: refresh::AttributionCandidate,
}

#[derive(Debug, Default)]
struct HubGenerationState {
    next_generation: u64,
    current_hub: Option<VerifiedAttribution>,
}

impl RuntimeRefreshState {
    fn map_for(&self, generation: AttributionGeneration) -> Option<Arc<refresh::PrefixMap>> {
        self.maps
            .lock()
            .expect("attribution map registry poisoned")
            .get(&generation)
            .cloned()
    }

    fn register_map(&self, generation: AttributionGeneration, map: Arc<refresh::PrefixMap>) {
        self.maps
            .lock()
            .expect("attribution map registry poisoned")
            .insert(generation, map);
    }

    fn prune(&self, retained: &HashSet<AttributionGeneration>) {
        let Ok(hub) = self.hub_access.try_read() else {
            return;
        };
        let current = hub.current_hub.as_ref().map(|current| current.generation);
        self.maps
            .lock()
            .expect("attribution map registry poisoned")
            .retain(|generation, _| Some(*generation) == current || retained.contains(generation));
    }
}

/// What the UI thread consumes from its one channel: a **raw** key event from the
/// input thread, or an app [`Msg`] from a background worker.
///
/// Keys arrive raw (not pre-decoded) so [`keys::map_key`] runs on the UI thread
/// against the app's *live* search-input focus. Because the channel preserves
/// order, a `/` that opens the search editor is reduced before the next key is
/// decoded — so a pasted `/query` burst can never decode `query`'s characters as
/// commands (e.g. `q` quitting) the way a producer-thread decoder racing an
/// asynchronously-published mode flag could.
#[derive(Debug)]
pub(crate) enum Incoming {
    /// A raw key press/repeat, decoded on the UI thread against live app state.
    Key(KeyEvent),
    /// An app message from a background worker (refresh / detail / search).
    Msg(Msg),
    /// A row-bearing result whose immutable attribution map must be registered
    /// before the app can retain those rows.
    AttributedMsg {
        msg: Msg,
        generation: AttributionGeneration,
        map: Arc<refresh::PrefixMap>,
    },
}

impl From<Msg> for Incoming {
    fn from(msg: Msg) -> Self {
        Incoming::Msg(msg)
    }
}

/// Launch the interactive TUI (bare `hank`). Sets up the terminal, runs the event
/// loop against `roster`, and always restores the terminal before returning —
/// even on error, so a failure never leaves the user's terminal wedged.
pub fn run(paths: &Paths, roster: Config) -> Result<(), CliError> {
    let mut terminal = setup_terminal().map_err(CliError::Io)?;
    let loop_result = event_loop(&mut terminal, paths, &roster);
    let restore_result = restore_terminal(&mut terminal);
    // Surface a loop failure first; a restore failure only if the loop was fine.
    loop_result?;
    restore_result.map_err(CliError::Io)?;
    Ok(())
}

/// The UI thread: spawn the input + initial-refresh producers, then consume
/// messages, reduce, execute effects, and redraw until the app is done.
fn event_loop(terminal: &mut Tui, paths: &Paths, roster: &Config) -> Result<(), CliError> {
    let (tx, rx) = mpsc::channel::<Incoming>();
    let stop = Arc::new(AtomicBool::new(false));

    let input_handle = {
        let tx = tx.clone();
        let stop = Arc::clone(&stop);
        thread::spawn(move || input_thread(&tx, &stop))
    };

    // A fresh (<12h) on-disk cache paints instantly, before the real refresh
    // below has a chance to land, so launch never sits in `Loading` behind a
    // slow `bd ready` when yesterday's rows would do. `hydrate_from_cache`
    // (unlike `reduce(Msg::RefreshCompleted { .. })`) leaves `stale` alone,
    // so the born-stale in-flight guard the constructor reserves for the launch
    // refresh below stays armed the whole time. A stale/missing/corrupt
    // cache is a silent no-op: the app stays `Loading` exactly as before
    // this existed.
    let mut app = initial_app(paths, roster, SystemTime::now());
    // In-flight background workers (refresh *and* detail), tracked so shutdown can
    // wait for the running bd subprocess to finish and release the hub lock —
    // never orphaning a child that would keep mutating the hub after hank's lock
    // has dropped. Finished handles are pruned on each new spawn so the vec cannot
    // grow across a long session (the Slice 8 guard bounds live refresh workers to
    // one; detail fetches are short and pruned likewise).
    let mut worker_handles: Vec<thread::JoinHandle<()>> = Vec::new();
    let refresh_state = Arc::new(RuntimeRefreshState::default());
    // The App is born stale; launch immediately kicks off the first refresh.
    worker_handles.push(spawn_refresh(
        &tx,
        paths,
        roster,
        Arc::clone(&refresh_state),
    ));

    // Run the render/reduce loop, then join threads *unconditionally* — for a
    // clean quit and for every error return alike — so a terminal write failure
    // can never detach the input thread or an in-flight worker (which would
    // orphan its bd subprocess while our process exits and drops the hub lock).
    let result = ui_loop(
        terminal,
        &rx,
        &tx,
        &mut app,
        &mut worker_handles,
        paths,
        roster,
        &refresh_state,
    );
    stop.store(true, Ordering::SeqCst);
    let _ = input_handle.join();
    for handle in worker_handles {
        let _ = handle.join();
    }
    result
}

/// Restore persisted UI preferences before hydrating cached rows, preserving
/// the app's born-stale launch-refresh guard.
fn initial_app(paths: &Paths, roster: &Config, now: SystemTime) -> App {
    let mut app = App::with_repo_view(ui_state::load(paths.ui_state_file()));
    if let Some(snapshot) = cache::load(paths.cache_file(), now, roster) {
        app.hydrate_from_cache(snapshot);
    }
    app
}

/// The render/reduce loop, factored out so [`event_loop`] can join its threads
/// whether this returns `Ok` (a `q` quit) or `Err` (a terminal draw failure).
#[allow(clippy::too_many_arguments)]
fn ui_loop(
    terminal: &mut Tui,
    rx: &Receiver<Incoming>,
    tx: &Sender<Incoming>,
    app: &mut App,
    worker_handles: &mut Vec<thread::JoinHandle<()>>,
    paths: &Paths,
    roster: &Config,
    refresh_state: &Arc<RuntimeRefreshState>,
) -> Result<(), CliError> {
    draw(terminal, app)?;
    // Redraw on every message and on every idle tick, so the staleness age keeps
    // advancing even while no messages arrive. `Disconnected` cannot occur while
    // the caller still holds `tx`, but is handled defensively as a clean exit.
    loop {
        match rx.recv_timeout(TICK) {
            Ok(incoming) => {
                // Decode a raw key against the app's *current* search focus (so a
                // pasted `/query` burst can't run `query` as commands); worker
                // messages pass through. An unmapped key yields no message.
                let msg = match incoming {
                    Incoming::Key(key) => keys::map_key(key, app.input_context()),
                    Incoming::Msg(msg) => Some(msg),
                    Incoming::AttributedMsg {
                        msg,
                        generation,
                        map,
                    } => {
                        refresh_state.register_map(generation, map);
                        Some(msg)
                    }
                };
                if let Some(msg) = msg {
                    for effect in app.reduce(msg) {
                        execute_effect(effect, tx, worker_handles, paths, roster, refresh_state);
                    }
                    refresh_state.prune(&app.attribution_generations());
                    if app.is_done() {
                        return Ok(());
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        }
        draw(terminal, app)?;
    }
}

/// Perform one [`Effect`] by spawning the matching background worker, tracking
/// its handle for shutdown. The single dispatch point for every effect `reduce`
/// returns — Slice 11's `Effect::Search` slots in as one more arm with no change
/// to [`ui_loop`]. Finished handles are pruned first so the vec stays bounded.
fn execute_effect(
    effect: Effect,
    tx: &Sender<Incoming>,
    worker_handles: &mut Vec<thread::JoinHandle<()>>,
    paths: &Paths,
    roster: &Config,
    refresh_state: &Arc<RuntimeRefreshState>,
) {
    worker_handles.retain(|h| !h.is_finished());
    let handle = match effect {
        Effect::Refresh => spawn_refresh(tx, paths, roster, Arc::clone(refresh_state)),
        Effect::FetchDetail { id, token } => spawn_detail(tx, paths, id, token),
        Effect::Search { query, token } => {
            spawn_search(tx, paths, query, token, Arc::clone(refresh_state))
        }
        Effect::Copy {
            row,
            markdown,
            token,
        } => {
            let map = row
                .attribution_generation
                .and_then(|generation| refresh_state.map_for(generation));
            spawn_copy(tx, paths, *row, markdown, token, map)
        }
        // Not a worker: write the OSC 52 escape here, on the UI thread that owns
        // the tty, so it can never interleave with a ratatui draw. Returns without
        // a handle to track.
        Effect::WriteClipboard(payload) => {
            write_clipboard(&payload);
            return;
        }
        Effect::PersistRepoView(repo) => {
            let result = ui_state::save(paths.ui_state_file(), &repo)
                .map_err(|error| sanitize(&format!("couldn't save repository view: {error}")));
            let _ = tx.send(Msg::RepoViewPersisted { repo, result }.into());
            return;
        }
    };
    worker_handles.push(handle);
}

/// Render the current state with a fresh `now` for the staleness age.
fn draw(terminal: &mut Tui, app: &mut App) -> Result<(), CliError> {
    let mut detail_max_scroll = None;
    terminal
        .draw(|frame| {
            detail_max_scroll = view::draw(frame, app, SystemTime::now());
        })
        .map_err(CliError::Io)?;
    if let Some(max_scroll) = detail_max_scroll {
        let effects = app.reduce(Msg::DetailScrollBounds { max_scroll });
        debug_assert!(effects.is_empty());
    }
    Ok(())
}

/// Spawn a background refresh worker that reports over `tx`, returning its join
/// handle so the event loop can wait for it on shutdown. Clones the roster and
/// paths into the thread and builds a fresh [`BdCli`] (stateless).
fn spawn_refresh(
    tx: &Sender<Incoming>,
    paths: &Paths,
    roster: &Config,
    state: Arc<RuntimeRefreshState>,
) -> thread::JoinHandle<()> {
    let tx = tx.clone();
    let paths = paths.clone();
    let roster = roster.clone();
    thread::spawn(move || refresh_worker_with_state(BdCli::new(), roster, paths, tx, state))
}

/// The refresh worker body: announce the start, run the pipeline, cache a
/// successful snapshot to disk (best-effort — a write failure never blocks
/// delivery), then send exactly one atomic completion. Owned args so it moves
/// cleanly into a thread; unit-tested directly with a [`crate::bd::FakeBdClient`]
/// and a channel.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn refresh_worker(
    bd: impl BdClient,
    roster: Config,
    paths: Paths,
    tx: Sender<Incoming>,
) {
    refresh_worker_with_state(
        bd,
        roster,
        paths,
        tx,
        Arc::new(RuntimeRefreshState::default()),
    );
}

fn refresh_worker_with_state(
    bd: impl BdClient,
    roster: Config,
    paths: Paths,
    tx: Sender<Incoming>,
    state: Arc<RuntimeRefreshState>,
) {
    let _ = tx.send(Msg::RefreshStarted.into());
    let (snapshot, warnings) = gather_snapshot_with_state(&bd, &roster, &paths, &state);
    if let Some(snapshot) = &snapshot {
        let _ = cache::save(paths.cache_file(), snapshot, &roster);
    }
    let _ = tx.send(Msg::RefreshCompleted { snapshot, warnings }.into());
}

/// Spawn a background detail worker that reports over `tx`, returning its join
/// handle so the event loop can wait for it on shutdown. Clones the paths into
/// the thread and builds a fresh [`BdCli`] (stateless).
fn spawn_detail(
    tx: &Sender<Incoming>,
    paths: &Paths,
    id: String,
    token: u64,
) -> thread::JoinHandle<()> {
    let tx = tx.clone();
    let paths = paths.clone();
    thread::spawn(move || detail_worker(BdCli::new(), paths, id, token, tx))
}

/// The detail worker body: fetch one issue's detail and send exactly one
/// [`Msg::DetailReady`] echoing `token` (so a superseded response can be dropped).
/// Owned args so it moves cleanly into a thread; unit-tested directly with a
/// [`crate::bd::FakeBdClient`] and a channel.
pub(crate) fn detail_worker(
    bd: impl BdClient,
    paths: Paths,
    id: String,
    token: u64,
    tx: Sender<Incoming>,
) {
    let detail = gather_detail(&bd, &paths, &id);
    let _ = tx.send(Msg::DetailReady { token, detail }.into());
}

/// Run `bd show <id>` against the hub, mapping a [`BdError`] to a
/// pre-formatted, [`sanitize`]d message for the pane. No version gate or
/// `ensure_hub`: the detail pane is reachable only from the list, i.e. after a
/// snapshot already hydrated the hub.
pub(crate) fn gather_detail(bd: &impl BdClient, paths: &Paths, id: &str) -> Result<String, String> {
    bd.show(&hub_dir(paths), id)
        .map_err(|e| sanitize(&format!("couldn't load {id}: {e}")))
}

/// Spawn a background search worker that reports over `tx`, returning its join
/// handle so the event loop can wait for it on shutdown.
fn spawn_search(
    tx: &Sender<Incoming>,
    paths: &Paths,
    query: String,
    token: u64,
    state: Arc<RuntimeRefreshState>,
) -> thread::JoinHandle<()> {
    let tx = tx.clone();
    let paths = paths.clone();
    thread::spawn(move || search_worker_with_state(BdCli::new(), paths, query, token, tx, state))
}

/// The search worker body: run the query, attribute the results, and send exactly
/// one [`Msg::SearchResults`] echoing `token` (so a superseded response can be
/// dropped). Owned args so it moves cleanly into a thread; unit-tested directly
/// with a [`crate::bd::FakeBdClient`] and a channel.
#[cfg(test)]
pub(crate) fn search_worker(
    bd: impl BdClient,
    roster: Config,
    paths: Paths,
    query: String,
    token: u64,
    tx: Sender<Incoming>,
) {
    let rows = gather_search(&bd, &roster, &paths, &query);
    let _ = tx.send(Msg::SearchResults { token, rows }.into());
}

fn search_worker_with_state(
    bd: impl BdClient,
    paths: Paths,
    query: String,
    token: u64,
    tx: Sender<Incoming>,
    state: Arc<RuntimeRefreshState>,
) {
    match gather_search_with_state(&bd, &paths, &query, &state) {
        Ok((rows, verified)) => {
            let _ = tx.send(Incoming::AttributedMsg {
                msg: Msg::SearchResults {
                    token,
                    rows: Ok(rows),
                },
                generation: verified.generation,
                map: Arc::clone(&verified.candidate.prefix_map),
            });
        }
        Err(message) => {
            let _ = tx.send(
                Msg::SearchResults {
                    token,
                    rows: Err(message),
                }
                .into(),
            );
        }
    }
}

/// Run `bd search <query> --json` against the hub and attribute the results
/// through the **same** [`snapshot::attribute`] path as ready rows, so search rows
/// carry `repo_name` identically. The prefix map is rebuilt from the roster via
/// [`refresh::attribution_map`] (its per-repo prefix-read failures are non-fatal —
/// those ids fall to the `unknown` bucket). A `bd search` failure maps to a
/// [`sanitize`]d message. No version gate / `ensure_hub`: search is reachable only
/// from the list, i.e. after a snapshot already hydrated the hub.
#[cfg(test)]
pub(crate) fn gather_search(
    bd: &impl BdClient,
    roster: &Config,
    paths: &Paths,
    query: &str,
) -> Result<Vec<Row>, String> {
    let hub = hub_dir(paths);
    let issues = bd
        .search(&hub, query)
        .map_err(|e| sanitize(&format!("search failed: {e}")))?;
    let (prefix_map, _errors) = refresh::attribution_map(bd, roster);
    Ok(snapshot::attribute(issues, &prefix_map, SystemTime::now()).rows)
}

#[cfg(test)]
fn gather_search_with_prefixes(
    bd: &impl BdClient,
    roster: &Config,
    paths: &Paths,
    query: &str,
    prefixes: &HashMap<std::path::PathBuf, String>,
) -> Result<Vec<Row>, String> {
    if prefixes.is_empty() {
        return gather_search(bd, roster, paths, query);
    }
    let hub = hub_dir(paths);
    let issues = bd
        .search(&hub, query)
        .map_err(|e| sanitize(&format!("search failed: {e}")))?;
    let prefix_map = cached_prefix_map(roster, prefixes);
    Ok(snapshot::attribute(issues, &prefix_map, SystemTime::now()).rows)
}

fn gather_search_with_state(
    bd: &impl BdClient,
    paths: &Paths,
    query: &str,
    state: &RuntimeRefreshState,
) -> Result<(Vec<Row>, VerifiedAttribution), String> {
    let generation_state = state
        .hub_access
        .read()
        .expect("hub generation state poisoned");
    let verified = generation_state
        .current_hub
        .clone()
        .ok_or_else(|| "search unavailable until a verified refresh completes".to_string())?;
    let hub = hub_dir(paths);
    let _hub_lock = match refresh::HubLock::try_acquire(&hub)
        .map_err(|error| sanitize(&format!("search failed: {error}")))?
    {
        Some(lock) => lock,
        None => {
            return Err(
                "search unavailable while another hank is refreshing; retry shortly".to_string(),
            );
        }
    };
    let marker = refresh::read_hub_generation(paths)
        .map_err(|error| sanitize(&format!("search failed: {error}")))?;
    if marker.as_ref() != Some(&verified.hub_token) {
        return Err("hub changed in another hank process; refresh required".to_string());
    }
    let issues = bd
        .search(&hub, query)
        .map_err(|error| sanitize(&format!("search failed: {error}")))?;
    let rows = snapshot::attribute_with_generation(
        issues,
        &verified.candidate.prefix_map,
        SystemTime::now(),
        verified.generation,
    )
    .rows;
    Ok((rows, verified))
}

/// Spawn a background copy worker that reports over `tx`, returning its join
/// handle so the event loop can wait for it on shutdown. Clones the roster and
/// paths into the thread and builds a fresh [`BdCli`] (stateless).
fn spawn_copy(
    tx: &Sender<Incoming>,
    paths: &Paths,
    row: Row,
    markdown: bool,
    token: u64,
    map: Option<Arc<refresh::PrefixMap>>,
) -> thread::JoinHandle<()> {
    let tx = tx.clone();
    let paths = paths.clone();
    thread::spawn(move || {
        let (payload, summary) = build_copy_with_map(&BdCli::new(), &paths, &row, markdown, map);
        let _ = tx.send(
            Msg::Copied {
                token,
                payload,
                summary,
            }
            .into(),
        );
    })
}

/// The copy worker body: build the clipboard payload + status summary off the UI
/// thread (the id→repo-path resolution runs `bd`), then send exactly one
/// [`Msg::Copied`]. `reduce` turns that into the UI-thread [`Effect::WriteClipboard`]
/// so the escape write never races a draw. Owned args so it moves cleanly into a
/// thread; unit-tested directly with a [`crate::bd::FakeBdClient`] and a channel.
#[cfg(test)]
pub(crate) fn copy_worker(
    bd: impl BdClient,
    roster: Config,
    paths: Paths,
    row: Row,
    markdown: bool,
    token: u64,
    tx: Sender<Incoming>,
) {
    let (payload, summary) = build_copy(&bd, &roster, &paths, &row, markdown);
    let _ = tx.send(
        Msg::Copied {
            token,
            payload,
            summary,
        }
        .into(),
    );
}

/// Build the clipboard payload and its status-bar summary for `row`.
///
/// The command form (`markdown == false`) resolves the row's source-repo path
/// from its issue id via [`refresh::attribution_map`] — the **same** prefix map
/// search uses — and falls back to the hub (`bd -C <hub> show <id>`) for an
/// unattributed id. The Markdown form refreshes the issue with one structured
/// `bd show --json` call when copied, falling back to the pinned row if that
/// refresh fails. This keeps detail navigation to one native `bd show` while
/// avoiding stale copied metadata. All bd-sourced text is sanitized inside
/// [`context`].
#[cfg(test)]
fn build_copy(
    bd: &impl BdClient,
    roster: &Config,
    paths: &Paths,
    row: &Row,
    markdown: bool,
) -> (String, String) {
    let payload = if markdown {
        let issue = bd
            .show_issue(&hub_dir(paths), &row.issue.id)
            .unwrap_or_else(|_| row.issue.clone());
        context::markdown_block(&issue, &row.repo_name)
    } else {
        let (prefix_map, _errors) = refresh::attribution_map(bd, roster);
        let repo = prefix_map.repo_for(&row.issue.id).map(|e| e.path.clone());
        context::shell_command(repo.as_deref(), &hub_dir(paths), &row.issue.id)
    };
    let summary = context::summarize(&payload, COPY_SUMMARY_MAX);
    (payload, summary)
}

#[cfg(test)]
fn build_copy_with_prefixes(
    bd: &impl BdClient,
    roster: &Config,
    paths: &Paths,
    row: &Row,
    markdown: bool,
    prefixes: &HashMap<std::path::PathBuf, String>,
) -> (String, String) {
    if markdown || prefixes.is_empty() {
        return build_copy(bd, roster, paths, row, markdown);
    }
    let prefix_map = cached_prefix_map(roster, prefixes);
    let repo = prefix_map
        .repo_for(&row.issue.id)
        .map(|entry| entry.path.clone());
    let payload = context::shell_command(repo.as_deref(), &hub_dir(paths), &row.issue.id);
    let summary = context::summarize(&payload, COPY_SUMMARY_MAX);
    (payload, summary)
}

fn build_copy_with_map(
    bd: &impl BdClient,
    paths: &Paths,
    row: &Row,
    markdown: bool,
    map: Option<Arc<refresh::PrefixMap>>,
) -> (String, String) {
    let payload = if markdown {
        let issue = bd
            .show_issue(&hub_dir(paths), &row.issue.id)
            .unwrap_or_else(|_| row.issue.clone());
        context::markdown_block(&issue, &row.repo_name)
    } else {
        let repo = map
            .as_deref()
            .and_then(|prefix_map| prefix_map.repo_for(&row.issue.id))
            .and_then(|entry| entry.path.is_dir().then(|| entry.path.clone()));
        context::shell_command(repo.as_deref(), &hub_dir(paths), &row.issue.id)
    };
    let summary = context::summarize(&payload, COPY_SUMMARY_MAX);
    (payload, summary)
}

#[cfg(test)]
fn cached_prefix_map(
    roster: &Config,
    prefixes: &HashMap<std::path::PathBuf, String>,
) -> refresh::PrefixMap {
    let mut seen = HashSet::new();
    let pairs = roster
        .repos
        .iter()
        .filter_map(|entry| {
            let normalized_path = refresh::normalize_path(&entry.path);
            if !seen.insert(normalized_path.clone()) {
                return None;
            }
            prefixes
                .get(&normalized_path)
                .map(|prefix| (prefix.clone(), entry.clone()))
        })
        .collect();
    refresh::PrefixMap::from_pairs(pairs)
}

/// Write `payload` to the terminal clipboard via an OSC 52 escape. Called only on
/// the UI thread (which owns the tty), so the sequence can never interleave with
/// a ratatui draw. Best-effort: a terminal that ignores OSC 52 simply drops it,
/// and a write failure is non-fatal (the status bar still confirms the attempt).
fn write_clipboard(payload: &str) {
    let seq = context::osc52(payload);
    let mut out = io::stdout();
    let _ = out.write_all(seq.as_bytes());
    let _ = out.flush();
}

/// Run `ensure_hub → refresh → fetch` and return the fresh snapshot (or `None`
/// on any fatal failure, keeping the caller's last-good rows) plus the warnings
/// to surface. Deliberately fatal-tolerant, unlike the fail-fast CLI
/// [`crate::cli::run_snapshot`]: the TUI degrades and stays interactive. All
/// warnings are [`sanitize`]d (they embed bd stderr / paths and reach a
/// terminal).
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn gather_snapshot(
    bd: &impl BdClient,
    roster: &Config,
    paths: &Paths,
) -> (Option<Snapshot>, Vec<String>) {
    gather_snapshot_with_state(bd, roster, paths, &RuntimeRefreshState::default())
}

fn gather_snapshot_with_state(
    bd: &impl BdClient,
    roster: &Config,
    paths: &Paths,
    state: &RuntimeRefreshState,
) -> (Option<Snapshot>, Vec<String>) {
    gather_snapshot_measured(bd, roster, paths, state).0
}

pub(crate) fn gather_snapshot_measured(
    bd: &impl BdClient,
    roster: &Config,
    paths: &Paths,
    state: &RuntimeRefreshState,
) -> ((Option<Snapshot>, Vec<String>), PipelineMetrics) {
    let mut metrics = PipelineMetrics::default();
    let result = gather_snapshot_with_metrics(bd, roster, paths, state, &mut metrics);
    (result, metrics)
}

fn gather_snapshot_with_metrics(
    bd: &impl BdClient,
    roster: &Config,
    paths: &Paths,
    state: &RuntimeRefreshState,
    metrics: &mut PipelineMetrics,
) -> (Option<Snapshot>, Vec<String>) {
    let total_started = std::time::Instant::now();
    let mut warnings = Vec::new();

    // Version gate: a bd whose schema hank cannot vouch for yields no snapshot.
    let version_was_uncached = state.compatibility.get().is_none();
    let version_started = std::time::Instant::now();
    let compatibility = state.compatibility.get_or_init(|| match bd.version() {
        Ok(version) => version_gate(&version).map_err(|message| sanitize(&message)),
        Err(error) => Err(sanitize(&format!("bd version check failed: {error}"))),
    });
    metrics.version = version_started.elapsed();
    metrics.calls.version = usize::from(version_was_uncached);
    if let Err(message) = compatibility {
        warnings.push(message.clone());
        metrics.total = total_started.elapsed();
        return (None, warnings);
    }

    let reconcile_started = std::time::Instant::now();
    match ensure_reconciled(bd, paths, roster, state) {
        Ok(reconcile_warnings) => {
            warnings.extend(reconcile_warnings.iter().map(|warning| sanitize(warning)))
        }
        Err(e) => {
            warnings.push(sanitize(&format!("hub error: {e}")));
            metrics.reconcile = reconcile_started.elapsed();
            metrics.total = total_started.elapsed();
            return (None, warnings);
        }
    }
    metrics.reconcile = reconcile_started.elapsed();

    let hub = hub_dir(paths);
    // Bind sync, marker publication, generation registration, and ready to one
    // in-process write generation. Search takes the read side.
    let mut generation_state = state
        .hub_access
        .write()
        .expect("hub generation state poisoned");
    let previous = generation_state
        .current_hub
        .as_ref()
        .map(|verified| verified.candidate.clone());
    let synced = match refresh::run_with_state(bd, roster, paths, previous.as_ref(), 4) {
        Ok(synced) => {
            for repo_error in &synced.outcome().errors {
                warnings.push(sanitize(&repo_error.to_string()));
            }
            for collision in synced.outcome().prefix_map.collisions() {
                warnings.push(sanitize(&format!(
                    "id prefix `{}` is claimed by {} repos; its issues show as `{}`",
                    collision.prefix,
                    collision.repos.len(),
                    snapshot::UNKNOWN_REPO,
                )));
            }
            synced
        }
        // Another hank holds the lock: keep the current view intact rather than
        // fetching a snapshot with no prefix map (which would re-attribute every
        // row to `unknown`, reset the age, and empty an active repo filter).
        // Returning `None` makes `reduce` retain the last-good rows.
        Err(RefreshError::AlreadyRefreshing) => {
            warnings.push("another hank is refreshing this hub; keeping the current view".into());
            metrics.total = total_started.elapsed();
            return (None, warnings);
        }
        Err(fatal) => {
            warnings.push(sanitize(&format!("refresh failed: {fatal}")));
            metrics.total = total_started.elapsed();
            return (None, warnings);
        }
    };
    metrics.source_wall = synced.outcome().metrics.source_wall;
    metrics.sync = synced.outcome().metrics.sync;
    metrics.calls.export = synced.outcome().metrics.export_calls;
    metrics.calls.issue_prefix = synced.outcome().metrics.issue_prefix_calls;
    metrics.calls.repo_sync = 1;
    metrics.sync_report = Some(synced.outcome().sync_report.clone());

    let hub_token = HubGenerationToken::fresh();
    if let Err(error) = refresh::publish_hub_generation(paths, &hub_token) {
        generation_state.current_hub = None;
        warnings.push(sanitize(&format!("refresh failed: {error}")));
        metrics.total = total_started.elapsed();
        return (None, warnings);
    }
    generation_state.next_generation += 1;
    let generation = AttributionGeneration::new(generation_state.next_generation);
    let verified = VerifiedAttribution {
        generation,
        hub_token,
        candidate: synced.candidate().clone(),
    };
    state.register_map(generation, Arc::clone(&verified.candidate.prefix_map));
    generation_state.current_hub = Some(verified);

    let ready_started = std::time::Instant::now();
    metrics.calls.ready = 1;
    let result = match bd.ready(&hub) {
        Ok(issues) => (
            Some(snapshot::attribute_with_generation(
                issues,
                &synced.outcome().prefix_map,
                synced.outcome().synced_at,
                generation,
            )),
            warnings,
        ),
        Err(e) => {
            warnings.push(sanitize(&format!("reading ready list failed: {e}")));
            (None, warnings)
        }
    };
    metrics.ready = ready_started.elapsed();
    metrics.total = total_started.elapsed();
    result
}

fn ensure_reconciled(
    bd: &impl BdClient,
    paths: &Paths,
    roster: &Config,
    state: &RuntimeRefreshState,
) -> Result<Vec<String>, crate::hub::HubError> {
    let before = reconcile_witness(paths, roster)?;
    let can_reuse = state
        .reconciled
        .lock()
        .expect("reconcile state poisoned")
        .as_ref()
        .is_some_and(|cached| cached == &before && before.is_satisfied());
    if can_reuse {
        return Ok(before.warnings());
    }

    let status = ensure_hub(bd, paths, roster)?;
    let after = reconcile_witness(paths, roster)?;
    *state.reconciled.lock().expect("reconcile state poisoned") = Some(after);
    Ok(status.warnings)
}

/// The crossterm event producer: forward **raw** key presses until told to stop
/// (the UI thread decodes them against live app state). Polls with a timeout so
/// the stop flag is observed even while idle. On a terminal read/poll failure it
/// sends `Quit` before exiting, so the UI thread — which holds its own sender and
/// would otherwise block on `recv` forever with no producer left — always has a
/// path to a clean shutdown.
fn input_thread(tx: &Sender<Incoming>, stop: &AtomicBool) {
    while !stop.load(Ordering::SeqCst) {
        match event::poll(INPUT_POLL) {
            Ok(true) => match event::read() {
                Ok(Event::Key(key)) => {
                    if tx.send(Incoming::Key(key)).is_err() {
                        return; // UI thread gone.
                    }
                }
                Ok(_) => {} // non-key event (resize, mouse): ignored
                Err(_) => {
                    let _ = tx.send(Msg::Quit.into()); // can't read input: quit cleanly
                    return;
                }
            },
            Ok(false) => {} // timeout: loop and re-check the stop flag
            Err(_) => {
                let _ = tx.send(Msg::Quit.into());
                return;
            }
        }
    }
}

/// Enter raw mode + the alternate screen and install the restoring panic hook.
///
/// Rolls back each step if a later one fails, so a partial setup never returns
/// `Err` while leaving the terminal in raw mode or the alternate screen (the
/// caller has no `Tui` to restore in that case).
fn setup_terminal() -> io::Result<Tui> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    if let Err(e) = execute!(stdout, EnterAlternateScreen) {
        let _ = disable_raw_mode();
        return Err(e);
    }
    set_panic_hook();
    match Terminal::new(CrosstermBackend::new(stdout)) {
        Ok(terminal) => Ok(terminal),
        Err(e) => {
            let _ = execute!(io::stdout(), LeaveAlternateScreen);
            let _ = disable_raw_mode();
            Err(e)
        }
    }
}

/// Leave the alternate screen, disable raw mode, and show the cursor.
///
/// Best-effort: every step is attempted even if an earlier one fails (cleanup
/// matters most precisely when a terminal op is failing), and the first error is
/// returned once all three have run.
fn restore_terminal(terminal: &mut Tui) -> io::Result<()> {
    let raw = disable_raw_mode();
    let screen = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    let cursor = terminal.show_cursor();
    raw.and(screen).and(cursor)
}

/// Chain a terminal-restoring step before the default panic hook, so a panic
/// mid-render leaves the user with a usable terminal instead of a wedged one.
fn set_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        original(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bd::{BdError, BdErrorKind, FakeBdClient, Issue};
    use crate::config::RepoEntry;
    use crate::refresh::HubLock;
    use std::fs;
    use std::path::{Path, PathBuf};

    /// Receive the next app message a worker sent, unwrapping the [`Incoming`]
    /// channel envelope (workers only ever send `Incoming::Msg`).
    fn recv_msg(rx: &Receiver<Incoming>) -> Msg {
        match rx.recv().expect("a worker message") {
            Incoming::Msg(msg) => msg,
            Incoming::AttributedMsg { msg, .. } => msg,
            Incoming::Key(key) => panic!("workers never send keys, got {key:?}"),
        }
    }

    /// macOS can briefly retain an advisory flock after the refresh owner has
    /// returned. Production correctly asks the user to retry; tests that need
    /// the post-refresh state wait a bounded interval and still fail on a real
    /// lock leak.
    fn search_after_completed_refresh(
        bd: &impl BdClient,
        paths: &Paths,
        query: &str,
        state: &RuntimeRefreshState,
    ) -> Result<(Vec<Row>, VerifiedAttribution), String> {
        for _ in 0..100 {
            match gather_search_with_state(bd, paths, query, state) {
                Err(error) if error.contains("another hank is refreshing") => {
                    thread::sleep(Duration::from_millis(1));
                }
                result => return result,
            }
        }
        Err("completed refresh lock remained held for 100 ms".to_string())
    }

    fn issue(id: &str, priority: i64, title: &str) -> Issue {
        Issue {
            id: id.to_string(),
            title: title.to_string(),
            status: "open".into(),
            priority,
            description: None,
            issue_type: None,
            owner: None,
            labels: Vec::new(),
            created_at: None,
            created_by: None,
            updated_at: Some("2026-07-11T00:00:00Z".into()),
            dependency_count: None,
            dependent_count: None,
            comment_count: None,
        }
    }

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

    fn seed_initialized_hub(paths: &Paths, additional: &[&Path]) {
        let beads = hub_dir(paths).join(".beads");
        fs::create_dir_all(beads.join("embeddeddolt")).unwrap();
        let mut yaml = String::from("repos:\n  primary: \".\"\n  additional:\n");
        for repo in additional {
            yaml.push_str(&format!("    - \"{}\"\n", repo.display()));
        }
        fs::write(beads.join("config.yaml"), yaml).unwrap();
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
            command: "bd repo sync".into(),
            stderr: "boom".into(),
            kind: BdErrorKind::NonZeroExit { code: Some(1) },
        }
    }

    #[test]
    fn startup_restores_repo_view_before_cache_hydration() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let roster = Config::default();
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        ui_state::save(
            paths.ui_state_file(),
            &crate::app::RepoFilter::Only("rb".into()),
        )
        .unwrap();
        cache::save(
            paths.cache_file(),
            &Snapshot {
                rows: vec![
                    Row {
                        issue: issue("a-1", 1, "A"),
                        repo_id: Some("ra".into()),
                        repo_name: "repo-a".into(),
                        attribution_generation: None,
                    },
                    Row {
                        issue: issue("b-1", 1, "B"),
                        repo_id: Some("rb".into()),
                        repo_name: "repo-b".into(),
                        attribution_generation: None,
                    },
                ],
                fetched_at: now - Duration::from_secs(60),
            },
            &roster,
        )
        .unwrap();

        let app = initial_app(&paths, &roster, now);

        assert_eq!(app.repo_view(), &crate::app::RepoFilter::Only("rb".into()));
        assert_eq!(
            app.filtered_rows()
                .iter()
                .map(|row| row.issue.id.as_str())
                .collect::<Vec<_>>(),
            vec!["b-1"]
        );
        assert!(app.is_stale());
    }

    #[test]
    fn persist_repo_view_effect_writes_state_and_reports_completion() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let (tx, rx) = mpsc::channel();
        let mut handles = Vec::new();
        let repo = crate::app::RepoFilter::Only("repo-b".into());
        fs::create_dir_all(paths.data_dir()).unwrap();
        fs::write(paths.cache_file(), b"cache sentinel").unwrap();

        execute_effect(
            Effect::PersistRepoView(repo.clone()),
            &tx,
            &mut handles,
            &paths,
            &Config::default(),
            &Arc::new(RuntimeRefreshState::default()),
        );

        assert!(
            handles.is_empty(),
            "the atomic preference write is synchronous"
        );
        assert_eq!(
            recv_msg(&rx),
            Msg::RepoViewPersisted {
                repo: repo.clone(),
                result: Ok(())
            }
        );
        assert_eq!(ui_state::load(paths.ui_state_file()), repo);
        assert_eq!(
            fs::read(paths.cache_file()).unwrap(),
            b"cache sentinel",
            "repository preference writes do not rewrite the snapshot cache"
        );
    }

    #[test]
    fn persist_repo_view_failure_is_reported_without_aborting() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("hank"), "not a directory").unwrap();
        let paths = Paths::with_base(tmp.path());
        let (tx, rx) = mpsc::channel();
        let mut handles = Vec::new();
        let repo = crate::app::RepoFilter::Only("repo-b".into());

        execute_effect(
            Effect::PersistRepoView(repo.clone()),
            &tx,
            &mut handles,
            &paths,
            &Config::default(),
            &Arc::new(RuntimeRefreshState::default()),
        );

        match recv_msg(&rx) {
            Msg::RepoViewPersisted {
                repo: attempted,
                result: Err(message),
            } => {
                assert_eq!(attempted, repo);
                assert!(message.contains("couldn't save repository view"));
                assert!(!message.chars().any(char::is_control));
            }
            other => panic!("expected non-fatal persistence result, got {other:?}"),
        }
    }

    #[test]
    fn refresh_task_sends_started_then_completed() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let ra = seed_repo(tmp.path(), "ra", "ra");
        let bd = FakeBdClient::new().with_ready(vec![issue("ra-1", 1, "Ready one")]);
        let (tx, rx) = mpsc::channel();

        let handle = thread::spawn(move || refresh_worker(bd, roster(&[&ra]), paths, tx));

        // Exactly: RefreshStarted, then one RefreshCompleted carrying the rows.
        assert_eq!(recv_msg(&rx), Msg::RefreshStarted);
        match recv_msg(&rx) {
            Msg::RefreshCompleted { snapshot, .. } => {
                let snap = snapshot.expect("a snapshot on success");
                assert!(
                    snap.rows.iter().any(|r| r.issue.id == "ra-1"),
                    "the ready row flows through: {:?}",
                    snap.rows
                );
            }
            other => panic!("expected RefreshCompleted, got {other:?}"),
        }
        // The worker's `tx` drops when it returns, closing the channel: no third
        // message, so the two-message lifecycle is exact (no sleeps needed).
        assert!(
            rx.recv().is_err(),
            "exactly one completion, then the channel closes"
        );
        handle.join().unwrap();
    }

    #[test]
    fn refresh_task_caches_a_successful_snapshot_to_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let cache_file = paths.cache_file().to_path_buf();
        let ra = seed_repo(tmp.path(), "ra", "ra");
        let bd = FakeBdClient::new().with_ready(vec![issue("ra-1", 1, "Ready one")]);
        let cfg = roster(&[&ra]);
        let (tx, rx) = mpsc::channel();

        let handle = thread::spawn(move || refresh_worker(bd, cfg, paths, tx));
        assert_eq!(recv_msg(&rx), Msg::RefreshStarted);
        let snapshot = match recv_msg(&rx) {
            Msg::RefreshCompleted { snapshot, .. } => snapshot.expect("a snapshot on success"),
            other => panic!("expected RefreshCompleted, got {other:?}"),
        };
        handle.join().unwrap();

        let cached = crate::cache::load(&cache_file, SystemTime::now(), &roster(&[&ra]))
            .expect("a fresh cache hit");
        assert!(
            snapshot
                .rows
                .iter()
                .all(|row| row.attribution_generation.is_some()),
            "live rows retain in-process provenance"
        );
        assert!(
            cached
                .rows
                .iter()
                .all(|row| row.attribution_generation.is_none()),
            "disk cache never claims an in-process attribution generation"
        );
        let mut serializable_snapshot = snapshot;
        for row in &mut serializable_snapshot.rows {
            row.attribution_generation = None;
        }
        assert_eq!(
            cached, serializable_snapshot,
            "the cache preserves every serialized snapshot field"
        );
    }

    #[test]
    fn gather_snapshot_collects_repo_warnings() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let ra = seed_repo(tmp.path(), "ra", "ra");
        let missing = tmp.path().join("gone");
        let bd = FakeBdClient::new().with_ready(vec![issue("ra-1", 1, "t")]);

        let (snapshot, warnings) = gather_snapshot(&bd, &roster(&[&ra, &missing]), &paths);

        let snap = snapshot.expect("healthy repo still yields a snapshot");
        assert!(
            snap.rows.iter().any(|r| r.issue.id == "ra-1"),
            "the healthy repo's rows appear"
        );
        assert!(
            warnings.iter().any(|w| w.contains("gone")),
            "the missing roster path is warned about: {warnings:?}"
        );
    }

    #[test]
    fn gather_snapshot_none_on_fatal_sync() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let ra = seed_repo(tmp.path(), "ra", "ra");
        let bd = FakeBdClient::new().with_repo_sync_err(bd_err());

        let (snapshot, warnings) = gather_snapshot(&bd, &roster(&[&ra]), &paths);

        assert!(
            snapshot.is_none(),
            "a fatal sync failure yields no snapshot"
        );
        assert!(
            warnings.iter().any(|w| w.contains("refresh failed")),
            "the fatal refresh is surfaced: {warnings:?}"
        );
    }

    fn detail() -> String {
        "○ ra-1 [TASK] · Blocked task\n\nDEPENDENCIES\n  → ra-z70: Blocker task".into()
    }

    #[test]
    fn detail_worker_sends_ready_for_id() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let bd = FakeBdClient::new().with_show(detail());
        let (tx, rx) = mpsc::channel();

        let handle = thread::spawn(move || detail_worker(bd, paths, "ra-1".into(), 7, tx));

        match recv_msg(&rx) {
            Msg::DetailReady { token, detail } => {
                assert_eq!(token, 7, "the request token is echoed back");
                let d = detail.expect("a detail on success");
                assert!(d.contains("ra-z70"));
            }
            other => panic!("expected DetailReady, got {other:?}"),
        }
        // The worker's tx drops on return: exactly one message, then closed.
        assert!(rx.recv().is_err(), "exactly one DetailReady, then closed");
        handle.join().unwrap();
    }

    #[test]
    fn detail_worker_maps_error() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let bd = FakeBdClient::new().with_show_err(bd_err());
        let (tx, rx) = mpsc::channel();

        let handle = thread::spawn(move || detail_worker(bd, paths, "ra-1".into(), 1, tx));

        match recv_msg(&rx) {
            Msg::DetailReady { token, detail } => {
                assert_eq!(token, 1);
                let msg = detail.expect_err("a message on failure");
                assert!(
                    msg.contains("boom") || msg.to_lowercase().contains("fail"),
                    "the failure is surfaced: {msg}"
                );
            }
            other => panic!("expected DetailReady, got {other:?}"),
        }
        handle.join().unwrap();
    }

    #[test]
    fn search_worker_sends_results_for_token() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let ra = seed_repo(tmp.path(), "ra", "ra");
        let bd = FakeBdClient::new().with_search(vec![issue("ra-1", 1, "Found one")]);
        let (tx, rx) = mpsc::channel();

        let handle =
            thread::spawn(move || search_worker(bd, roster(&[&ra]), paths, "foo".into(), 7, tx));

        match recv_msg(&rx) {
            Msg::SearchResults { token, rows } => {
                assert_eq!(token, 7, "the request token is echoed back");
                let rows = rows.expect("results on success");
                let found = rows
                    .iter()
                    .find(|r| r.issue.id == "ra-1")
                    .expect("row present");
                assert_eq!(
                    found.repo_name, "ra",
                    "results are attributed via the roster prefix map"
                );
            }
            other => panic!("expected SearchResults, got {other:?}"),
        }
        // The worker's tx drops on return: exactly one message, then closed.
        assert!(rx.recv().is_err(), "exactly one SearchResults, then closed");
        handle.join().unwrap();
    }

    #[test]
    fn search_worker_maps_error() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let ra = seed_repo(tmp.path(), "ra", "ra");
        let bd = FakeBdClient::new().with_search_err(bd_err());
        let (tx, rx) = mpsc::channel();

        let handle =
            thread::spawn(move || search_worker(bd, roster(&[&ra]), paths, "foo".into(), 3, tx));

        match recv_msg(&rx) {
            Msg::SearchResults { token, rows } => {
                assert_eq!(token, 3);
                let msg = rows.expect_err("a message on failure");
                assert!(
                    msg.to_lowercase().contains("search failed") || msg.contains("boom"),
                    "the failure is surfaced: {msg}"
                );
            }
            other => panic!("expected SearchResults, got {other:?}"),
        }
        handle.join().unwrap();
    }

    /// The single `Msg::Copied` a copy worker sends: (token, payload, summary).
    fn recv_copied(rx: &Receiver<Incoming>) -> (u64, String, String) {
        match recv_msg(rx) {
            Msg::Copied {
                token,
                payload,
                summary,
            } => (token, payload, summary),
            other => panic!("expected Copied, got {other:?}"),
        }
    }

    fn copy_row(repo_name: &str, id: &str) -> Row {
        Row {
            issue: issue(id, 1, "Ready one"),
            repo_id: None,
            repo_name: repo_name.to_string(),
            attribution_generation: None,
        }
    }

    #[test]
    fn tui_session_reuses_version_and_verified_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let ra = seed_repo(tmp.path(), "ra", "ra");
        let bd = FakeBdClient::new()
            .with_ready(vec![issue("ra-1", 1, "ready")])
            .with_export_content(&ra, b"{\"id\":\"ra-1\"}\n".to_vec());
        let state = RuntimeRefreshState::default();

        assert!(
            gather_snapshot_with_state(&bd, &roster(&[&ra]), &paths, &state)
                .0
                .is_some()
        );
        assert!(
            gather_snapshot_with_state(&bd, &roster(&[&ra]), &paths, &state)
                .0
                .is_some()
        );
        let calls = bd.calls();
        assert_eq!(
            calls
                .iter()
                .filter(|call| matches!(call, crate::bd::Call::Version))
                .count(),
            1
        );
        assert_eq!(
            calls
                .iter()
                .filter(|call| matches!(call, crate::bd::Call::IssuePrefix(path) if path == &ra))
                .count(),
            1,
            "the warm refresh revalidates the cached prefix from its fresh export"
        );
    }

    #[test]
    fn missing_repo_becoming_present_invalidates_reconcile_witness() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        seed_initialized_hub(&paths, &[]);
        let repo = tmp.path().join("later");
        let config = roster(&[&repo]);
        let state = RuntimeRefreshState::default();
        let bd = FakeBdClient::new();

        let first = ensure_reconciled(&bd, &paths, &config, &state).unwrap();
        let second = ensure_reconciled(&bd, &paths, &config, &state).unwrap();
        assert_eq!(first, second);
        assert!(first[0].contains("does not exist"));
        assert!(
            bd.calls().is_empty(),
            "stable missing state skips reconcile"
        );

        fs::create_dir_all(&repo).unwrap();
        ensure_reconciled(&bd, &paths, &config, &state).unwrap();
        let canonical_repo = fs::canonicalize(&repo).unwrap();

        assert!(
            bd.calls()
                .iter()
                .any(|call| matches!(call, crate::bd::Call::RepoAdd(_, actual) if actual == &canonical_repo)),
            "missing→present reachability invalidates the retained witness"
        );
    }

    #[test]
    fn external_hub_reset_and_roster_drift_invalidate_reconcile_witness() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let repo = seed_repo(tmp.path(), "repo", "ra");
        let config = roster(&[&repo]);
        seed_initialized_hub(&paths, &[&repo]);
        let state = RuntimeRefreshState::default();
        let bd = FakeBdClient::new();
        ensure_reconciled(&bd, &paths, &config, &state).unwrap();
        assert!(bd.calls().is_empty());

        seed_initialized_hub(&paths, &[]);
        ensure_reconciled(&bd, &paths, &config, &state).unwrap();
        let canonical_repo = fs::canonicalize(&repo).unwrap();
        assert!(bd.calls().iter().any(
            |call| matches!(call, crate::bd::Call::RepoAdd(_, actual) if actual == &canonical_repo)
        ));

        fs::remove_dir_all(hub_dir(&paths)).unwrap();
        ensure_reconciled(&bd, &paths, &config, &state).unwrap();
        assert!(
            bd.calls()
                .iter()
                .any(|call| matches!(call, crate::bd::Call::Init(..)))
        );
    }

    #[test]
    fn tui_session_caches_incompatible_version_result() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let ra = seed_repo(tmp.path(), "ra", "ra");
        let config = roster(&[&ra]);
        let state = RuntimeRefreshState::default();
        let failing = FakeBdClient::new().with_version_err(bd_err());

        assert!(
            gather_snapshot_with_state(&failing, &config, &paths, &state)
                .0
                .is_none()
        );

        let recovered = FakeBdClient::new()
            .with_ready(vec![issue("ra-1", 1, "ready")])
            .with_export_content(&ra, b"{\"id\":\"ra-1\"}\n".to_vec());
        assert!(
            gather_snapshot_with_state(&recovered, &config, &paths, &state)
                .0
                .is_none(),
            "compatibility is a session invariant; recovery requires a restart"
        );
        assert!(
            recovered
                .calls()
                .iter()
                .all(|call| !matches!(call, crate::bd::Call::Version)),
            "the cached failure avoids another version subprocess"
        );
    }

    #[test]
    fn tui_session_keeps_authoritative_prefixes_for_search_attribution() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let ra = seed_repo(tmp.path(), "ra", "ra");
        let rb = seed_repo(tmp.path(), "rb", "rb");
        let config = roster(&[&ra, &rb]);
        let bd = FakeBdClient::new()
            .with_ready(vec![issue("rb-1", 1, "ready")])
            .with_search(vec![issue("rb-1", 1, "found")])
            .with_export_content(&ra, b"{\"id\":\"ra-1\"}\n".to_vec())
            .with_export_content(&rb, Vec::new());
        let state = RuntimeRefreshState::default();

        assert!(
            gather_snapshot_with_state(&bd, &config, &paths, &state)
                .0
                .is_some()
        );
        let rows = search_after_completed_refresh(&bd, &paths, "found", &state)
            .expect("search succeeds")
            .0;

        assert_eq!(
            rows[0].repo_name, "rb",
            "an authoritative prefix remains available even when its export was not reusable"
        );
    }

    #[test]
    fn search_after_refresh_uses_current_generation_without_prefix_reads() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let repo = seed_repo(tmp.path(), "repo", "ra");
        let config = roster(&[&repo]);
        let bd = FakeBdClient::new()
            .with_ready(vec![issue("ra-1", 1, "ready")])
            .with_search(vec![issue("ra-1", 1, "found")])
            .with_export_content(&repo, b"{\"id\":\"ra-1\"}\n".to_vec());
        let state = RuntimeRefreshState::default();
        let snapshot = gather_snapshot_with_state(&bd, &config, &paths, &state)
            .0
            .expect("refresh");
        let generation = snapshot.rows[0]
            .attribution_generation
            .expect("fresh rows are stamped");
        let prefix_calls_before = bd
            .calls()
            .iter()
            .filter(|call| matches!(call, crate::bd::Call::IssuePrefix(_)))
            .count();

        let rows = search_after_completed_refresh(&bd, &paths, "found", &state)
            .unwrap()
            .0;

        assert_eq!(rows[0].attribution_generation, Some(generation));
        assert_eq!(
            bd.calls()
                .iter()
                .filter(|call| matches!(call, crate::bd::Call::IssuePrefix(_)))
                .count(),
            prefix_calls_before
        );
    }

    #[test]
    fn old_ready_row_copied_after_refresh_uses_old_generation() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let repo = seed_repo(tmp.path(), "repo", "old");
        let config = roster(&[&repo]);
        let state = RuntimeRefreshState::default();
        let first_bd = FakeBdClient::new()
            .with_issue_prefix(repo.clone(), "old")
            .with_ready(vec![issue("old-1", 1, "old")])
            .with_export_content(&repo, b"{\"id\":\"old-1\"}\n".to_vec());
        let old_row = gather_snapshot_with_state(&first_bd, &config, &paths, &state)
            .0
            .unwrap()
            .rows
            .remove(0);

        let second_bd = FakeBdClient::new()
            .with_issue_prefix(repo.clone(), "new")
            .with_ready(vec![issue("new-1", 1, "new")])
            .with_export_content(&repo, b"{\"id\":\"new-1\"}\n".to_vec());
        assert!(
            gather_snapshot_with_state(&second_bd, &config, &paths, &state)
                .0
                .is_some()
        );

        let old_map = state
            .map_for(old_row.attribution_generation.unwrap())
            .expect("old visible generation retained");
        let (payload, _) =
            build_copy_with_map(&FakeBdClient::new(), &paths, &old_row, false, Some(old_map));

        assert_eq!(payload, format!("cd {} && bd show old-1", repo.display()));
    }

    #[test]
    fn copied_row_falls_back_to_hub_after_source_disappears() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let repo = seed_repo(tmp.path(), "repo", "old");
        let config = roster(&[&repo]);
        let state = RuntimeRefreshState::default();
        let bd = FakeBdClient::new()
            .with_issue_prefix(repo.clone(), "old")
            .with_ready(vec![issue("old-1", 1, "old")])
            .with_export_content(&repo, b"{\"id\":\"old-1\"}\n".to_vec());
        let row = gather_snapshot_with_state(&bd, &config, &paths, &state)
            .0
            .unwrap()
            .rows
            .remove(0);
        let map = state
            .map_for(row.attribution_generation.unwrap())
            .expect("visible generation retained");
        fs::remove_dir_all(&repo).unwrap();

        let (payload, _) =
            build_copy_with_map(&FakeBdClient::new(), &paths, &row, false, Some(map));

        assert_eq!(
            payload,
            format!("bd -C {} show old-1", hub_dir(&paths).display())
        );
    }

    #[test]
    fn search_rejects_mismatched_external_hub_token() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let repo = seed_repo(tmp.path(), "repo", "ra");
        let config = roster(&[&repo]);
        let bd = FakeBdClient::new()
            .with_ready(vec![issue("ra-1", 1, "ready")])
            .with_search(vec![issue("ra-1", 1, "found")])
            .with_export_content(&repo, b"{\"id\":\"ra-1\"}\n".to_vec());
        let state = RuntimeRefreshState::default();
        assert!(
            gather_snapshot_with_state(&bd, &config, &paths, &state)
                .0
                .is_some()
        );
        fs::write(hub_dir(&paths).join(".hank-generation"), "external").unwrap();

        let error = search_after_completed_refresh(&bd, &paths, "found", &state)
            .expect_err("a stale local map must fail closed");

        assert!(error.contains("refresh required"), "{error}");
        assert!(
            bd.calls()
                .iter()
                .all(|call| !matches!(call, crate::bd::Call::Search(..))),
            "the marker is verified before reading the changed hub"
        );
    }

    #[test]
    fn search_rejects_hub_advanced_by_stateless_refresh() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let repo = seed_repo(tmp.path(), "repo", "ra");
        let config = roster(&[&repo]);
        let bd = FakeBdClient::new()
            .with_ready(vec![issue("ra-1", 1, "ready")])
            .with_search(vec![issue("ra-1", 1, "found")])
            .with_export_content(&repo, b"{\"id\":\"ra-1\"}\n".to_vec());
        let state = RuntimeRefreshState::default();
        assert!(
            gather_snapshot_with_state(&bd, &config, &paths, &state)
                .0
                .is_some()
        );

        refresh::run(&bd, &config, &paths).expect("another hank refresh");
        let error = search_after_completed_refresh(&bd, &paths, "found", &state)
            .expect_err("the stateless refresh must advance the marker");

        assert!(error.contains("refresh required"), "{error}");
    }

    #[test]
    fn ready_failure_advances_hub_generation_but_preserves_old_map() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let repo = seed_repo(tmp.path(), "repo", "ra");
        let config = roster(&[&repo]);
        let state = RuntimeRefreshState::default();
        let first_bd = FakeBdClient::new()
            .with_ready(vec![issue("ra-1", 1, "ready")])
            .with_export_content(&repo, b"{\"id\":\"ra-1\"}\n".to_vec());
        let old_snapshot = gather_snapshot_with_state(&first_bd, &config, &paths, &state)
            .0
            .unwrap();
        let old_generation = old_snapshot.rows[0].attribution_generation.unwrap();

        let second_bd = FakeBdClient::new()
            .with_ready_err(bd_err())
            .with_search(vec![issue("ra-1", 1, "found")])
            .with_export_content(&repo, b"{\"id\":\"ra-1\"}\n".to_vec());
        assert!(
            gather_snapshot_with_state(&second_bd, &config, &paths, &state)
                .0
                .is_none()
        );
        let current_generation = state
            .hub_access
            .read()
            .unwrap()
            .current_hub
            .as_ref()
            .unwrap()
            .generation;

        assert_ne!(current_generation, old_generation);
        assert!(state.map_for(old_generation).is_some());
        let rows = search_after_completed_refresh(&second_bd, &paths, "found", &state)
            .unwrap()
            .0;
        assert_eq!(rows[0].attribution_generation, Some(current_generation));
    }

    #[test]
    fn marker_publish_failure_clears_current_hub_and_keeps_old_map() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let repo = seed_repo(tmp.path(), "repo", "ra");
        let config = roster(&[&repo]);
        let state = RuntimeRefreshState::default();
        let first_bd = FakeBdClient::new()
            .with_ready(vec![issue("ra-1", 1, "ready")])
            .with_export_content(&repo, b"{\"id\":\"ra-1\"}\n".to_vec());
        let old_generation = gather_snapshot_with_state(&first_bd, &config, &paths, &state)
            .0
            .unwrap()
            .rows[0]
            .attribution_generation
            .unwrap();
        let marker = hub_dir(&paths).join(".hank-generation");
        fs::remove_file(&marker).unwrap();
        fs::create_dir(&marker).unwrap();

        let second_bd =
            FakeBdClient::new().with_export_content(&repo, b"{\"id\":\"ra-1\"}\n".to_vec());
        let (snapshot, warnings) = gather_snapshot_with_state(&second_bd, &config, &paths, &state);

        assert!(snapshot.is_none());
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("generation marker"))
        );
        assert!(state.hub_access.read().unwrap().current_hub.is_none());
        assert!(state.map_for(old_generation).is_some());
    }

    #[test]
    fn sync_failure_preserves_current_hub_generation() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let repo = seed_repo(tmp.path(), "repo", "ra");
        let config = roster(&[&repo]);
        let state = RuntimeRefreshState::default();
        let first_bd = FakeBdClient::new()
            .with_ready(vec![issue("ra-1", 1, "ready")])
            .with_export_content(&repo, b"{\"id\":\"ra-1\"}\n".to_vec());
        let old_generation = gather_snapshot_with_state(&first_bd, &config, &paths, &state)
            .0
            .unwrap()
            .rows[0]
            .attribution_generation
            .unwrap();
        let failing = FakeBdClient::new()
            .with_repo_sync_err(bd_err())
            .with_export_content(&repo, b"{\"id\":\"ra-1\"}\n".to_vec());

        assert!(
            gather_snapshot_with_state(&failing, &config, &paths, &state)
                .0
                .is_none()
        );
        assert_eq!(
            state
                .hub_access
                .read()
                .unwrap()
                .current_hub
                .as_ref()
                .unwrap()
                .generation,
            old_generation
        );
    }

    #[test]
    fn generation_pruning_retains_current_and_explicit_references() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let repo = seed_repo(tmp.path(), "repo", "ra");
        let config = roster(&[&repo]);
        let state = RuntimeRefreshState::default();
        let bd = FakeBdClient::new()
            .with_ready(vec![issue("ra-1", 1, "ready")])
            .with_export_content(&repo, b"{\"id\":\"ra-1\"}\n".to_vec());
        let first = gather_snapshot_with_state(&bd, &config, &paths, &state)
            .0
            .unwrap()
            .rows[0]
            .attribution_generation
            .unwrap();
        let second = gather_snapshot_with_state(&bd, &config, &paths, &state)
            .0
            .unwrap()
            .rows[0]
            .attribution_generation
            .unwrap();
        let current = gather_snapshot_with_state(&bd, &config, &paths, &state)
            .0
            .unwrap()
            .rows[0]
            .attribution_generation
            .unwrap();

        state.prune(&HashSet::from([first]));

        assert!(state.map_for(first).is_some());
        assert!(state.map_for(current).is_some());
        assert!(state.map_for(second).is_none());
    }

    #[test]
    fn cached_row_without_generation_uses_hub_copy_fallback_without_prefix_reads() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let row = copy_row("repo", "ra-1");
        let bd = FakeBdClient::new();

        let (payload, _) = build_copy_with_map(&bd, &paths, &row, false, None);

        assert_eq!(
            payload,
            format!("bd -C {} show ra-1", hub_dir(&paths).display())
        );
        assert!(bd.calls().is_empty());
    }

    #[test]
    fn nested_source_panic_sends_one_fatal_completion_without_panicking_outer_worker() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let repo = seed_repo(tmp.path(), "repo", "ra");
        let bd = FakeBdClient::new().with_call_hook(|call| {
            if matches!(call, crate::bd::Call::Export(..)) {
                panic!("injected nested panic");
            }
        });
        let (tx, rx) = mpsc::channel();
        let worker_paths = paths.clone();
        let handle = thread::spawn(move || refresh_worker(bd, roster(&[&repo]), worker_paths, tx));

        assert_eq!(recv_msg(&rx), Msg::RefreshStarted);
        match recv_msg(&rx) {
            Msg::RefreshCompleted { snapshot, warnings } => {
                assert!(snapshot.is_none());
                assert!(
                    warnings
                        .iter()
                        .any(|warning| warning.contains("worker panicked"))
                );
            }
            other => panic!("expected fatal completion, got {other:?}"),
        }
        assert!(rx.recv().is_err(), "exactly one terminal completion");
        assert!(
            handle.join().is_ok(),
            "the outer refresh worker does not panic"
        );
        assert!(
            HubLock::try_acquire(&hub_dir(&paths)).unwrap().is_some(),
            "nested workers joined and released the hub lock"
        );
    }

    #[test]
    fn cached_search_prefixes_deduplicate_duplicate_roster_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let ra = seed_repo(tmp.path(), "ra", "ra");
        let config = roster(&[&ra, &ra]);
        let bd = FakeBdClient::new().with_search(vec![issue("ra-1", 1, "found")]);
        let prefixes = HashMap::from([(refresh::normalize_path(&ra), "ra".to_string())]);

        let rows = gather_search_with_prefixes(&bd, &config, &paths, "found", &prefixes)
            .expect("search succeeds");

        assert_eq!(
            rows[0].repo_name, "ra",
            "a duplicate roster entry must not turn one cached prefix into a collision"
        );
    }

    #[test]
    fn cached_copy_prefixes_deduplicate_duplicate_roster_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let ra = seed_repo(tmp.path(), "ra", "ra");
        let alias = ra.join(".");
        let config = roster(&[&ra, &alias]);
        let prefixes = HashMap::from([(refresh::normalize_path(&ra), "ra".to_string())]);
        let row = copy_row("ra", "ra-1");

        let (payload, _) = build_copy_with_prefixes(
            &FakeBdClient::new(),
            &config,
            &paths,
            &row,
            false,
            &prefixes,
        );

        assert_eq!(
            payload,
            format!("cd {} && bd show ra-1", ra.display()),
            "a duplicate roster entry must not force command copy through the hub"
        );
    }

    #[test]
    fn copy_worker_builds_cd_for_attributed() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let ra = seed_repo(tmp.path(), "ra", "ra");
        let bd = FakeBdClient::new();
        let (tx, rx) = mpsc::channel();

        let row = copy_row("ra", "ra-1");
        let paths2 = paths.clone();
        let ra2 = ra.clone();
        let handle =
            thread::spawn(move || copy_worker(bd, roster(&[&ra2]), paths2, row, false, 7, tx));

        let (token, payload, summary) = recv_copied(&rx);
        assert_eq!(token, 7, "the request token is echoed back");
        assert_eq!(
            payload,
            format!("cd {} && bd show ra-1", ra.display()),
            "attributed id resolves to its repo path"
        );
        assert!(
            summary.starts_with("cd ") && summary.chars().count() <= COPY_SUMMARY_MAX,
            "summary is the truncated command form: {summary}"
        );
        handle.join().unwrap();
    }

    #[test]
    fn copy_worker_falls_back_to_hub_for_unattributed() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let ra = seed_repo(tmp.path(), "ra", "ra");
        let bd = FakeBdClient::new();
        let (tx, rx) = mpsc::channel();

        // An id whose prefix (`zz`) matches no roster repo → hub fallback.
        let row = copy_row("unknown", "zz-9");
        let paths2 = paths.clone();
        let handle =
            thread::spawn(move || copy_worker(bd, roster(&[&ra]), paths2, row, false, 1, tx));

        let (_, payload, _) = recv_copied(&rx);
        assert_eq!(
            payload,
            format!("bd -C {} show zz-9", hub_dir(&paths).display()),
            "an unattributed id uses the always-correct hub form"
        );
        handle.join().unwrap();
    }

    #[test]
    fn copy_worker_markdown_block() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let ra = seed_repo(tmp.path(), "ra", "ra");
        let mut fresh = issue("ra-1", 2, "Renamed after snapshot");
        fresh.description = Some("Current description from bd show --json".into());
        let bd = FakeBdClient::new().with_show_issue(fresh);
        let (tx, rx) = mpsc::channel();

        // The row intentionally carries stale cached metadata. Markdown copy
        // refreshes it when requested instead of disagreeing with a freshly
        // loaded native detail pane.
        let row = copy_row("session-tui", "ra-1");
        let handle =
            thread::spawn(move || copy_worker(bd, roster(&[&ra]), paths, row, true, 1, tx));

        let (_, payload, _) = recv_copied(&rx);
        assert!(
            payload.contains("Renamed after snapshot"),
            "fresh markdown title: {payload:?}"
        );
        assert!(
            payload.contains("Current description from bd show --json"),
            "fresh markdown description: {payload:?}"
        );
        assert!(payload.contains("ra-1"), "markdown id: {payload:?}");
        assert!(
            payload.contains("session-tui"),
            "markdown repo: {payload:?}"
        );
        handle.join().unwrap();
    }

    #[test]
    fn pasted_query_keys_never_run_commands() {
        // Regression for the autoreview finding: a pasted `/qk` burst must open
        // search and type "qk" — never quit on `q` or move the list on `k`.
        // Decoding each raw key against the app's *live* focus (as `ui_loop`
        // does) guarantees this; a producer-side decoder reading an
        // asynchronously-published mode flag could map `q` to Quit before the
        // `/` was reduced. Here we drive the exact decode-then-reduce seam.
        use crossterm::event::{KeyCode, KeyModifiers};

        let mut app = App::new();
        app.reduce(Msg::RefreshCompleted {
            snapshot: Some(Snapshot {
                rows: Vec::new(),
                fetched_at: SystemTime::now(),
            }),
            warnings: Vec::new(),
        });

        for code in [KeyCode::Char('/'), KeyCode::Char('q'), KeyCode::Char('k')] {
            let key = KeyEvent::new(code, KeyModifiers::NONE);
            // Mirror the UI loop: decode against the current focus, then reduce.
            if let Some(msg) = keys::map_key(key, app.input_context()) {
                app.reduce(msg);
            }
        }

        assert!(
            !app.is_done(),
            "the pasted 'q' typed into the query, not quit"
        );
        assert_eq!(
            app.search_query(),
            Some("qk"),
            "the whole burst after '/' became the query text"
        );
    }

    #[test]
    fn gather_snapshot_none_when_refresh_declined() {
        // Another hank holds the lock: gather must NOT fetch a mis-attributed
        // snapshot; it returns None so the caller keeps its last-good rows.
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let ra = seed_repo(tmp.path(), "ra", "ra");
        let hub = hub_dir(&paths);
        fs::create_dir_all(&hub).unwrap();
        let _held = HubLock::try_acquire(&hub)
            .unwrap()
            .expect("acquired the lock");
        let bd = FakeBdClient::new().with_ready(vec![issue("ra-1", 1, "t")]);

        let (snapshot, warnings) = gather_snapshot(&bd, &roster(&[&ra]), &paths);

        assert!(
            snapshot.is_none(),
            "a declined refresh yields no snapshot, so last-good rows are kept"
        );
        assert!(
            warnings
                .iter()
                .any(|w| w.to_lowercase().contains("refreshing")),
            "the lock contention is surfaced: {warnings:?}"
        );
    }
}
