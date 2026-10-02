//! The pure app state core: an [`App`] value, a [`Msg`] enum covering keypresses
//! and the refresh lifecycle, and [`App::reduce`] mapping a message to a new
//! state plus a list of [`Effect`]s the runtime performs.
//!
//! No terminal, no threads, no `bd` calls, and no clock read inside `reduce`
//! (the shown snapshot's `fetched_at` is supplied by the caller; Slice 9 derives
//! staleness *age* from an injected `now`). Crossterm types appear only in
//! [`keys`], so this core stays backend-agnostic and exhaustively unit-testable.
//! See `plans/slices/slice-8.md`.

pub mod context;
pub mod keys;
pub mod view;

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use crate::bd::ShowDetail;
use crate::refresh::AttributionGeneration;
use crate::snapshot::{Row, Snapshot};

/// First delay before retrying a watcher refresh that failed; doubles per
/// consecutive failure up to [`WATCH_RETRY_MAX`].
const WATCH_RETRY_BASE: Duration = Duration::from_secs(5);
const WATCH_RETRY_MAX: Duration = Duration::from_secs(300);

/// Rows advanced by PageDown/PageUp in the detail pane. The view applies the
/// final content-height clamp because the pure app core does not know dimensions.
const DETAIL_PAGE_ROWS: u16 = 10;

/// A message driving a state transition: either a decoded keypress (see
/// [`keys::map_key`]) or a refresh-lifecycle event fed by the Slice 9 runtime's
/// worker thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Msg {
    // ---- Refresh lifecycle (runtime worker → app) ----
    /// A refresh began. Current rows stay visible and are marked [`App::is_stale`].
    RefreshStarted,
    /// A refresh cycle concluded, atomically: the fresh snapshot (`Some` on
    /// success, `None` when the refresh failed and the stale view is kept) plus
    /// the warnings/errors to surface (per-repo export failures, prefix
    /// collisions, missing roster paths, or a fatal sync error the runtime chose
    /// to show rather than abort on). One terminal message per cycle — the single
    /// point that clears [`App::is_stale`] — so a success-with-warnings cannot
    /// split into two `stale`-clearing messages and let an overlapping refresh
    /// slip through the dedup guard. Warnings are pre-formatted so this core
    /// stays free of `refresh`/`hub` error types.
    RefreshCompleted {
        snapshot: Option<Snapshot>,
        warnings: Vec<String>,
    },
    /// A `bd show` detail fetch concluded (runtime detail worker → app). The
    /// `token` echoes the request's generation (see [`Effect::FetchDetail`]) so a
    /// stale/out-of-order response is dropped — even when the *same* issue is
    /// reopened, whose two fetches would share an id but not a token. `detail` is
    /// the human-readable `bd show` stdout on success or a pre-formatted,
    /// sanitized message on failure (keeping this core free of `bd` error types).
    DetailReady {
        token: u64,
        detail: Result<String, String>,
    },
    /// A cross-repo search concluded (runtime search worker → app). `token` echoes
    /// the request's generation (see [`Effect::Search`]) so a superseded query's
    /// late reply is dropped. `rows` are **already attributed** `Row`s (the worker
    /// ran them through the same `PrefixMap` path as ready rows) on success, or a
    /// pre-formatted, sanitized message on failure — keeping this core free of
    /// `bd`/`PrefixMap` types and preserving `Msg`'s `Eq` derive.
    SearchResults {
        token: u64,
        rows: Result<Vec<Row>, String>,
    },

    // ---- Navigation ----
    /// Move the selection one row down (`j` / `Down`). Clamps at the last row.
    /// With the detail pane open, moves through the list behind the pane and
    /// refetches the detail for the newly selected bead.
    SelectNext,
    /// Move the selection one row up (`k` / `Up`). Clamps at the first row.
    /// With the detail pane open, moves through the list behind the pane.
    SelectPrev,
    /// Scroll the open detail pane one row down (`J`). No-op outside
    /// [`ViewMode::Detail`].
    DetailScrollDown,
    /// Scroll the open detail pane one row up (`K`). No-op outside
    /// [`ViewMode::Detail`].
    DetailScrollUp,
    /// Scroll the open detail pane one page down (`PageDown`). No-op outside
    /// [`ViewMode::Detail`].
    DetailPageDown,
    /// Scroll the open detail pane one page up (`PageUp`). No-op outside
    /// [`ViewMode::Detail`].
    DetailPageUp,
    /// Synchronize the stored scroll offset with the maximum visible offset
    /// computed by the pure renderer. This keeps PageUp relative to what was
    /// actually displayed after a partial final PageDown.
    DetailScrollBounds { max_scroll: u16 },

    // ---- Filters ----
    /// Open the repository picker over a browsable ready/search list (`f`).
    OpenRepoPicker,
    /// Move the repository picker's pending cursor down, clamped at the end.
    RepoPickerNext,
    /// Move the repository picker's pending cursor up, clamped at the start.
    RepoPickerPrev,
    /// Confirm the pending repository choice.
    ConfirmRepoPicker,
    /// Toggle the priority filter `All ↔ P0/P1-only` (`p`).
    TogglePriorityFilter,

    // ---- Commands / modes ----
    /// Request a refresh (`r`); `reduce` emits [`Effect::Refresh`].
    Refresh,
    /// Open the detail pane for the selected row (`Enter`). Placeholder in Slice
    /// 8; Slice 10 makes it emit `Effect::FetchDetail` and enter `Detail`.
    OpenDetail,
    /// Open cross-repo search (`/`): enter [`ViewMode::Search`] editing the query.
    OpenSearch,
    /// Append a character to the search query (a key typed while the search input
    /// is focused).
    SearchInput(char),
    /// Delete the last character of the search query (`Backspace` while editing).
    SearchBackspace,
    /// Run the current search query (`Enter` while editing); `reduce` emits
    /// [`Effect::Search`] unless the query is empty/whitespace.
    SubmitSearch,
    /// Copy an actionable `cd <repo> && bd show <id>` command for the selected
    /// row (`y`); `reduce` emits [`Effect::Copy`] with `markdown: false`.
    CopyContext,
    /// Copy a markdown block (title/id/repo/description) for the selected row
    /// (`Y`); `reduce` emits [`Effect::Copy`] with `markdown: true`.
    CopyMarkdown,
    /// A copy worker finished building the clipboard string (runtime copy worker →
    /// app). `token` echoes the request's generation (see [`Effect::Copy`]) so a
    /// superseded copy's late reply — the user copied row A, moved, copied row B,
    /// and A's slower path-resolution finished last — is dropped rather than
    /// clobbering the clipboard/confirmation with the stale selection. `payload`
    /// is the full text to place on the clipboard (via OSC 52); `summary` is the
    /// pre-truncated one-liner for the status-bar confirmation. Both are built off
    /// the UI thread (the id→repo-path resolution runs `bd`); on acceptance
    /// `reduce` stores `summary` and returns [`Effect::WriteClipboard`] so the
    /// actual tty write happens back on the UI thread.
    Copied {
        token: u64,
        payload: String,
        summary: String,
    },
    /// Persistence of a confirmed repository view completed. Results for a
    /// superseded view are ignored.
    RepoViewPersisted {
        repo: RepoFilter,
        result: Result<(), String>,
    },
    /// The live-refresh watcher saw journal records (or lost its place in a
    /// journal) and asks for a refresh of `scope`. Coalesced with any refresh
    /// already in flight: it runs once that cycle completes.
    WatchChanged(RefreshScope),
    /// A live-refresh problem worth surfacing (a repo's journal is off, `bd`
    /// can't follow it). Kept across refresh cycles; shown once per text.
    WatchWarning(String),
    /// How each roster repo fared in a refresh whose hub sync succeeded (runtime
    /// worker → app), sent just before that cycle's [`Msg::RefreshCompleted`].
    /// `synced_at` is the sync's wall-clock time. Folded into
    /// [`App::repo_health`]; never touches the in-flight flag.
    RepoSyncs {
        synced_at: SystemTime,
        repos: Vec<RepoSync>,
    },
    /// Open or close the sync-health panel (`h`). Opening runs `hank doctor`
    /// through [`Effect::CheckHealth`].
    ToggleHealth,
    /// Scroll the open health panel by this many rows (negative scrolls up).
    HealthScroll(i16),
    /// The health panel's `hank doctor` run concluded. `token` echoes
    /// [`Effect::CheckHealth`] so a run from an earlier opening is dropped;
    /// `report` is doctor's output, or a pre-formatted, sanitized failure.
    HealthReport {
        token: u64,
        report: Result<String, String>,
    },
    /// Leave the current sub-mode back to the list (`Esc`). No-op in `List`;
    /// Slices 10/11 return from `Detail`/`Search`.
    Back,
    /// Quit the app (`q`); sets [`App::is_done`].
    Quit,
}

/// A side effect the runtime must perform after a transition. `reduce` stays pure
/// by *describing* I/O rather than performing it. Slice 8 emits only
/// [`Effect::Refresh`]; Slice 10 adds `FetchDetail`, Slice 11 `Search(String)` —
/// additive, without changing `reduce`'s signature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Spawn a refresh worker over `scope`: [`RefreshScope::Full`] for the `r`
    /// keypress (`Msg::Refresh`), or the repos a [`Msg::WatchChanged`] named.
    Refresh(RefreshScope),
    /// Fetch one issue's detail via `bd show <id>` (the `Enter` keypress →
    /// `Msg::OpenDetail`). `token` is the request's generation; the runtime runs
    /// the fetch on a worker thread and echoes `token` back in [`Msg::DetailReady`]
    /// so a superseded request's late response is dropped.
    FetchDetail { id: String, token: u64 },
    /// Run `bd search <query> --json` against the hub (the `Enter` keypress while
    /// editing → `Msg::SubmitSearch`). `token` is the request's generation; the
    /// runtime runs the search on a worker thread, attributes the results, and
    /// echoes `token` back in [`Msg::SearchResults`] so a superseded query's late
    /// response is dropped.
    Search { query: String, token: u64 },
    /// Build the copy-context string for `row` (`y`/`Y`). Boxed to keep `Effect`
    /// small. The runtime resolves the row's source-repo path from its issue id
    /// (the same prefix-map path search uses), builds the `cd …`/`bd -C …`
    /// command (`markdown: false`) or a markdown block (`markdown: true`), and
    /// sends it back as [`Msg::Copied`] echoing `token` (the request's generation,
    /// so a superseded copy's late reply is dropped). Kept off `reduce` because
    /// `Row` carries no filesystem path (by [`crate::snapshot`] design), so the
    /// string can only be built where the roster/prefix map is available.
    Copy {
        row: Box<Row>,
        markdown: bool,
        token: u64,
    },
    /// Write `payload` to the terminal clipboard via an OSC 52 escape (the
    /// [`Msg::Copied`] follow-up). Performed on the UI thread, which owns the tty,
    /// so the escape can never interleave with a ratatui draw from a worker.
    WriteClipboard(String),
    /// Persist one newly confirmed repository view.
    PersistRepoView(RepoFilter),
    /// Run `hank doctor` off the UI thread for the health panel and send its
    /// output back as [`Msg::HealthReport`] echoing `token`.
    CheckHealth { token: u64 },
    /// A watcher refresh failed (another Hank held the hub lock, a sync
    /// failed, ...): send `Msg::WatchChanged(scope)` again after `after`, so
    /// the change the journal reported is not lost until the next write.
    RetryWatch {
        scope: RefreshScope,
        after: Duration,
    },
}

/// What a refresh re-exports before the hub sync.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefreshScope {
    /// Every roster repo (launch, `r`, a pruned journal checkpoint).
    Full,
    /// Only these repos (resolved, normalized roster paths); the rest keep
    /// their last export.
    Repos(BTreeSet<PathBuf>),
}

impl RefreshScope {
    /// The smallest scope covering both `self` and `other`.
    pub fn merge(self, other: RefreshScope) -> RefreshScope {
        match (self, other) {
            (RefreshScope::Repos(mut a), RefreshScope::Repos(b)) => {
                a.extend(b);
                RefreshScope::Repos(a)
            }
            _ => RefreshScope::Full,
        }
    }
}

/// How one roster repo fared in a refresh cycle, as the runtime reports it.
/// Failures are pre-formatted so this core stays free of `refresh` error types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoSync {
    /// The resolved roster path.
    pub path: PathBuf,
    /// The repo's id prefix (what [`Row::repo_id`] carries), when known.
    pub prefix: Option<String>,
    pub outcome: RepoSyncOutcome,
}

/// The result of one repo's part in a refresh cycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoSyncOutcome {
    /// Exported cleanly; the hub now holds its latest issues.
    Exported,
    /// A scoped (live) refresh skipped it because its journal reported no
    /// change, so its last export is still current.
    Carried,
    /// It failed this cycle; the hub keeps whatever it last exported. The
    /// message is pre-formatted and sanitized.
    Failed(String),
}

/// One roster repo's freshness, accumulated across refresh cycles, for the
/// health panel and the stale-repo flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoHealth {
    /// The resolved roster path.
    pub path: PathBuf,
    /// The repo's id prefix, when known.
    pub prefix: Option<String>,
    /// When the hub last took a clean export of this repo (that cycle's sync
    /// time). `None` if it has not exported cleanly since launch.
    pub synced_at: Option<SystemTime>,
    /// Why the latest attempt failed; `None` after a clean one.
    pub problem: Option<String>,
}

impl RepoHealth {
    /// Whether the hub's copy of this repo is behind: its latest refresh failed,
    /// so it shows an older export (or nothing).
    pub fn is_stale(&self) -> bool {
        self.problem.is_some()
    }
}

/// The `hank doctor` part of the health panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DoctorState {
    /// Doctor is running.
    Running,
    /// Doctor's output.
    Done(String),
    /// Doctor could not run; pre-formatted and sanitized.
    Failed(String),
}

/// The open sync-health panel.
#[derive(Debug, Clone)]
struct HealthPanel {
    /// The generation of this opening's doctor run.
    token: u64,
    doctor: DoctorState,
    /// Vertical scroll offset (rows); the view clamps it to the content.
    scroll: u16,
}

/// Which screen the app is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    /// Before the first snapshot arrives.
    Loading,
    /// The cross-repo ready list.
    List,
    /// One issue's detail pane (opened with `Enter`, left with `Esc`).
    Detail,
    /// Cross-repo search: a query input and its results (opened with `/`).
    Search,
}

/// Live key-routing context. Picker input takes precedence over the underlying
/// screen, then search editing, then normal commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputContext {
    Normal,
    SearchEditing,
    RepoPicker,
    Health,
}

/// The detail pane's state for one issue id.
#[derive(Debug, Clone)]
pub enum DetailState {
    /// The `bd show` fetch is in flight for this id.
    Loading { id: String },
    /// The fetched native output plus structured issue data for actions.
    Loaded(Box<ShowDetail>),
    /// The fetch failed; `message` is a pre-formatted, sanitized reason.
    Error { id: String, message: String },
}

impl DetailState {
    /// The issue id the pane is showing, across every variant (for the header /
    /// error line; stale-response matching is by request token, not id).
    pub fn id(&self) -> &str {
        match self {
            DetailState::Loading { id } => id,
            DetailState::Loaded(detail) => &detail.issue.id,
            DetailState::Error { id, .. } => id,
        }
    }
}

/// A navigable, filterable list of rows with a clamped selection — the shared
/// "row list" state the ready list and the search results both use, so selection,
/// filtering, and navigation live in **one** place (Slice 11's required refactor).
/// The Slice 8 selection invariant (`filtered_ix` empty ⇒ `selection == 0` and
/// `selected_row() == None`; else `selection < filtered_ix.len()`) is enforced
/// here after every mutation.
#[derive(Debug, Clone, Default)]
struct RowList {
    /// Every row (unfiltered), in display (sorted) order.
    rows: Vec<Row>,
    /// Indices into `rows` passing `filter`, in display order.
    filtered_ix: Vec<usize>,
    /// Offset into `filtered_ix` (never into `rows`).
    selection: usize,
    /// The list-local priority filter. The repository view belongs to `App`.
    filter: FilterSet,
}

impl RowList {
    /// Replace the rows, keeping the active filter, then recompute + re-clamp.
    fn set_rows(&mut self, rows: Vec<Row>, repo_view: &RepoFilter) {
        self.rows = rows;
        self.recompute(repo_view);
    }

    /// Rebuild `filtered_ix` under the current filter and re-clamp `selection` —
    /// the one place the selection invariant is re-established.
    fn recompute(&mut self, repo_view: &RepoFilter) {
        self.filtered_ix = (0..self.rows.len())
            .filter(|&i| self.filter.matches(repo_view, &self.rows[i]))
            .collect();
        if self.filtered_ix.is_empty() {
            self.selection = 0;
        } else if self.selection >= self.filtered_ix.len() {
            self.selection = self.filtered_ix.len() - 1;
        }
    }

    /// Move the selection one row down, clamping at the last row (safe when empty).
    fn select_next(&mut self) {
        if !self.filtered_ix.is_empty() {
            self.selection = (self.selection + 1).min(self.filtered_ix.len() - 1);
        }
    }

    /// Move the selection one row up, clamping at the first row (safe when empty).
    fn select_prev(&mut self) {
        self.selection = self.selection.saturating_sub(1);
    }

    /// Toggle the priority filter `All ↔ P0/P1-only`, then recompute.
    fn toggle_priority_filter(&mut self, repo_view: &RepoFilter) {
        self.filter.priority = match self.filter.priority {
            PriorityFilter::All => PriorityFilter::HighOnly,
            PriorityFilter::HighOnly => PriorityFilter::All,
        };
        self.recompute(repo_view);
    }

    /// Relocate the selection onto the visible row whose issue id is `id`,
    /// returning whether it was found (else the selection is left as-is).
    fn select_id(&mut self, id: &str) -> bool {
        if let Some(pos) = self
            .filtered_ix
            .iter()
            .position(|&i| self.rows[i].issue.id == id)
        {
            self.selection = pos;
            true
        } else {
            false
        }
    }

    /// The selection offset into the filtered rows, or `None` when nothing shows.
    fn selection(&self) -> Option<usize> {
        if self.filtered_ix.is_empty() {
            None
        } else {
            Some(self.selection)
        }
    }

    /// The selected row, or `None` when nothing is visible.
    fn selected_row(&self) -> Option<&Row> {
        self.filtered_ix.get(self.selection).map(|&i| &self.rows[i])
    }

    /// The rows passing the current filter, in display order.
    fn filtered_rows(&self) -> Vec<&Row> {
        self.filtered_ix.iter().map(|&i| &self.rows[i]).collect()
    }
}

/// A phase of the cross-repo search flow (see [`ViewMode::Search`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchPhase {
    /// The query input is focused; keys edit the query.
    Editing,
    /// A query was submitted; awaiting results.
    Loading,
    /// Attributed results are shown and browsable (the list may be empty).
    Results,
    /// The `bd search` call failed; the pre-formatted message is shown instead of
    /// a misleading empty result.
    Error(String),
}

/// The cross-repo search state, `Some` exactly while the search flow is live (in
/// [`ViewMode::Search`], or in [`ViewMode::Detail`] opened from a search result).
#[derive(Debug, Clone)]
struct SearchState {
    /// The query being edited / that produced the current results.
    query: String,
    /// Which phase of the flow.
    phase: SearchPhase,
    /// The generation of the in-flight/last request, matched against
    /// [`Msg::SearchResults`] to drop a superseded query's late reply.
    token: u64,
    /// The attributed results, sharing all navigation/filter code with the ready
    /// list.
    list: RowList,
}

/// A snapshotted repository-picker option list and pending cursor.
#[derive(Debug, Clone)]
struct RepoPickerState {
    choices: Vec<RepoFilter>,
    labels: Vec<String>,
    cursor: usize,
}

/// The repo-attribution axis of the filter. Attributed prefixes and the
/// unattributed bucket are separate variants, so a real prefix such as
/// `unknown` cannot collide with the bucket.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum RepoFilter {
    /// Show every repo.
    #[default]
    All,
    /// Show only rows attributed to this stable repository prefix.
    Only(String),
    /// Show only unattributed or ambiguously attributed rows.
    Unknown,
}

/// The priority axis of the filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PriorityFilter {
    /// Show every priority.
    #[default]
    All,
    /// Show only P0/P1 (`priority <= 1`).
    HighOnly,
}

/// The list-local priority filter. [`App::repo_view`] supplies the independent
/// repository axis when [`FilterSet::matches`] evaluates a row.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FilterSet {
    priority: PriorityFilter,
}

impl FilterSet {
    /// Whether a row passes the global repository and local priority axes.
    pub fn matches(&self, repo_view: &RepoFilter, row: &Row) -> bool {
        let repo_ok = match repo_view {
            RepoFilter::All => true,
            RepoFilter::Only(identity) => row.repo_id.as_deref() == Some(identity),
            RepoFilter::Unknown => row.repo_id.is_none(),
        };
        let priority_ok = match self.priority {
            PriorityFilter::All => true,
            PriorityFilter::HighOnly => row.issue.priority <= 1,
        };
        repo_ok && priority_ok
    }

    /// The active priority filter.
    pub fn priority(&self) -> PriorityFilter {
        self.priority
    }
}

/// The whole TUI state as a pure value. Fields are private to protect the
/// selection invariant (see [`App::reduce`]); read through the accessors.
#[derive(Debug, Clone)]
pub struct App {
    /// The application-wide repository view, shared by ready and search lists.
    repo_view: RepoFilter,
    /// Open picker overlay. Choices are frozen until it closes.
    repo_picker: Option<RepoPickerState>,
    /// Non-fatal warning from the latest relevant UI-state save failure.
    persistence_warning: Option<String>,
    /// The cross-repo ready list (rows, filter, selection), always maintained by
    /// refresh even while a search overlays it.
    ready: RowList,
    /// The cross-repo search flow, `Some` while it is live (see [`SearchState`]).
    /// The ready list stays untouched behind it, so `Esc` restores it exactly.
    search: Option<SearchState>,
    /// A monotonic generation stamped on each search request, echoed by the worker
    /// so a superseded query's late results are dropped (mirrors `detail_seq`).
    search_seq: u64,
    /// Which screen is shown.
    view_mode: ViewMode,
    /// A refresh is in flight over the shown rows (they may be about to change).
    stale: bool,
    /// Non-fatal warnings for the status bar, replaced each refresh cycle.
    status_warnings: Vec<String>,
    /// Whether the live-refresh watcher is running (shown in the status bar).
    watching: bool,
    /// Live-refresh warnings; unlike `status_warnings` these outlive a cycle.
    watch_warnings: Vec<String>,
    /// Per-repo freshness from the latest successful sync, in roster order.
    repo_health: Vec<RepoHealth>,
    /// The sync-health panel overlay, `Some` while open.
    health: Option<HealthPanel>,
    /// A monotonic generation stamped on each health-panel doctor run.
    health_seq: u64,
    /// The token of the doctor run still in flight, if any. Reopening the
    /// panel while one runs shares it instead of starting another, so a held
    /// `h` cannot pile up concurrent doctor runs.
    health_in_flight: Option<u64>,
    /// A watcher refresh requested while another refresh was in flight. Runs
    /// when that cycle completes, merged with any later requests.
    pending_refresh: Option<RefreshScope>,
    /// The scope of the in-flight refresh when the watcher started it (`None`
    /// for launch and `r`), retried if that refresh fails.
    watch_in_flight: Option<RefreshScope>,
    /// Consecutive failed watcher refreshes, for the retry backoff.
    watch_failures: u32,
    /// When the shown snapshot was fetched (injected upstream; Slice 9 renders
    /// its age against a `now`). `None` before the first snapshot.
    fetched_at: Option<SystemTime>,
    /// The detail pane, `Some` exactly when `view_mode == Detail`.
    detail: Option<DetailState>,
    /// The row the detail pane was opened from, kept for the whole pane lifetime
    /// (`Some` exactly with `detail`). A copy from the pane uses this row's repo
    /// attribution, so it survives a refresh that drops the issue from the ready
    /// list.
    detail_row: Option<Row>,
    /// A monotonic generation stamped on each detail request. The current pane's
    /// token is this value; a `DetailReady` is accepted only when its token still
    /// matches, so a superseded fetch (including a reopen of the same issue) is
    /// dropped.
    detail_seq: u64,
    /// The detail pane's vertical scroll offset (rows). Reset on open/close; the
    /// view clamps it to the wrapped content so all of a long detail is reachable.
    detail_scroll: u16,
    /// A transient "copied: …" confirmation for the status bar, set when a copy
    /// worker reports back and cleared on the next refresh cycle. `None` when no
    /// copy has happened since the last refresh.
    copy_flash: Option<String>,
    /// A monotonic generation stamped on each copy request, echoed by the worker
    /// so a superseded copy's late result is dropped (mirrors `detail_seq` /
    /// `search_seq`): the newest copy always wins the clipboard.
    copy_seq: u64,
    /// Whether a copy worker is resolving right now. New copy requests coalesce
    /// into `copy_pending` while this holds, so a held `y`/`Y` cannot spawn an
    /// unbounded pile of subprocess-backed workers (at most one runs at a time).
    copy_in_flight: bool,
    /// A copy requested while one was already in flight: the target row (captured
    /// and validated at press time, so a later cursor move can't redirect it) and
    /// its format. Only the latest is kept; it fires when the flight completes.
    copy_pending: Option<(Box<Row>, bool)>,
    /// The user asked to quit; the runtime loop should exit.
    done: bool,
}

impl Default for App {
    fn default() -> Self {
        App::new()
    }
}

impl App {
    /// A fresh app: `Loading`, no rows, no selection, not done — and already
    /// **in-flight**. Construction is always part of launch, which immediately
    /// initiates the first refresh (the Slice 9 runtime spawns it), so the app is
    /// born `stale`: this reserves the refresh in-flight slot from the very first
    /// event, deduping an `r` keypress that races the initial worker's
    /// `RefreshStarted`. The flag clears when that first refresh concludes with a
    /// `RefreshCompleted`, like any other cycle.
    pub fn new() -> App {
        App::with_repo_view(RepoFilter::All)
    }

    /// Construct an app with the repository view restored from persisted UI
    /// state. The view is installed before cache or refresh rows can arrive.
    pub fn with_repo_view(repo_view: RepoFilter) -> App {
        App {
            repo_view,
            repo_picker: None,
            persistence_warning: None,
            ready: RowList::default(),
            search: None,
            search_seq: 0,
            view_mode: ViewMode::Loading,
            stale: true,
            status_warnings: Vec::new(),
            watching: false,
            watch_warnings: Vec::new(),
            repo_health: Vec::new(),
            health: None,
            health_seq: 0,
            health_in_flight: None,
            pending_refresh: None,
            watch_in_flight: None,
            watch_failures: 0,
            fetched_at: None,
            detail: None,
            detail_row: None,
            detail_seq: 0,
            detail_scroll: 0,
            copy_flash: None,
            copy_seq: 0,
            copy_in_flight: false,
            copy_pending: None,
            done: false,
        }
    }

    /// Seed a freshly-constructed app with a snapshot loaded from the on-disk
    /// cache, so launch can paint immediately instead of sitting in `Loading`
    /// until the real refresh lands. Deliberately **not** routed through
    /// `reduce(Msg::RefreshCompleted { .. })`: that message's terminal
    /// `self.stale = false` would clear the in-flight guard `App::new`
    /// reserves for the launch refresh the runtime spawns right after this
    /// call, opening a window where a fast `r` keypress dedups against the
    /// wrong (already-cleared) flag and spawns a second, overlapping refresh
    /// worker. Call this only before the app has processed any other
    /// message — `apply_snapshot`'s open-detail relocation is a no-op then
    /// anyway, since neither `detail` nor `search` can yet be set.
    pub fn hydrate_from_cache(&mut self, snapshot: Snapshot) {
        self.apply_snapshot(snapshot);
    }

    /// Apply a message, returning the effects the runtime must perform.
    ///
    /// Pure: given the same starting state and message, the resulting state and
    /// effects are identical and nothing outside `self` is touched. Every branch
    /// that can change the row set or filter re-establishes the selection
    /// invariant via [`RowList::recompute`].
    pub fn reduce(&mut self, msg: Msg) -> Vec<Effect> {
        // Esc always cancels the topmost overlay before it can affect the
        // underlying detail/search mode.
        if self.repo_picker.is_some() && matches!(&msg, Msg::Back) {
            self.repo_picker = None;
            return Vec::new();
        }
        if self.health.is_some() && matches!(&msg, Msg::Back) {
            self.health = None;
            return Vec::new();
        }
        match msg {
            Msg::RefreshStarted => {
                // Keep the current rows on screen; mark them stale while the
                // refresh runs. First load (no rows) stays in `Loading`.
                self.stale = true;
            }
            Msg::RefreshCompleted { snapshot, warnings } => {
                let failed_watch = match (snapshot.is_some(), self.watch_in_flight.take()) {
                    (false, scope) => scope,
                    (true, _) => {
                        self.watch_failures = 0;
                        None
                    }
                };
                if let Some(snapshot) = snapshot {
                    self.apply_snapshot(snapshot);
                }
                // The runtime sends the full warning set per cycle, so replace.
                self.status_warnings = warnings;
                // A fresh cycle clears any lingering copy confirmation.
                self.copy_flash = None;
                // The single, atomic point that ends the in-flight cycle.
                self.stale = false;
                // Changes the watcher saw mid-cycle may postdate this cycle's
                // exports, so they get a cycle of their own.
                if let Some(scope) = self.pending_refresh.take() {
                    let scope = match failed_watch {
                        Some(failed) => scope.merge(failed),
                        None => scope,
                    };
                    self.stale = true;
                    self.watch_in_flight = Some(scope.clone());
                    return vec![Effect::Refresh(scope)];
                }
                if let Some(scope) = failed_watch {
                    let after = WATCH_RETRY_BASE
                        .saturating_mul(1 << self.watch_failures.min(6))
                        .min(WATCH_RETRY_MAX);
                    self.watch_failures = self.watch_failures.saturating_add(1);
                    return vec![Effect::RetryWatch { scope, after }];
                }
            }
            // `j`/`k` move the selection of the active browsing list (ready in
            // `List`, the results in `Search`+`Results`). With the detail pane
            // open they move through the list *behind* the pane, refetching the
            // detail for each newly selected bead (`J`/`K` scroll the pane
            // instead). While the search editor is up, the selection never moves.
            Msg::SelectNext => {
                if self.view_mode == ViewMode::Detail {
                    return self.detail_nav(true);
                }
                if let Some(list) = self.browsing_list_mut() {
                    list.select_next();
                }
            }
            Msg::SelectPrev => {
                if self.view_mode == ViewMode::Detail {
                    return self.detail_nav(false);
                }
                if let Some(list) = self.browsing_list_mut() {
                    list.select_prev();
                }
            }
            // `J`/`K` scroll the open detail pane. The view clamps the offset to
            // the wrapped content height, so `reduce` stays dimension-free.
            Msg::DetailScrollDown => {
                if self.view_mode == ViewMode::Detail {
                    self.detail_scroll = self.detail_scroll.saturating_add(1);
                }
            }
            Msg::DetailScrollUp => {
                if self.view_mode == ViewMode::Detail {
                    self.detail_scroll = self.detail_scroll.saturating_sub(1);
                }
            }
            Msg::DetailPageDown => {
                if self.view_mode == ViewMode::Detail {
                    self.detail_scroll = self.detail_scroll.saturating_add(DETAIL_PAGE_ROWS);
                }
            }
            Msg::DetailPageUp => {
                if self.view_mode == ViewMode::Detail {
                    self.detail_scroll = self.detail_scroll.saturating_sub(DETAIL_PAGE_ROWS);
                }
            }
            Msg::DetailScrollBounds { max_scroll } => {
                // The view reports the bounds of whatever scrolls on top: the
                // health panel when open, else the detail pane.
                if let Some(health) = &mut self.health {
                    health.scroll = health.scroll.min(max_scroll);
                } else if self.view_mode == ViewMode::Detail {
                    self.detail_scroll = self.detail_scroll.min(max_scroll);
                }
            }
            Msg::OpenRepoPicker => {
                if self.is_browsing() && self.repo_picker.is_none() {
                    self.repo_picker = Some(self.build_repo_picker());
                }
            }
            Msg::RepoPickerNext => {
                if let Some(picker) = &mut self.repo_picker {
                    picker.cursor = (picker.cursor + 1).min(picker.choices.len() - 1);
                }
            }
            Msg::RepoPickerPrev => {
                if let Some(picker) = &mut self.repo_picker {
                    picker.cursor = picker.cursor.saturating_sub(1);
                }
            }
            Msg::ConfirmRepoPicker => {
                let Some(picker) = self.repo_picker.take() else {
                    return Vec::new();
                };
                let selected = picker.choices[picker.cursor].clone();
                if selected != self.repo_view {
                    self.apply_repo_view(selected.clone());
                    return vec![Effect::PersistRepoView(selected)];
                }
                if self.persistence_warning.is_some() {
                    return vec![Effect::PersistRepoView(selected)];
                }
            }
            Msg::TogglePriorityFilter => {
                let repo_view = self.repo_view.clone();
                if let Some(list) = self.browsing_list_mut() {
                    list.toggle_priority_filter(&repo_view);
                }
            }
            Msg::Refresh => {
                // Dedup: a refresh is already pending/in-flight (`stale`), so
                // requesting another would spawn an overlapping worker whose
                // out-of-order completion could clobber a newer snapshot. Mark
                // in-flight synchronously here — before any `RefreshStarted`
                // arrives — so a mashed or key-repeated `r` yields one effect.
                // The guard clears only when the single terminal
                // `RefreshCompleted` arrives, so there is no window between two
                // completion messages for an overlapping request to slip through.
                if self.stale {
                    return Vec::new();
                }
                self.stale = true;
                return vec![Effect::Refresh(RefreshScope::Full)];
            }
            Msg::WatchChanged(scope) => {
                if self.stale {
                    self.pending_refresh = Some(match self.pending_refresh.take() {
                        Some(pending) => pending.merge(scope),
                        None => scope,
                    });
                    return Vec::new();
                }
                self.stale = true;
                self.watch_in_flight = Some(scope.clone());
                return vec![Effect::Refresh(scope)];
            }
            Msg::WatchWarning(warning) => {
                if !self.watch_warnings.contains(&warning) {
                    self.watch_warnings.push(warning);
                }
            }
            Msg::RepoSyncs { synced_at, repos } => self.apply_repo_syncs(synced_at, repos),
            Msg::ToggleHealth => {
                if self.health.take().is_some() {
                    return Vec::new();
                }
                if let Some(token) = self.health_in_flight {
                    self.health = Some(HealthPanel {
                        token,
                        doctor: DoctorState::Running,
                        scroll: 0,
                    });
                    return Vec::new();
                }
                self.health_seq += 1;
                let token = self.health_seq;
                self.health_in_flight = Some(token);
                self.health = Some(HealthPanel {
                    token,
                    doctor: DoctorState::Running,
                    scroll: 0,
                });
                return vec![Effect::CheckHealth { token }];
            }
            Msg::HealthScroll(delta) => {
                if let Some(health) = &mut self.health {
                    health.scroll = health.scroll.saturating_add_signed(delta);
                }
            }
            Msg::HealthReport { token, report } => {
                if self.health_in_flight == Some(token) {
                    self.health_in_flight = None;
                }
                if let Some(health) = &mut self.health
                    && health.token == token
                {
                    health.doctor = match report {
                        Ok(output) => DoctorState::Done(output),
                        Err(message) => DoctorState::Failed(message),
                    };
                }
            }
            Msg::OpenDetail => {
                // Open only from a browsing list (the ready list, or search
                // results), and only with a selected row: exactly one `bd show`
                // per Enter (a second Enter in `Detail` is a no-op, an empty list
                // has no row), and cursor movement never fetches. The search state
                // is left intact, so `Back` returns to the results behind the pane.
                if self.is_browsing()
                    && let Some(row) = self.active().selected_row()
                {
                    // Remember the opened row so a copy from the pane keeps its
                    // exact issue and repo attribution even if a later refresh
                    // drops it from the list (the pane pins one issue).
                    let row = row.clone();
                    let id = row.issue.id.clone();
                    self.detail_seq += 1;
                    let token = self.detail_seq;
                    self.view_mode = ViewMode::Detail;
                    self.detail = Some(DetailState::Loading { id: id.clone() });
                    self.detail_row = Some(row);
                    self.detail_scroll = 0;
                    return vec![Effect::FetchDetail { id, token }];
                }
            }
            Msg::DetailReady { token, detail } => {
                // Accept only the response for the pane's current request; a
                // superseded one (the user moved on, or reopened the same issue)
                // carries an older token and is dropped.
                if self.detail.is_some() && token == self.detail_seq {
                    let id = self
                        .detail
                        .as_ref()
                        .map(DetailState::id)
                        .unwrap_or("")
                        .to_string();
                    self.detail = Some(match detail {
                        Ok(output) => {
                            let issue = self
                                .detail_row
                                .as_ref()
                                .expect("detail state always retains its opened row")
                                .issue
                                .clone();
                            DetailState::Loaded(Box::new(ShowDetail { output, issue }))
                        }
                        // Reuse the id the pane is already bound to (the Loading
                        // state) so the error names the right issue.
                        Err(message) => DetailState::Error { id, message },
                    });
                }
            }
            Msg::Back => match self.view_mode {
                // Leave the detail pane, returning to the search results it was
                // opened from (if any) or the ready list; the selection is
                // untouched, so it is preserved across an open/close.
                ViewMode::Detail => {
                    self.view_mode = if self.search.is_some() {
                        ViewMode::Search
                    } else {
                        ViewMode::List
                    };
                    self.detail = None;
                    self.detail_row = None;
                    self.detail_scroll = 0;
                }
                // From the query editor, `Esc` exits search and restores the ready
                // list (never touched, so this is exact). From the results/loading/
                // error phases it returns to editing so the query can be refined.
                ViewMode::Search => {
                    if let Some(s) = &mut self.search {
                        if matches!(s.phase, SearchPhase::Editing) {
                            self.view_mode = ViewMode::List;
                            self.search = None;
                        } else {
                            s.phase = SearchPhase::Editing;
                        }
                    }
                }
                ViewMode::List | ViewMode::Loading => {}
            },
            Msg::OpenSearch => match self.view_mode {
                // `/` from the ready list opens an empty query editor; the ready
                // list is preserved behind it.
                ViewMode::List => {
                    self.view_mode = ViewMode::Search;
                    self.search = Some(SearchState {
                        query: String::new(),
                        phase: SearchPhase::Editing,
                        token: 0,
                        list: RowList::default(),
                    });
                }
                // `/` while viewing results (not editing — a typed `/` is input)
                // restarts a fresh query.
                ViewMode::Search => {
                    if let Some(s) = &mut self.search {
                        s.query.clear();
                        s.phase = SearchPhase::Editing;
                    }
                }
                ViewMode::Detail | ViewMode::Loading => {}
            },
            Msg::SearchInput(c) => {
                if let Some(s) = &mut self.search
                    && matches!(s.phase, SearchPhase::Editing)
                {
                    s.query.push(c);
                }
            }
            Msg::SearchBackspace => {
                if let Some(s) = &mut self.search
                    && matches!(s.phase, SearchPhase::Editing)
                {
                    s.query.pop();
                }
            }
            Msg::SubmitSearch => {
                // Only a non-empty query submits; bump the request generation,
                // enter `Loading`, and ask the runtime to run the search.
                let ready = matches!(
                    self.search.as_ref().map(|s| &s.phase),
                    Some(SearchPhase::Editing)
                ) && self
                    .search
                    .as_ref()
                    .is_some_and(|s| !s.query.trim().is_empty());
                if ready {
                    self.search_seq += 1;
                    let token = self.search_seq;
                    let s = self.search.as_mut().expect("checked Some above");
                    s.token = token;
                    s.phase = SearchPhase::Loading;
                    let query = s.query.clone();
                    return vec![Effect::Search { query, token }];
                }
            }
            Msg::SearchResults { token, rows } => {
                let repo_view = self.repo_view.clone();
                // Accept only the response for the current, still-pending query; a
                // superseded one (re-submitted, or `Esc`'d back to editing) is
                // dropped by the token + `Loading`-phase guard.
                if let Some(s) = &mut self.search
                    && token == s.token
                    && matches!(s.phase, SearchPhase::Loading)
                {
                    match rows {
                        Ok(rows) => {
                            s.list.set_rows(rows, &repo_view);
                            s.phase = SearchPhase::Results;
                        }
                        Err(message) => {
                            s.list.set_rows(Vec::new(), &repo_view);
                            s.phase = SearchPhase::Error(message);
                        }
                    }
                }
            }
            // `y`/`Y` copy the selected row's context. `reduce` can't build the
            // string (a `Row` carries no path), so it emits `Effect::Copy` and the
            // runtime resolves the path + builds the string off the UI thread.
            Msg::CopyContext => return self.copy_effect(false),
            Msg::CopyMarkdown => return self.copy_effect(true),
            Msg::Copied {
                token,
                payload,
                summary,
            } => {
                // Accept only the in-flight copy's result; a stale token (defensive
                // — coalescing keeps one copy in flight) is dropped without ending
                // the flight.
                if token == self.copy_seq {
                    self.copy_in_flight = false;
                    self.copy_flash = Some(summary);
                    let mut effects = vec![Effect::WriteClipboard(payload)];
                    // A copy requested while this one was resolving was coalesced;
                    // launch that captured target now (the row was validated and
                    // snapshotted at press time, so a later cursor move can't
                    // redirect it), without ever having spawned a second worker.
                    if let Some((row, markdown)) = self.copy_pending.take() {
                        effects.push(self.launch_copy(row, markdown));
                    }
                    return effects;
                }
            }
            Msg::RepoViewPersisted { repo, result } => {
                if repo == self.repo_view {
                    self.persistence_warning = result.err();
                }
            }
            Msg::Quit => self.done = true,
        }
        Vec::new()
    }

    /// Fold one successful sync's per-repo results into [`App::repo_health`].
    /// The new list follows the reported roster, so a removed repo drops out.
    /// A clean export is fresh as of `synced_at`; a failure keeps the last
    /// clean time; a carried-over repo (not exported) keeps its last clean
    /// time and any standing problem.
    fn apply_repo_syncs(&mut self, synced_at: SystemTime, repos: Vec<RepoSync>) {
        let mut previous: HashMap<PathBuf, RepoHealth> = self
            .repo_health
            .drain(..)
            .map(|health| (health.path.clone(), health))
            .collect();
        self.repo_health = repos
            .into_iter()
            .map(|repo| {
                let before = previous.remove(&repo.path);
                let last_clean = before.as_ref().and_then(|health| health.synced_at);
                let (synced_at, problem) = match repo.outcome {
                    RepoSyncOutcome::Exported => (Some(synced_at), None),
                    // Not exported this cycle: its last clean export and any
                    // standing problem carry over unchanged.
                    RepoSyncOutcome::Carried => match before {
                        Some(before) => (before.synced_at, before.problem),
                        None => (Some(synced_at), None),
                    },
                    RepoSyncOutcome::Failed(message) => (last_clean, Some(message)),
                };
                RepoHealth {
                    path: repo.path,
                    prefix: repo.prefix,
                    synced_at,
                    problem,
                }
            })
            .collect();
    }

    /// Move the selection of the list behind the open detail pane (`j`/`k` in
    /// `Detail`) and refetch the pane for the newly selected bead, so the pane
    /// tracks the list without a `Esc`+`Enter` round trip. At a clamped edge the
    /// selection — and therefore the pane — is unchanged, so no duplicate fetch
    /// is issued; the old fetch's token is superseded on every real move.
    fn detail_nav(&mut self, next: bool) -> Vec<Effect> {
        // The list the pane was opened from: the search results while a search is
        // live (a detail opened from a result keeps `search` intact), else ready.
        let list = match &mut self.search {
            Some(s) => &mut s.list,
            None => &mut self.ready,
        };
        if next {
            list.select_next();
        } else {
            list.select_prev();
        }
        let Some(row) = list.selected_row().cloned() else {
            return Vec::new();
        };
        if self.detail_row.as_ref().map(|r| &r.issue.id) == Some(&row.issue.id) {
            return Vec::new(); // clamped at an edge: the pane already shows it
        }
        self.detail_seq += 1;
        let token = self.detail_seq;
        let id = row.issue.id.clone();
        self.detail = Some(DetailState::Loading { id: id.clone() });
        self.detail_row = Some(row);
        self.detail_scroll = 0;
        vec![Effect::FetchDetail { id, token }]
    }

    /// Snapshot deterministic picker choices from every unfiltered list known to
    /// the app. The confirmed stale choice remains visible even with no rows.
    fn build_repo_picker(&self) -> RepoPickerState {
        let mut labels_by_identity = HashMap::<RepoFilter, String>::new();
        for row in self.ready.rows.iter().chain(
            self.search
                .as_ref()
                .into_iter()
                .flat_map(|search| search.list.rows.iter()),
        ) {
            let identity = match &row.repo_id {
                Some(prefix) => RepoFilter::Only(prefix.clone()),
                None => RepoFilter::Unknown,
            };
            let candidate = row.repo_name.clone();
            labels_by_identity
                .entry(identity)
                .and_modify(|label| {
                    if candidate.len() > label.len()
                        || (candidate.len() == label.len() && candidate < *label)
                    {
                        *label = candidate.clone();
                    }
                })
                .or_insert(candidate);
        }
        match &self.repo_view {
            RepoFilter::All => {}
            RepoFilter::Only(identity) => {
                labels_by_identity
                    .entry(self.repo_view.clone())
                    .or_insert_with(|| identity.clone());
            }
            RepoFilter::Unknown => {
                labels_by_identity
                    .entry(RepoFilter::Unknown)
                    .or_insert_with(|| crate::snapshot::UNKNOWN_REPO.to_string());
            }
        }
        let mut repositories: Vec<(RepoFilter, String)> = labels_by_identity.into_iter().collect();
        loop {
            let mut label_counts = HashMap::<String, usize>::new();
            for (_, label) in &repositories {
                *label_counts.entry(label.clone()).or_default() += 1;
            }
            let mut changed = false;
            for (identity, label) in &mut repositories {
                if label_counts.get(label.as_str()).copied().unwrap_or(0) > 1 {
                    let suffix = match identity {
                        RepoFilter::Only(prefix) => format!("prefix {prefix}"),
                        RepoFilter::Unknown => "unattributed".to_string(),
                        RepoFilter::All => unreachable!("All is not a repository choice"),
                    };
                    *label = format!("{label} ({suffix})");
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        repositories.sort_by(|(left_id, left_label), (right_id, right_label)| {
            left_label
                .to_lowercase()
                .cmp(&right_label.to_lowercase())
                .then_with(|| left_label.cmp(right_label))
                .then_with(|| left_id.cmp(right_id))
        });

        let mut choices = Vec::with_capacity(repositories.len() + 1);
        let mut labels = Vec::with_capacity(repositories.len() + 1);
        choices.push(RepoFilter::All);
        labels.push("All repos".to_string());
        for (identity, label) in repositories {
            choices.push(identity);
            labels.push(label);
        }
        let cursor = choices
            .iter()
            .position(|choice| choice == &self.repo_view)
            .unwrap_or(0);
        RepoPickerState {
            choices,
            labels,
            cursor,
        }
    }

    /// Apply one confirmed global repository view to every materialized list and
    /// reset each affected selection to its first visible row.
    fn apply_repo_view(&mut self, repo: RepoFilter) {
        self.repo_view = repo.clone();
        self.ready.selection = 0;
        self.ready.recompute(&repo);
        if let Some(search) = &mut self.search {
            search.list.selection = 0;
            search.list.recompute(&repo);
        }
    }

    /// Emit an [`Effect::Copy`] for the selected row of the active list, or no
    /// effect when nothing is selected (an empty list, or the search editor). The
    /// active list is the search results while a search is live (including a
    /// detail opened from one) else the ready list, so `y`/`Y` copy the issue the
    /// user is actually looking at in List / Search-results / Detail alike.
    fn copy_effect(&mut self, markdown: bool) -> Vec<Effect> {
        // Resolve the target at press time, so a copy always names the row the user
        // was looking at when they pressed `y`/`Y` — never a row they moved to
        // later, and never anything when the current mode has no source (e.g. the
        // search `Loading` phase, where copy is a no-op).
        let Some(row) = self.copy_source_row() else {
            return Vec::new();
        };
        let row = Box::new(row);
        // Coalesce while a copy is resolving: its path resolution runs a `bd`
        // subprocess per repo on a worker thread, so a held `y`/`Y` (auto-repeat
        // arrives as a burst of key events) must not pile up workers. Capture this
        // (already-validated) target; only the latest fires when the flight ends.
        if self.copy_in_flight {
            self.copy_pending = Some((row, markdown));
            return Vec::new();
        }
        vec![self.launch_copy(row, markdown)]
    }

    /// Start a copy for an already-resolved `row`: stamp a fresh generation, mark
    /// the flight in progress, and return the [`Effect::Copy`] the runtime runs.
    fn launch_copy(&mut self, row: Box<Row>, markdown: bool) -> Effect {
        self.copy_seq += 1;
        self.copy_in_flight = true;
        Effect::Copy {
            row,
            markdown,
            token: self.copy_seq,
        }
    }

    /// The row `y`/`Y` should copy for the current mode, or `None` when there is
    /// nothing to copy.
    ///
    /// In `Detail` this is the issue the pane *pins* — the row it was opened from
    /// (`detail_row`), **not** the underlying list selection, which a refresh can
    /// re-clamp to a different row while the pane stays open. Its repo attribution
    /// is preserved even if the issue has since left the ready list.
    /// Otherwise only a browsable list copies (the ready list, or *settled* search
    /// results): the search editor and its `Loading` phase have no visible
    /// selection, so `y` there must not copy a retained, invisible result.
    fn copy_source_row(&self) -> Option<Row> {
        if self.view_mode == ViewMode::Detail {
            let opened = self.detail_row.as_ref()?;
            let issue = match &self.detail {
                Some(DetailState::Loaded(detail)) if detail.issue.id == opened.issue.id => {
                    detail.issue.clone()
                }
                _ => opened.issue.clone(),
            };
            return Some(Row {
                issue,
                repo_id: opened.repo_id.clone(),
                repo_name: opened.repo_name.clone(),
                attribution_generation: opened.attribution_generation,
            });
        }
        if self.is_browsing() {
            return self.active().selected_row().cloned();
        }
        None
    }

    /// The list the read accessors and the view reflect: the search results while
    /// the search flow is live (including a detail opened from a result), else the
    /// ready list. So one read API renders whichever list is active.
    fn active(&self) -> &RowList {
        match &self.search {
            Some(s) => &s.list,
            None => &self.ready,
        }
    }

    /// The list a navigation/filter key should act on right now, or `None` when
    /// keys don't move a selection (the detail pane scrolls; the search editor and
    /// its loading phase take no navigation). `List` acts on the ready list;
    /// `Search`+`Results` on the results.
    fn browsing_list_mut(&mut self) -> Option<&mut RowList> {
        match self.view_mode {
            ViewMode::List => Some(&mut self.ready),
            ViewMode::Search => match &mut self.search {
                Some(s) if matches!(s.phase, SearchPhase::Results) => Some(&mut s.list),
                _ => None,
            },
            ViewMode::Detail | ViewMode::Loading => None,
        }
    }

    /// Whether a navigation list is currently browsable (so `Enter` may open a
    /// detail): the ready list, or search results.
    fn is_browsing(&self) -> bool {
        matches!(self.view_mode, ViewMode::List)
            || (self.view_mode == ViewMode::Search
                && matches!(
                    self.search.as_ref().map(|s| &s.phase),
                    Some(SearchPhase::Results)
                ))
    }

    // ---- Accessors (the Slice 9 view's read API) ----

    /// The current screen.
    pub fn view_mode(&self) -> ViewMode {
        self.view_mode
    }

    /// Every row of the active list (unfiltered), in display order.
    pub fn rows(&self) -> &[Row] {
        &self.active().rows
    }

    /// The active list's rows passing its filter, in display order.
    pub fn filtered_rows(&self) -> Vec<&Row> {
        self.active().filtered_rows()
    }

    /// The selection offset into [`App::filtered_rows`], or `None` when nothing
    /// is visible.
    pub fn selection(&self) -> Option<usize> {
        self.active().selection()
    }

    /// The selected row, or `None` when nothing is visible.
    pub fn selected_row(&self) -> Option<&Row> {
        self.active().selected_row()
    }

    /// Attribution generations still referenced by visible or retained app
    /// state. Runtime uses this to prune immutable prefix maps safely.
    pub fn attribution_generations(&self) -> HashSet<AttributionGeneration> {
        let mut generations = HashSet::new();
        generations.extend(
            self.ready
                .rows
                .iter()
                .filter_map(|row| row.attribution_generation),
        );
        if let Some(search) = &self.search {
            generations.extend(
                search
                    .list
                    .rows
                    .iter()
                    .filter_map(|row| row.attribution_generation),
            );
        }
        if let Some(row) = &self.detail_row
            && let Some(generation) = row.attribution_generation
        {
            generations.insert(generation);
        }
        if let Some((row, _)) = &self.copy_pending
            && let Some(generation) = row.attribution_generation
        {
            generations.insert(generation);
        }
        generations
    }

    /// Install a fresh snapshot's rows: shared by `Msg::RefreshCompleted` and
    /// [`App::hydrate_from_cache`]. Does not touch `stale`/`status_warnings`/
    /// `copy_flash` — each caller owns those per its own semantics.
    fn apply_snapshot(&mut self, snapshot: Snapshot) {
        // With a detail opened *from the ready list*, remember the opened
        // issue so the refresh's re-sort does not move the selection to a
        // *different* row: the pane pins one issue, and `Esc` must return to
        // it. (Slice 8 decision 5 otherwise preserves only the selection
        // index; this narrower rule applies just while a ready-list detail is
        // open.) A detail opened from a *search* result must NOT relocate the
        // hidden ready selection — its id may also be a ready row, and moving
        // to it would corrupt the ready selection `Esc` restores. So relocate
        // only when no search is active (`search.is_none()`).
        let opened_id = if self.search.is_none() {
            self.detail.as_ref().map(|d| d.id().to_string())
        } else {
            None
        };
        // A refresh always updates the ready list, even under a search or
        // detail overlay; `set_rows` keeps the active filter and re-clamps
        // the selection. (`None` keeps the last-good rows.)
        self.ready.set_rows(snapshot.rows, &self.repo_view);
        self.fetched_at = Some(snapshot.fetched_at);
        // Only promote the first-snapshot transition; a refresh landing under
        // an open `Detail`/`Search` overlay must not slam it shut (the 1s
        // cadence would otherwise eject the reader).
        if self.view_mode == ViewMode::Loading {
            self.view_mode = ViewMode::List;
        }
        // Relocate the ready selection onto the opened ready issue if it
        // survived the refresh; otherwise the clamped index stands.
        if let Some(id) = opened_id {
            self.ready.select_id(&id);
        }
    }

    /// Whether a refresh is in flight over the shown rows.
    pub fn is_stale(&self) -> bool {
        self.stale
    }

    /// The status-bar warnings from the last refresh cycle.
    pub fn status_warnings(&self) -> &[String] {
        &self.status_warnings
    }

    /// Live-refresh warnings collected since launch.
    pub fn watch_warnings(&self) -> &[String] {
        &self.watch_warnings
    }

    /// Mark the live-refresh watcher as running. Called by the runtime at
    /// launch, before any message is processed.
    pub fn set_watching(&mut self, watching: bool) {
        self.watching = watching;
    }

    /// Whether the live-refresh watcher is running.
    pub fn is_watching(&self) -> bool {
        self.watching
    }

    /// Whether the user asked to quit.
    pub fn is_done(&self) -> bool {
        self.done
    }

    /// The active list's filter.
    pub fn filter(&self) -> &FilterSet {
        &self.active().filter
    }

    /// The confirmed application-wide repository view.
    pub fn repo_view(&self) -> &RepoFilter {
        &self.repo_view
    }

    /// Whether the repository picker overlay is open.
    pub fn repo_picker_open(&self) -> bool {
        self.repo_picker.is_some()
    }

    /// The picker's snapshotted choices, when open.
    pub fn repo_picker_choices(&self) -> Option<&[RepoFilter]> {
        self.repo_picker
            .as_ref()
            .map(|picker| picker.choices.as_slice())
    }

    /// Presentation labels corresponding one-to-one with the picker's choices.
    pub fn repo_picker_labels(&self) -> Option<&[String]> {
        self.repo_picker
            .as_ref()
            .map(|picker| picker.labels.as_slice())
    }

    /// The picker's pending cursor, when open.
    pub fn repo_picker_cursor(&self) -> Option<usize> {
        self.repo_picker.as_ref().map(|picker| picker.cursor)
    }

    /// The current UI-state persistence warning, if any.
    pub fn persistence_warning(&self) -> Option<&str> {
        self.persistence_warning.as_deref()
    }

    /// The current search query, if the search flow is live.
    pub fn search_query(&self) -> Option<&str> {
        self.search.as_ref().map(|s| s.query.as_str())
    }

    /// The current search phase, if the search flow is live.
    pub fn search_phase(&self) -> Option<&SearchPhase> {
        self.search.as_ref().map(|s| &s.phase)
    }

    /// Whether the search query input is focused (so a key edits the query rather
    /// than acting as a command). Read by the runtime to route key mapping.
    pub fn search_editing(&self) -> bool {
        matches!(self.search_phase(), Some(SearchPhase::Editing))
    }

    /// The current key-routing context.
    pub fn input_context(&self) -> InputContext {
        if self.repo_picker.is_some() {
            InputContext::RepoPicker
        } else if self.health.is_some() {
            InputContext::Health
        } else if self.search_editing() {
            InputContext::SearchEditing
        } else {
            InputContext::Normal
        }
    }

    /// The number of rows the current search returned (0 when not in results).
    pub fn search_result_count(&self) -> usize {
        self.search.as_ref().map(|s| s.list.rows.len()).unwrap_or(0)
    }

    /// When the shown snapshot was fetched, if any.
    pub fn fetched_at(&self) -> Option<SystemTime> {
        self.fetched_at
    }

    /// The detail pane state, `Some` exactly when [`ViewMode::Detail`] is shown.
    pub fn detail(&self) -> Option<&DetailState> {
        self.detail.as_ref()
    }

    /// The detail pane's requested vertical scroll offset (rows). The view clamps
    /// this to the wrapped content so an over-scroll never shows blank space.
    pub fn detail_scroll(&self) -> u16 {
        self.detail_scroll
    }

    /// The transient copy confirmation for the status bar, if a copy has happened
    /// since the last refresh.
    pub fn copy_flash(&self) -> Option<&str> {
        self.copy_flash.as_deref()
    }

    /// Per-repo freshness from the latest successful sync, in roster order.
    /// Empty until the first sync completes.
    pub fn repo_health(&self) -> &[RepoHealth] {
        &self.repo_health
    }

    /// The repos whose latest refresh failed, so the hub shows an older export.
    pub fn stale_repos(&self) -> impl Iterator<Item = &RepoHealth> {
        self.repo_health.iter().filter(|health| health.is_stale())
    }

    /// Whether the sync-health panel is open.
    pub fn health_open(&self) -> bool {
        self.health.is_some()
    }

    /// The health panel's doctor state, when the panel is open.
    pub fn health_doctor(&self) -> Option<&DoctorState> {
        self.health.as_ref().map(|health| &health.doctor)
    }

    /// The health panel's requested scroll offset (0 when closed).
    pub fn health_scroll(&self) -> u16 {
        self.health.as_ref().map_or(0, |health| health.scroll)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bd::Issue;
    use std::path::Path;
    use std::time::{Duration, UNIX_EPOCH};

    fn row(repo: &str, id: &str, priority: i64) -> Row {
        Row {
            issue: Issue {
                id: id.to_string(),
                title: format!("title {id}"),
                status: "open".into(),
                priority,
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
            repo_id: Some(repo.to_string()),
            repo_name: repo.to_string(),
            attribution_generation: None,
        }
    }

    fn unattributed_row(repo_name: &str, id: &str, priority: i64) -> Row {
        let mut row = row(repo_name, id, priority);
        row.repo_id = None;
        row
    }

    fn attributed_row(repo_id: &str, repo_name: &str, id: &str, priority: i64) -> Row {
        let mut row = row(repo_name, id, priority);
        row.repo_id = Some(repo_id.to_string());
        row
    }

    fn snapshot(rows: Vec<Row>) -> Snapshot {
        Snapshot {
            rows,
            fetched_at: UNIX_EPOCH + Duration::from_secs(1_700_000_000),
        }
    }

    /// A successful refresh completion carrying `rows` and no warnings.
    fn completed(rows: Vec<Row>) -> Msg {
        Msg::RefreshCompleted {
            snapshot: Some(snapshot(rows)),
            warnings: Vec::new(),
        }
    }

    /// An app advanced to `List` with the given rows via a `RefreshCompleted`.
    fn app_with(rows: Vec<Row>) -> App {
        let mut app = App::new();
        app.reduce(completed(rows));
        app
    }

    #[test]
    fn restored_repo_view_filters_cache_before_the_first_frame() {
        let mut app = App::with_repo_view(RepoFilter::Only("repo-b".into()));

        assert_eq!(app.repo_view(), &RepoFilter::Only("repo-b".into()));
        app.hydrate_from_cache(snapshot(vec![
            row("repo-a", "a-1", 1),
            row("repo-b", "b-1", 2),
        ]));

        assert_eq!(ids(&app.filtered_rows()), vec!["b-1"]);
        assert!(app.is_stale(), "cache hydration keeps the launch guard");
    }

    #[test]
    fn repo_picker_opens_with_deterministic_choices_and_cancel_is_non_destructive() {
        let mut app = app_with(vec![
            row("zeta", "z-1", 1),
            row("Alpha", "a-1", 2),
            row("alpha", "a-2", 3),
            row("zeta", "z-2", 4),
        ]);
        app.reduce(Msg::SelectNext);
        let selected = app.selected_row().unwrap().issue.id.clone();

        assert_eq!(app.reduce(Msg::OpenRepoPicker), Vec::new());
        assert_eq!(
            app.repo_picker_choices(),
            Some(
                [
                    RepoFilter::All,
                    RepoFilter::Only("Alpha".into()),
                    RepoFilter::Only("alpha".into()),
                    RepoFilter::Only("zeta".into()),
                ]
                .as_slice()
            )
        );
        assert_eq!(app.repo_picker_cursor(), Some(0));
        assert_eq!(app.repo_view(), &RepoFilter::All);

        assert_eq!(app.reduce(Msg::RepoPickerNext), Vec::new());
        assert_eq!(app.reduce(Msg::Back), Vec::new());
        assert!(!app.repo_picker_open());
        assert_eq!(app.repo_view(), &RepoFilter::All);
        assert_eq!(app.selected_row().unwrap().issue.id, selected);
    }

    #[test]
    fn confirmed_repo_view_recomputes_ready_and_search_and_persists_once() {
        let mut app = app_with(vec![
            row("repo-a", "a-ready", 1),
            row("repo-b", "b-ready", 1),
        ]);
        let token = submit(&mut app, "work");
        app.reduce(Msg::SearchResults {
            token,
            rows: Ok(vec![
                row("repo-a", "a-search", 1),
                row("repo-b", "b-search", 1),
            ]),
        });
        app.reduce(Msg::SelectNext);

        assert_eq!(
            choose_repo(&mut app, RepoFilter::Only("repo-b".into())),
            vec![Effect::PersistRepoView(RepoFilter::Only("repo-b".into()))]
        );
        assert_eq!(ids(&app.filtered_rows()), vec!["b-search"]);
        assert_eq!(app.selection(), Some(0));

        app.reduce(Msg::Back);
        app.reduce(Msg::Back);
        assert_eq!(ids(&app.filtered_rows()), vec!["b-ready"]);
        assert_eq!(app.selection(), Some(0));

        assert_eq!(
            choose_repo(&mut app, RepoFilter::Only("repo-b".into())),
            Vec::new(),
            "confirming the active view performs no write"
        );
    }

    #[test]
    fn persisted_repo_identity_survives_result_dependent_display_labels() {
        let mut app = app_with(vec![
            attributed_row("ra", "api (ra)", "ra-ready", 1),
            attributed_row("rb", "api (rb)", "rb-ready", 1),
        ]);
        app.reduce(Msg::OpenRepoPicker);
        assert_eq!(
            app.repo_picker_choices(),
            Some(
                [
                    RepoFilter::All,
                    RepoFilter::Only("ra".into()),
                    RepoFilter::Only("rb".into()),
                ]
                .as_slice()
            ),
            "picker values use stable attribution identities"
        );
        assert_eq!(
            app.repo_picker_labels(),
            Some(
                [
                    "All repos".to_string(),
                    "api (ra)".to_string(),
                    "api (rb)".to_string(),
                ]
                .as_slice()
            ),
            "picker labels remain presentation-only"
        );
        app.reduce(Msg::Back);
        assert_eq!(
            choose_repo(&mut app, RepoFilter::Only("ra".into())),
            vec![Effect::PersistRepoView(RepoFilter::Only("ra".into()))],
            "the persisted value is the stable identity"
        );
        assert_eq!(
            ids(&app.filtered_rows()),
            vec!["ra-ready"],
            "the ready row matches the selected attribution identity"
        );

        let token = submit(&mut app, "work");
        app.reduce(Msg::SearchResults {
            token,
            rows: Ok(vec![attributed_row("ra", "api", "ra-search", 1)]),
        });
        assert_eq!(
            ids(&app.filtered_rows()),
            vec!["ra-search"],
            "the same repository remains selected when its search label differs"
        );
    }

    #[test]
    fn configured_unknown_prefix_is_distinct_from_unattributed_bucket() {
        let mut app = app_with(vec![
            attributed_row("unknown", "unknown", "unknown-1", 1),
            unattributed_row("unknown", "zz-unattributed", 1),
        ]);

        app.reduce(Msg::OpenRepoPicker);
        assert_eq!(
            app.repo_picker_choices().unwrap().len(),
            3,
            "all, the configured prefix, and the unattributed bucket are distinct"
        );
        assert!(
            app.repo_picker_choices()
                .unwrap()
                .contains(&RepoFilter::Unknown),
            "the unattributed bucket has its own tagged choice"
        );
        let labels = app.repo_picker_labels().unwrap();
        assert_ne!(
            labels[1], labels[2],
            "the configured prefix and unattributed bucket have distinct labels"
        );
        app.reduce(Msg::Back);

        choose_repo(&mut app, RepoFilter::Only("unknown".into()));
        assert_eq!(
            ids(&app.filtered_rows()),
            vec!["unknown-1"],
            "selecting the configured prefix excludes unattributed rows"
        );
    }

    #[test]
    fn picker_disambiguates_duplicate_labels_merged_from_ready_and_search() {
        let mut app = app_with(vec![attributed_row("ra", "api", "ra-ready", 1)]);
        let token = submit(&mut app, "work");
        app.reduce(Msg::SearchResults {
            token,
            rows: Ok(vec![attributed_row("rb", "api", "rb-search", 1)]),
        });

        app.reduce(Msg::OpenRepoPicker);

        assert_eq!(
            app.repo_picker_choices(),
            Some(
                [
                    RepoFilter::All,
                    RepoFilter::Only("ra".into()),
                    RepoFilter::Only("rb".into()),
                ]
                .as_slice()
            )
        );
        assert_eq!(
            app.repo_picker_labels(),
            Some(
                [
                    "All repos".to_string(),
                    "api (prefix ra)".to_string(),
                    "api (prefix rb)".to_string(),
                ]
                .as_slice()
            ),
            "identical cross-list labels include their stable identities"
        );
    }

    #[test]
    fn picker_disambiguation_cannot_create_a_second_label_collision() {
        let mut app = app_with(vec![
            attributed_row("ra", "api", "ra-ready", 1),
            attributed_row("rc", "api (ra)", "rc-ready", 1),
        ]);
        let token = submit(&mut app, "work");
        app.reduce(Msg::SearchResults {
            token,
            rows: Ok(vec![attributed_row("rb", "api", "rb-search", 1)]),
        });

        app.reduce(Msg::OpenRepoPicker);

        let labels = app.repo_picker_labels().expect("picker is open");
        let unique: std::collections::HashSet<&str> = labels.iter().map(String::as_str).collect();
        assert_eq!(
            unique.len(),
            labels.len(),
            "every distinct repository identity has a visually unique label"
        );
    }

    #[test]
    fn persistence_warning_tracks_only_the_current_repo_view() {
        let mut app = app_with(vec![row("repo-a", "a-1", 1), row("repo-b", "b-1", 1)]);
        choose_repo(&mut app, RepoFilter::Only("repo-b".into()));

        app.reduce(Msg::RepoViewPersisted {
            repo: RepoFilter::Only("repo-b".into()),
            result: Err("couldn't save repository view: denied".into()),
        });
        assert_eq!(
            app.persistence_warning(),
            Some("couldn't save repository view: denied")
        );

        choose_repo(&mut app, RepoFilter::All);
        app.reduce(Msg::RepoViewPersisted {
            repo: RepoFilter::Only("repo-b".into()),
            result: Err("stale failure".into()),
        });
        assert_eq!(
            app.persistence_warning(),
            Some("couldn't save repository view: denied"),
            "stale completion cannot replace the warning"
        );
        app.reduce(Msg::RepoViewPersisted {
            repo: RepoFilter::All,
            result: Ok(()),
        });
        assert_eq!(app.persistence_warning(), None);
    }

    #[test]
    fn confirming_unchanged_repo_retries_after_persistence_failure() {
        let mut app = app_with(vec![row("repo-a", "a-1", 1), row("repo-b", "b-1", 1)]);
        let repo_b = RepoFilter::Only("repo-b".into());
        choose_repo(&mut app, repo_b.clone());
        app.reduce(Msg::RepoViewPersisted {
            repo: repo_b.clone(),
            result: Err("couldn't save repository view: denied".into()),
        });

        assert_eq!(
            choose_repo(&mut app, repo_b.clone()),
            vec![Effect::PersistRepoView(repo_b.clone())],
            "confirming the unchanged choice retries the failed write"
        );
        assert_eq!(app.repo_view(), &repo_b);

        app.reduce(Msg::RepoViewPersisted {
            repo: repo_b,
            result: Ok(()),
        });
        assert_eq!(
            app.persistence_warning(),
            None,
            "a successful retry clears the warning"
        );
    }

    #[test]
    fn picker_keeps_stale_active_choice_and_freezes_during_refresh() {
        let mut app = App::with_repo_view(RepoFilter::Only("missing-repo".into()));
        app.hydrate_from_cache(snapshot(vec![row("repo-a", "a-1", 1)]));
        app.reduce(Msg::OpenRepoPicker);
        assert_eq!(
            app.repo_picker_choices(),
            Some(
                [
                    RepoFilter::All,
                    RepoFilter::Only("missing-repo".into()),
                    RepoFilter::Only("repo-a".into()),
                ]
                .as_slice()
            )
        );
        assert_eq!(app.repo_picker_cursor(), Some(1));

        app.reduce(completed(vec![row("repo-z", "z-1", 1)]));

        assert_eq!(
            app.repo_picker_choices(),
            Some(
                [
                    RepoFilter::All,
                    RepoFilter::Only("missing-repo".into()),
                    RepoFilter::Only("repo-a".into()),
                ]
                .as_slice()
            ),
            "background refresh does not mutate the open modal snapshot"
        );
        assert_eq!(app.repo_picker_cursor(), Some(1));
        assert_eq!(
            app.reduce(Msg::ConfirmRepoPicker),
            Vec::new(),
            "confirming a stale but already-active view is safe"
        );
    }

    fn ids(rows: &[&Row]) -> Vec<String> {
        rows.iter().map(|r| r.issue.id.clone()).collect()
    }

    fn choose_repo(app: &mut App, choice: RepoFilter) -> Vec<Effect> {
        app.reduce(Msg::OpenRepoPicker);
        let index = app
            .repo_picker_choices()
            .unwrap()
            .iter()
            .position(|candidate| candidate == &choice)
            .expect("repository picker contains requested choice");
        for _ in 0..app.repo_picker_cursor().unwrap() {
            app.reduce(Msg::RepoPickerPrev);
        }
        for _ in 0..index {
            app.reduce(Msg::RepoPickerNext);
        }
        app.reduce(Msg::ConfirmRepoPicker)
    }

    /// Representative native and structured `bd show` detail for `id`.
    fn detail(id: &str) -> String {
        format!("○ {id} [TASK] · title {id} [● P2 · OPEN]\n\nDESCRIPTION\n\n  a description")
    }

    fn detail_with_dependency(id: &str, dependency: &str) -> String {
        let mut detail = detail(id);
        detail.push_str(&format!("\n\nDEPENDENCIES\n  → {dependency}: blocker"));
        detail
    }

    /// Open the detail for the current selection, returning the request token from
    /// the emitted `FetchDetail` (so tests echo the right token in `DetailReady`).
    fn open(app: &mut App) -> u64 {
        match app.reduce(Msg::OpenDetail).as_slice() {
            [Effect::FetchDetail { token, .. }] => *token,
            other => panic!("expected one FetchDetail, got {other:?}"),
        }
    }

    /// Drive `OpenSearch → type each char → SubmitSearch` from the list, returning
    /// the search request token from the emitted `Effect::Search`.
    fn submit(app: &mut App, query: &str) -> u64 {
        app.reduce(Msg::OpenSearch);
        for c in query.chars() {
            app.reduce(Msg::SearchInput(c));
        }
        match app.reduce(Msg::SubmitSearch).as_slice() {
            [Effect::Search { token, query: q }] => {
                assert_eq!(q, query, "the effect carries the typed query");
                *token
            }
            other => panic!("expected one Search effect, got {other:?}"),
        }
    }

    #[test]
    fn enter_requests_detail() {
        let mut app = app_with(vec![row("ra", "ra-1", 1)]);
        assert_eq!(app.view_mode(), ViewMode::List);

        let effects = app.reduce(Msg::OpenDetail);
        assert_eq!(
            effects,
            vec![Effect::FetchDetail {
                id: "ra-1".into(),
                token: 1
            }]
        );
        assert_eq!(app.view_mode(), ViewMode::Detail);
        assert!(
            matches!(app.detail(), Some(DetailState::Loading { id }) if id == "ra-1"),
            "the pane is loading the selected id: {:?}",
            app.detail()
        );
    }

    #[test]
    fn cursor_movement_does_not_fetch() {
        // Browsing the list must make zero bd calls (no FetchDetail effect).
        let mut app = app_with(vec![row("ra", "ra-1", 1), row("ra", "ra-2", 1)]);
        assert_eq!(app.reduce(Msg::SelectNext), Vec::new());
        assert_eq!(app.reduce(Msg::SelectPrev), Vec::new());
        assert_eq!(app.view_mode(), ViewMode::List, "still browsing the list");
    }

    #[test]
    fn open_detail_noop_on_empty_list() {
        // No selected row: Enter cannot open a detail and emits no effect.
        let mut app = app_with(vec![]);
        assert_eq!(app.selected_row(), None);
        assert_eq!(app.reduce(Msg::OpenDetail), Vec::new());
        assert_eq!(app.view_mode(), ViewMode::List);
        assert!(app.detail().is_none());
    }

    #[test]
    fn detail_ready_stores_for_matching_id() {
        let mut app = app_with(vec![row("ra", "ra-1", 1)]);
        let token = open(&mut app);

        app.reduce(Msg::DetailReady {
            token,
            detail: Ok(detail_with_dependency("ra-1", "ra-z70")),
        });
        match app.detail() {
            Some(DetailState::Loaded(detail)) => {
                assert_eq!(detail.issue.id, "ra-1");
                assert!(detail.output.contains("ra-z70"));
            }
            other => panic!("expected Loaded, got {other:?}"),
        }
    }

    #[test]
    fn stale_detail_response_is_dropped() {
        let mut app = app_with(vec![row("ra", "ra-1", 1), row("ra", "ra-2", 1)]);
        let token = open(&mut app); // bound to ra-1

        // A response carrying a token that is not the current request is dropped.
        app.reduce(Msg::DetailReady {
            token: token + 99,
            detail: Ok(detail("ra-1")),
        });
        assert!(
            matches!(app.detail(), Some(DetailState::Loading { id }) if id == "ra-1"),
            "a mismatched-token response is dropped: {:?}",
            app.detail()
        );

        // The pane's own request still completes.
        app.reduce(Msg::DetailReady {
            token,
            detail: Ok(detail("ra-1")),
        });
        assert!(matches!(app.detail(), Some(DetailState::Loaded(_))));
    }

    #[test]
    fn same_id_reopen_drops_earlier_response() {
        // Open ra-1, Esc, reopen ra-1: the two fetches share an id but not a
        // token, so the first (slower) worker's late response must not overwrite
        // the pane the second request owns.
        let mut app = app_with(vec![row("ra", "ra-1", 1)]);
        let first = open(&mut app);
        app.reduce(Msg::Back);
        let second = open(&mut app);
        assert_ne!(first, second, "each open gets a fresh token");

        // The first request answers late (after the reopen): dropped.
        app.reduce(Msg::DetailReady {
            token: first,
            detail: Err("stale error".into()),
        });
        assert!(
            matches!(app.detail(), Some(DetailState::Loading { .. })),
            "the earlier request's late response is dropped: {:?}",
            app.detail()
        );

        // The second (current) request lands and is shown.
        app.reduce(Msg::DetailReady {
            token: second,
            detail: Ok(detail("ra-1")),
        });
        assert!(matches!(app.detail(), Some(DetailState::Loaded(_))));
    }

    #[test]
    fn detail_fetch_error_shows_message() {
        let mut app = app_with(vec![row("ra", "ra-1", 1)]);
        let token = open(&mut app);

        app.reduce(Msg::DetailReady {
            token,
            detail: Err("bd show failed: boom".into()),
        });
        match app.detail() {
            Some(DetailState::Error { id, message }) => {
                assert_eq!(id, "ra-1");
                assert!(message.contains("boom"), "message surfaced: {message}");
            }
            other => panic!("expected Error, got {other:?}"),
        }
        assert_eq!(app.view_mode(), ViewMode::Detail, "pane still open");
        assert_eq!(app.rows().len(), 1, "the list is intact behind the pane");
    }

    #[test]
    fn esc_returns_to_list() {
        let mut app = app_with(vec![row("ra", "ra-1", 1), row("ra", "ra-2", 1)]);
        app.reduce(Msg::SelectNext); // selection = 1 -> ra-2
        assert_eq!(app.selection(), Some(1));
        let token = open(&mut app);
        app.reduce(Msg::DetailReady {
            token,
            detail: Ok(detail("ra-2")),
        });

        app.reduce(Msg::Back);
        assert_eq!(app.view_mode(), ViewMode::List);
        assert!(app.detail().is_none());
        assert_eq!(
            app.selection(),
            Some(1),
            "selection preserved across detail"
        );
    }

    #[test]
    fn filters_inert_while_detail_open() {
        // With the pane open, f/p are inert — the visible row set behind the
        // pane must not shift under the reader.
        let mut app = app_with(vec![row("ra", "ra-1", 1), row("ra", "ra-2", 1)]);
        app.reduce(Msg::OpenDetail); // Detail, bound to ra-1, selection 0

        app.reduce(Msg::OpenRepoPicker);
        app.reduce(Msg::TogglePriorityFilter);
        assert!(!app.repo_picker_open(), "picker is inert in detail");
        assert_eq!(app.repo_view(), &RepoFilter::All, "filters frozen");
        assert_eq!(app.selection(), Some(0), "selection untouched by filters");

        app.reduce(Msg::Back);
        assert_eq!(app.selection(), Some(0), "returns to the original row");
    }

    #[test]
    fn detail_nav_moves_through_beads() {
        // With the pane open, j/k move the selection through the list behind it
        // and refetch the pane for each newly selected bead.
        let mut app = app_with(vec![row("ra", "ra-1", 1), row("ra", "ra-2", 1)]);
        let token = open(&mut app); // pane on ra-1
        app.reduce(Msg::DetailReady {
            token,
            detail: Ok(detail("ra-1")),
        });

        let effects = app.reduce(Msg::SelectNext);
        assert_eq!(
            app.selection(),
            Some(1),
            "selection follows j under the pane"
        );
        match effects.as_slice() {
            [Effect::FetchDetail { id, token: t }] => {
                assert_eq!(id, "ra-2", "the pane fetches the newly selected bead");
                assert!(*t > token, "a fresh token supersedes the old fetch");
            }
            other => panic!("expected one FetchDetail, got {other:?}"),
        }
        assert!(
            matches!(app.detail(), Some(DetailState::Loading { id }) if id == "ra-2"),
            "the pane reloads for the newly selected bead: {:?}",
            app.detail()
        );
        assert_eq!(app.view_mode(), ViewMode::Detail, "the pane stays open");

        // k moves back up and refetches ra-1.
        match app.reduce(Msg::SelectPrev).as_slice() {
            [Effect::FetchDetail { id, .. }] => assert_eq!(id, "ra-1"),
            other => panic!("expected one FetchDetail, got {other:?}"),
        }

        // Clamped at the first row: no movement, no duplicate fetch.
        assert_eq!(app.reduce(Msg::SelectPrev), Vec::new());
        assert_eq!(app.selection(), Some(0));
    }

    #[test]
    fn detail_nav_in_search_moves_results_not_ready() {
        // A detail opened from a search result navigates the results list; the
        // hidden ready selection is untouched, so leaving search restores it.
        let mut app = app_with(vec![row("ra", "ra-1", 1), row("ra", "ra-2", 1)]);
        app.reduce(Msg::SelectNext); // ready selection -> ra-2
        let token = submit(&mut app, "foo");
        app.reduce(Msg::SearchResults {
            token,
            rows: Ok(vec![row("mc", "mc-1", 1), row("mc", "mc-2", 1)]),
        });
        open(&mut app); // detail on mc-1

        match app.reduce(Msg::SelectNext).as_slice() {
            [Effect::FetchDetail { id, .. }] => assert_eq!(id, "mc-2"),
            other => panic!("expected one FetchDetail, got {other:?}"),
        }
        assert_eq!(app.selection(), Some(1), "the results selection moved");

        app.reduce(Msg::Back); // Detail -> Search results
        app.reduce(Msg::Back); // Results -> Editing
        app.reduce(Msg::Back); // Editing -> List
        assert_eq!(
            app.selected_row().map(|r| r.issue.id.as_str()),
            Some("ra-2"),
            "the ready selection survives detail navigation under a search"
        );
    }

    #[test]
    fn detail_scroll_moves_and_resets() {
        // In Detail mode J/K scroll the pane; the offset resets on open, close,
        // and whenever j/k move the pane to another bead.
        let mut app = app_with(vec![row("ra", "ra-1", 1), row("ra", "ra-2", 1)]);
        open(&mut app);
        assert_eq!(app.detail_scroll(), 0, "opens at the top");

        app.reduce(Msg::DetailScrollDown);
        app.reduce(Msg::DetailScrollDown);
        assert_eq!(app.detail_scroll(), 2, "J scrolls down");
        app.reduce(Msg::DetailScrollUp);
        assert_eq!(app.detail_scroll(), 1, "K scrolls up");
        // Saturates at the top rather than wrapping.
        app.reduce(Msg::DetailScrollUp);
        app.reduce(Msg::DetailScrollUp);
        assert_eq!(app.detail_scroll(), 0, "clamps at the top");

        app.reduce(Msg::DetailPageDown);
        assert_eq!(app.detail_scroll(), 10, "PageDown advances one page");
        app.reduce(Msg::DetailPageUp);
        assert_eq!(app.detail_scroll(), 0, "PageUp returns one page");

        app.reduce(Msg::DetailScrollDown);
        app.reduce(Msg::SelectNext); // move the pane to ra-2
        assert_eq!(app.detail_scroll(), 0, "reset when moving to another bead");

        app.reduce(Msg::DetailScrollDown);
        app.reduce(Msg::Back);
        assert_eq!(app.detail_scroll(), 0, "reset on close");
        open(&mut app);
        assert_eq!(app.detail_scroll(), 0, "reset on reopen");

        // Outside the pane, the scroll keys are inert.
        app.reduce(Msg::Back);
        app.reduce(Msg::DetailScrollDown);
        app.reduce(Msg::DetailPageDown);
        assert_eq!(app.detail_scroll(), 0, "no pane, no scroll");
        assert_eq!(app.selection(), Some(1), "and the selection does not move");
    }

    #[test]
    fn detail_scroll_bounds_clamp_partial_page_before_page_up() {
        let mut app = app_with(vec![row("ra", "ra-1", 1)]);
        open(&mut app);

        app.reduce(Msg::DetailPageDown);
        app.reduce(Msg::DetailPageDown);
        assert_eq!(app.detail_scroll(), 20, "requested offset may overshoot");

        // Rendering a partial final page reports the visible offset back to the
        // reducer. PageUp must then move from that visible row, not from the
        // stale overshoot retained before rendering.
        app.reduce(Msg::DetailScrollBounds { max_scroll: 11 });
        assert_eq!(app.detail_scroll(), 11, "stored offset matches the view");
        app.reduce(Msg::DetailPageUp);
        assert_eq!(app.detail_scroll(), 1, "moves a full page from row 11");
    }

    #[test]
    fn refresh_under_detail_keeps_pane() {
        // A background refresh must not slam the open detail pane shut.
        let mut app = app_with(vec![row("ra", "ra-1", 1)]);
        let token = open(&mut app);
        app.reduce(Msg::DetailReady {
            token,
            detail: Ok(detail("ra-1")),
        });
        assert_eq!(app.view_mode(), ViewMode::Detail);

        app.reduce(completed(vec![row("ra", "ra-1", 1), row("ra", "ra-9", 2)]));
        assert_eq!(
            app.view_mode(),
            ViewMode::Detail,
            "the pane stays open across a refresh"
        );
        assert!(app.detail().is_some());
        assert_eq!(app.rows().len(), 2, "rows updated underneath the pane");
    }

    #[test]
    fn refresh_under_detail_preserves_opened_row() {
        // Open the detail on ra-1 (selection 0), then a refresh reorders the rows
        // so ra-1 moves. The selection must follow the opened issue, so Esc
        // returns to ra-1 rather than whatever now sits at index 0.
        let mut app = app_with(vec![row("ra", "ra-1", 1), row("ra", "ra-2", 2)]);
        let token = open(&mut app);
        app.reduce(Msg::DetailReady {
            token,
            detail: Ok(detail("ra-1")),
        });

        app.reduce(completed(vec![
            row("ra", "ra-0", 0), // new row jumps to the front
            row("ra", "ra-2", 2),
            row("ra", "ra-1", 1), // ra-1 now at index 2
        ]));

        app.reduce(Msg::Back);
        assert_eq!(
            app.selected_row().map(|r| r.issue.id.as_str()),
            Some("ra-1"),
            "selection follows the opened issue across the refresh re-sort"
        );
    }

    #[test]
    fn refresh_under_detail_falls_back_when_opened_row_gone() {
        // If the opened issue vanishes from the refreshed rows, the selection
        // falls back to the clamped index (no panic, still a valid selection).
        let mut app = app_with(vec![row("ra", "ra-1", 1), row("ra", "ra-2", 2)]);
        let token = open(&mut app);
        app.reduce(Msg::DetailReady {
            token,
            detail: Ok(detail("ra-1")),
        });

        app.reduce(completed(vec![row("ra", "ra-2", 2), row("ra", "ra-3", 2)]));
        app.reduce(Msg::Back);
        assert!(app.selected_row().is_some(), "a valid selection remains");
    }

    // ---- Cross-repo search (Slice 11) ----

    #[test]
    fn slash_opens_search_input() {
        let mut app = app_with(vec![row("ra", "ra-1", 1), row("ra", "ra-2", 1)]);
        app.reduce(Msg::OpenSearch);
        assert_eq!(app.view_mode(), ViewMode::Search);
        assert!(app.search_editing(), "the query input is focused");
        assert_eq!(app.search_query(), Some(""));
        // While editing, navigation keys don't move a selection (they are text
        // now): the active (empty) search list has no selection.
        app.reduce(Msg::SelectNext);
        assert_eq!(app.selection(), None);
    }

    #[test]
    fn typing_edits_query() {
        let mut app = app_with(vec![row("ra", "ra-1", 1)]);
        app.reduce(Msg::OpenSearch);
        app.reduce(Msg::SearchInput('f'));
        app.reduce(Msg::SearchInput('o'));
        app.reduce(Msg::SearchInput('o'));
        assert_eq!(app.search_query(), Some("foo"));
        app.reduce(Msg::SearchBackspace);
        assert_eq!(app.search_query(), Some("fo"));
        // Backspace past empty is a safe no-op.
        app.reduce(Msg::SearchBackspace);
        app.reduce(Msg::SearchBackspace);
        app.reduce(Msg::SearchBackspace);
        assert_eq!(app.search_query(), Some(""));
    }

    #[test]
    fn enter_submits_search_effect() {
        let mut app = app_with(vec![row("ra", "ra-1", 1)]);
        app.reduce(Msg::OpenSearch);
        for c in "foo".chars() {
            app.reduce(Msg::SearchInput(c));
        }
        let effects = app.reduce(Msg::SubmitSearch);
        assert_eq!(
            effects,
            vec![Effect::Search {
                query: "foo".into(),
                token: 1
            }]
        );
        assert_eq!(app.search_phase(), Some(&SearchPhase::Loading));
    }

    #[test]
    fn empty_query_no_ops() {
        let mut app = app_with(vec![row("ra", "ra-1", 1)]);
        app.reduce(Msg::OpenSearch);
        assert_eq!(
            app.reduce(Msg::SubmitSearch),
            Vec::new(),
            "an empty query submits nothing"
        );
        assert!(app.search_editing(), "and stays in the editor");
        // Whitespace-only is also treated as empty.
        app.reduce(Msg::SearchInput(' '));
        assert_eq!(app.reduce(Msg::SubmitSearch), Vec::new());
        assert!(app.search_editing());
    }

    #[test]
    fn results_replace_rows_with_attribution() {
        let mut app = app_with(vec![row("ra", "ra-1", 1)]);
        let token = submit(&mut app, "foo");

        // The worker delivers already-attributed rows (its repo_name carried).
        app.reduce(Msg::SearchResults {
            token,
            rows: Ok(vec![
                row("megaclock", "mc-1", 0),
                row("session-tui", "ra-9", 2),
            ]),
        });
        assert_eq!(app.view_mode(), ViewMode::Search);
        assert_eq!(app.search_phase(), Some(&SearchPhase::Results));
        assert_eq!(ids(&app.filtered_rows()), vec!["mc-1", "ra-9"]);
        assert_eq!(
            app.filtered_rows()[0].repo_name,
            "megaclock",
            "attribution flows through the same PrefixMap path as ready rows"
        );
        assert_eq!(app.selection(), Some(0));
    }

    #[test]
    fn stale_search_results_dropped() {
        let mut app = app_with(vec![row("ra", "ra-1", 1)]);
        let first = submit(&mut app, "foo");

        // A non-current token is dropped: the phase stays Loading.
        app.reduce(Msg::SearchResults {
            token: first + 99,
            rows: Ok(vec![row("ra", "ra-1", 1)]),
        });
        assert_eq!(app.search_phase(), Some(&SearchPhase::Loading));

        // Re-submit a new query: a fresh token supersedes the first.
        app.reduce(Msg::Back); // Loading -> Editing (query preserved)
        for c in "bar".chars() {
            app.reduce(Msg::SearchInput(c));
        }
        let second = match app.reduce(Msg::SubmitSearch).as_slice() {
            [Effect::Search { token, .. }] => *token,
            other => panic!("expected Search, got {other:?}"),
        };
        assert_ne!(first, second, "each submit gets a fresh token");

        // The first request answers late (after the re-submit): dropped.
        app.reduce(Msg::SearchResults {
            token: first,
            rows: Ok(vec![row("ra", "ra-1", 1)]),
        });
        assert_eq!(
            app.search_phase(),
            Some(&SearchPhase::Loading),
            "the superseded query's late reply is dropped"
        );
        // The current request lands.
        app.reduce(Msg::SearchResults {
            token: second,
            rows: Ok(vec![row("ra", "ra-2", 2)]),
        });
        assert_eq!(app.search_phase(), Some(&SearchPhase::Results));
        assert_eq!(ids(&app.filtered_rows()), vec!["ra-2"]);
    }

    #[test]
    fn esc_restores_ready_list() {
        // A ready list with an active filter and a non-zero selection.
        let mut app = app_with(vec![
            row("repo-a", "ra-1", 1),
            row("repo-a", "ra-2", 1),
            row("repo-b", "rb-1", 1),
        ]);
        choose_repo(&mut app, RepoFilter::Only("repo-a".into()));
        app.reduce(Msg::SelectNext); // selection 1 -> ra-2
        let ready_filter = app.filter().clone();
        let ready_selection = app.selection();
        let ready_ids = ids(&app.filtered_rows());
        assert_eq!(ready_ids, vec!["ra-1", "ra-2"]);
        assert_eq!(ready_selection, Some(1));

        let token = submit(&mut app, "foo");
        app.reduce(Msg::SearchResults {
            token,
            rows: Ok(vec![row("megaclock", "mc-1", 0)]),
        });

        // Esc from the results returns to editing, query preserved (refine path).
        app.reduce(Msg::Back);
        assert_eq!(app.view_mode(), ViewMode::Search);
        assert_eq!(app.search_phase(), Some(&SearchPhase::Editing));
        assert_eq!(
            app.search_query(),
            Some("foo"),
            "query preserved for refining"
        );

        // Esc from the editor exits search and restores the ready list exactly.
        app.reduce(Msg::Back);
        assert_eq!(app.view_mode(), ViewMode::List);
        assert!(app.search_query().is_none(), "search state cleared");
        assert_eq!(app.filter(), &ready_filter, "ready filter restored");
        assert_eq!(app.selection(), ready_selection, "ready selection restored");
        assert_eq!(ids(&app.filtered_rows()), ready_ids, "ready rows restored");
    }

    #[test]
    fn search_detail_and_back() {
        let mut app = app_with(vec![row("ra", "ra-1", 1)]);
        let token = submit(&mut app, "foo");
        app.reduce(Msg::SearchResults {
            token,
            rows: Ok(vec![
                row("megaclock", "mc-1", 0),
                row("megaclock", "mc-2", 1),
            ]),
        });
        app.reduce(Msg::SelectNext); // select mc-2 in the results
        assert_eq!(
            app.selected_row().map(|r| r.issue.id.as_str()),
            Some("mc-2")
        );

        // Enter opens the detail on the selected search result.
        let effects = app.reduce(Msg::OpenDetail);
        let dtoken = match effects.as_slice() {
            [Effect::FetchDetail { id, token }] => {
                assert_eq!(id, "mc-2");
                *token
            }
            other => panic!("expected one FetchDetail, got {other:?}"),
        };
        assert_eq!(app.view_mode(), ViewMode::Detail);

        app.reduce(Msg::DetailReady {
            token: dtoken,
            detail: Ok(detail("mc-2")),
        });
        // Back returns to the search results (not the ready list), selection intact.
        app.reduce(Msg::Back);
        assert_eq!(app.view_mode(), ViewMode::Search);
        assert_eq!(app.search_phase(), Some(&SearchPhase::Results));
        assert_eq!(
            app.selected_row().map(|r| r.issue.id.as_str()),
            Some("mc-2"),
            "the results selection survives the detail round-trip"
        );
    }

    #[test]
    fn search_error_shows_message() {
        let mut app = app_with(vec![row("ra", "ra-1", 1)]);
        let token = submit(&mut app, "foo");
        app.reduce(Msg::SearchResults {
            token,
            rows: Err("bd search failed: boom".into()),
        });
        match app.search_phase() {
            Some(SearchPhase::Error(msg)) => assert!(msg.contains("boom"), "message: {msg}"),
            other => panic!("expected Error, got {other:?}"),
        }
        // The ready list is intact behind the search.
        app.reduce(Msg::Back); // Error -> Editing
        app.reduce(Msg::Back); // Editing -> List
        assert_eq!(ids(&app.filtered_rows()), vec!["ra-1"]);
    }

    // ---- Copy-context (Slice 12) ----

    /// The single `Effect::Copy` from a copy message (row, markdown, token),
    /// panicking otherwise.
    fn copy(app: &mut App, msg: Msg) -> (Row, bool, u64) {
        match app.reduce(msg).as_slice() {
            [
                Effect::Copy {
                    row,
                    markdown,
                    token,
                },
            ] => ((**row).clone(), *markdown, *token),
            other => panic!("expected one Copy effect, got {other:?}"),
        }
    }

    #[test]
    fn copy_context_emits_effect() {
        let mut app = app_with(vec![row("megaclock", "mc-abc", 1)]);
        let (r, markdown, token) = copy(&mut app, Msg::CopyContext);
        assert_eq!(r.issue.id, "mc-abc", "the selected row is carried");
        assert_eq!(r.repo_name, "megaclock");
        assert!(!markdown, "y copies the command form");
        assert_eq!(token, 1, "the first copy is generation 1");
    }

    #[test]
    fn copy_markdown_emits_effect() {
        let mut app = app_with(vec![row("megaclock", "mc-abc", 1)]);
        let (r, markdown, _) = copy(&mut app, Msg::CopyMarkdown);
        assert_eq!(r.issue.id, "mc-abc");
        assert!(markdown, "Y copies the markdown form");
    }

    #[test]
    fn copy_in_search_results_emits_effect() {
        let mut app = app_with(vec![row("ra", "ra-1", 1)]);
        let token = submit(&mut app, "foo");
        app.reduce(Msg::SearchResults {
            token,
            rows: Ok(vec![
                row("megaclock", "mc-1", 0),
                row("session-tui", "st-9", 2),
            ]),
        });
        app.reduce(Msg::SelectNext); // select st-9 in the results
        let (r, _, _) = copy(&mut app, Msg::CopyContext);
        assert_eq!(r.issue.id, "st-9", "copies the selected search result");
    }

    #[test]
    fn copy_in_detail_emits_effect() {
        let mut opened = row("megaclock", "mc-abc", 1);
        opened.issue.description = Some("description from selected row".into());
        let mut app = app_with(vec![opened]);
        let token = open(&mut app);
        app.reduce(Msg::DetailReady {
            token,
            detail: Ok(detail("mc-abc")),
        });
        assert_eq!(app.view_mode(), ViewMode::Detail);
        let (r, _, _) = copy(&mut app, Msg::CopyContext);
        assert_eq!(r.issue.id, "mc-abc", "copies the opened issue from Detail");
        assert_eq!(
            r.issue.description.as_deref(),
            Some("description from selected row"),
            "detail copy retains structured data from the selected row"
        );
    }

    #[test]
    fn copy_in_detail_uses_pinned_issue_after_refresh() {
        // Open detail on ra-1, then a refresh removes ra-1 and the ready selection
        // clamps to ra-2. `y` must still copy the pinned ra-1 (what the pane
        // shows), not the re-clamped selection.
        let old_generation = crate::refresh::AttributionGeneration::new(1);
        let new_generation = crate::refresh::AttributionGeneration::new(2);
        let mut first = row("ra", "ra-1", 1);
        first.attribution_generation = Some(old_generation);
        let mut app = app_with(vec![first, row("ra", "ra-2", 2)]);
        let token = open(&mut app);
        app.reduce(Msg::DetailReady {
            token,
            detail: Ok(detail("ra-1")),
        });
        let mut refreshed = row("ra", "ra-2", 2);
        refreshed.attribution_generation = Some(new_generation);
        app.reduce(completed(vec![refreshed, row("ra", "ra-3", 2)]));
        assert_eq!(app.view_mode(), ViewMode::Detail, "pane stays open");

        let (r, _, _) = copy(&mut app, Msg::CopyContext);
        assert_eq!(
            r.issue.id, "ra-1",
            "copies the pinned detail issue, not the re-clamped list selection"
        );
        assert_eq!(
            r.repo_name, "ra",
            "the opened row's repo attribution survives the refresh, not 'unknown'"
        );
        assert_eq!(
            r.attribution_generation,
            Some(old_generation),
            "the pinned detail row keeps the immutable map that attributed it"
        );
    }

    #[test]
    fn copy_noop_while_search_loading() {
        // A resubmitted query is Loading with the previous results still retained
        // (but invisible). `y` must not copy that stale, hidden result.
        let mut app = app_with(vec![row("ra", "ra-1", 1)]);
        let token = submit(&mut app, "foo");
        app.reduce(Msg::SearchResults {
            token,
            rows: Ok(vec![row("megaclock", "mc-1", 0)]),
        });
        app.reduce(Msg::Back); // Results -> Editing (results retained internally)
        app.reduce(Msg::SearchInput('b'));
        app.reduce(Msg::SubmitSearch); // -> Loading
        assert_eq!(app.search_phase(), Some(&SearchPhase::Loading));

        assert_eq!(
            app.reduce(Msg::CopyContext),
            Vec::new(),
            "no copy of a retained, invisible result while a new search loads"
        );
    }

    #[test]
    fn copy_no_selection_noops() {
        let mut app = app_with(vec![]);
        assert_eq!(app.selected_row(), None);
        assert_eq!(
            app.reduce(Msg::CopyContext),
            Vec::new(),
            "no selection copies nothing"
        );
        assert!(app.copy_flash().is_none(), "and shows no confirmation");
    }

    #[test]
    fn copied_sets_flash_and_writes() {
        let mut app = app_with(vec![row("megaclock", "mc-abc", 1)]);
        let (_, _, token) = copy(&mut app, Msg::CopyContext);
        let effects = app.reduce(Msg::Copied {
            token,
            payload: "cd /dev/megaclock && bd show mc-abc".into(),
            summary: "copied cd mc-abc".into(),
        });
        assert_eq!(
            effects,
            vec![Effect::WriteClipboard(
                "cd /dev/megaclock && bd show mc-abc".into()
            )],
            "the payload is handed back for the UI-thread tty write"
        );
        assert_eq!(app.copy_flash(), Some("copied cd mc-abc"));
    }

    #[test]
    fn stale_copy_token_dropped() {
        // A defensive guard: a `Copied` whose token is not the in-flight copy's is
        // dropped (writes nothing, keeps the flight open) so only the real result
        // is applied.
        let mut app = app_with(vec![row("megaclock", "mc-abc", 1)]);
        let (_, _, token) = copy(&mut app, Msg::CopyContext);

        let effects = app.reduce(Msg::Copied {
            token: token + 99,
            payload: "wrong".into(),
            summary: "wrong".into(),
        });
        assert_eq!(
            effects,
            Vec::new(),
            "a mismatched-token result writes nothing"
        );
        assert!(app.copy_flash().is_none(), "and shows no confirmation");

        // The real result still lands.
        let effects = app.reduce(Msg::Copied {
            token,
            payload: "right".into(),
            summary: "copied mc-abc".into(),
        });
        assert_eq!(effects, vec![Effect::WriteClipboard("right".into())]);
        assert_eq!(app.copy_flash(), Some("copied mc-abc"));
    }

    #[test]
    fn copy_coalesces_while_in_flight() {
        // Holding y/Y (or mashing it) while a copy resolves must not spawn a second
        // worker: the request coalesces and fires once the first completes, for the
        // then-current selection — so the last press wins with at most one worker
        // in flight at a time.
        let mut app = app_with(vec![
            row("megaclock", "mc-a", 1),
            row("session-tui", "st-b", 1),
        ]);
        let (_, _, first) = copy(&mut app, Msg::CopyContext); // worker for mc-a, in flight

        // A second copy while in flight: no new Copy effect (coalesced), and the
        // target (st-b) is captured now.
        app.reduce(Msg::SelectNext); // selecting st-b
        assert_eq!(
            app.reduce(Msg::CopyContext),
            Vec::new(),
            "a copy while one is in flight spawns no second worker"
        );
        // Moving the cursor after the coalesced press must NOT redirect the copy.
        app.reduce(Msg::SelectPrev); // back to mc-a

        // The first completes: writes its payload AND fires the copy captured at
        // press time (st-b), not the now-current selection (mc-a).
        let effects = app.reduce(Msg::Copied {
            token: first,
            payload: "cd a && bd show mc-a".into(),
            summary: "copied mc-a".into(),
        });
        let second = match effects.as_slice() {
            [
                Effect::WriteClipboard(p),
                Effect::Copy {
                    row,
                    markdown: false,
                    token,
                },
            ] => {
                assert_eq!(p, "cd a && bd show mc-a", "the first copy still writes");
                assert_eq!(
                    row.issue.id, "st-b",
                    "the coalesced copy targets the row selected when it was pressed"
                );
                assert_ne!(*token, first, "the coalesced copy is a fresh generation");
                *token
            }
            other => panic!("expected write + coalesced copy, got {other:?}"),
        };

        // The coalesced copy completes and is shown; nothing further is pending.
        let effects = app.reduce(Msg::Copied {
            token: second,
            payload: "cd b && bd show st-b".into(),
            summary: "copied st-b".into(),
        });
        assert_eq!(
            effects,
            vec![Effect::WriteClipboard("cd b && bd show st-b".into())],
            "no further coalesced copy remains"
        );
        assert_eq!(app.copy_flash(), Some("copied st-b"));
    }

    #[test]
    fn no_pending_copy_from_a_no_source_state() {
        // A copy is in flight; the user opens search and submits (Loading phase,
        // where copy is a no-op). Pressing `y` there must not queue a copy that
        // later fires against an arriving result.
        let mut app = app_with(vec![row("megaclock", "mc-abc", 1)]);
        let (_, _, first) = copy(&mut app, Msg::CopyContext); // in flight

        let stoken = submit(&mut app, "foo"); // -> Search, Loading
        assert_eq!(app.search_phase(), Some(&SearchPhase::Loading));
        assert_eq!(
            app.reduce(Msg::CopyContext),
            Vec::new(),
            "copy in the Loading phase is a no-op and queues nothing"
        );

        // The in-flight copy completes: no pending copy was captured, so only the
        // write happens (no stray Copy effect).
        let effects = app.reduce(Msg::Copied {
            token: first,
            payload: "cd a && bd show mc-abc".into(),
            summary: "copied mc-abc".into(),
        });
        assert_eq!(
            effects,
            vec![Effect::WriteClipboard("cd a && bd show mc-abc".into())],
            "nothing was queued during the no-source Loading phase"
        );

        // Results arrive; the earlier `y` must not have copied one.
        app.reduce(Msg::SearchResults {
            token: stoken,
            rows: Ok(vec![row("session-tui", "st-9", 1)]),
        });
        assert_eq!(
            app.copy_flash(),
            Some("copied mc-abc"),
            "no phantom copy fired"
        );
    }

    #[test]
    fn copy_flash_clears_on_refresh() {
        let mut app = app_with(vec![row("megaclock", "mc-abc", 1)]);
        let (_, _, token) = copy(&mut app, Msg::CopyContext);
        app.reduce(Msg::Copied {
            token,
            payload: "x".into(),
            summary: "copied cd mc-abc".into(),
        });
        assert!(app.copy_flash().is_some());
        app.reduce(completed(vec![row("megaclock", "mc-abc", 1)]));
        assert!(
            app.copy_flash().is_none(),
            "a fresh refresh cycle clears the stale confirmation"
        );
    }

    #[test]
    fn refresh_under_search_updates_ready_not_view() {
        let mut app = app_with(vec![row("ra", "ra-1", 1)]);
        let token = submit(&mut app, "foo");
        app.reduce(Msg::SearchResults {
            token,
            rows: Ok(vec![row("megaclock", "mc-1", 0)]),
        });

        // A background refresh lands under the search: it updates the ready list
        // but must not change the shown results or eject the user from search.
        app.reduce(completed(vec![row("ra", "ra-1", 1), row("ra", "ra-9", 2)]));
        assert_eq!(app.view_mode(), ViewMode::Search);
        assert_eq!(
            ids(&app.filtered_rows()),
            vec!["mc-1"],
            "the visible search results are unchanged by the refresh"
        );

        // Leaving search reveals the refreshed ready list.
        app.reduce(Msg::Back); // Results -> Editing
        app.reduce(Msg::Back); // Editing -> List
        assert_eq!(
            ids(&app.filtered_rows()),
            vec!["ra-1", "ra-9"],
            "the ready list reflects the refresh that landed during search"
        );
    }

    #[test]
    fn refresh_under_search_detail_preserves_ready_selection() {
        // A detail opened from a search result must not hijack the hidden ready
        // selection when a refresh lands — even when the searched id is ALSO a
        // ready row. Otherwise backing all the way out lands on the searched
        // issue instead of the ready row selected before search.
        let mut app = app_with(vec![row("ra", "ra-1", 1), row("ra", "ra-2", 1)]);
        assert_eq!(
            app.selected_row().map(|r| r.issue.id.as_str()),
            Some("ra-1"),
            "ready starts selected on ra-1"
        );

        // Search returns ra-2 (which is also a ready row); open its detail.
        let token = submit(&mut app, "foo");
        app.reduce(Msg::SearchResults {
            token,
            rows: Ok(vec![row("ra", "ra-2", 1)]),
        });
        let dtoken = match app.reduce(Msg::OpenDetail).as_slice() {
            [Effect::FetchDetail { id, token }] => {
                assert_eq!(id, "ra-2");
                *token
            }
            other => panic!("expected one FetchDetail, got {other:?}"),
        };
        app.reduce(Msg::DetailReady {
            token: dtoken,
            detail: Ok(detail("ra-2")),
        });

        // A refresh lands under the search-opened detail (same ready rows).
        app.reduce(completed(vec![row("ra", "ra-1", 1), row("ra", "ra-2", 1)]));

        // Back out fully: detail -> results -> editing -> list.
        app.reduce(Msg::Back);
        app.reduce(Msg::Back);
        app.reduce(Msg::Back);
        assert_eq!(app.view_mode(), ViewMode::List);
        assert_eq!(
            app.selected_row().map(|r| r.issue.id.as_str()),
            Some("ra-1"),
            "the ready selection is untouched by the search-opened detail"
        );
    }

    #[test]
    fn starts_in_loading_then_shows_rows() {
        let mut app = App::new();
        assert_eq!(app.view_mode(), ViewMode::Loading);
        assert!(app.rows().is_empty());
        assert_eq!(app.selection(), None);
        assert!(!app.is_done());

        // A refresh begins before any data: still loading.
        app.reduce(Msg::RefreshStarted);
        assert_eq!(app.view_mode(), ViewMode::Loading, "no rows yet -> Loading");

        // First snapshot: rows appear, list shown, selection at the top.
        app.reduce(completed(vec![row("ra", "ra-1", 1), row("ra", "ra-2", 2)]));
        assert_eq!(app.view_mode(), ViewMode::List);
        assert_eq!(app.rows().len(), 2);
        assert_eq!(app.selection(), Some(0));
        assert!(!app.is_stale());
    }

    #[test]
    fn reports_attribution_generations_retained_by_ready_rows() {
        let generation = crate::refresh::AttributionGeneration::new(11);
        let mut retained = row("repo", "ra-1", 1);
        retained.attribution_generation = Some(generation);
        let app = app_with(vec![retained]);

        assert_eq!(
            app.attribution_generations(),
            std::collections::HashSet::from([generation])
        );
    }

    #[test]
    fn selection_moves_and_clamps() {
        let mut app = app_with(vec![
            row("ra", "ra-1", 1),
            row("ra", "ra-2", 1),
            row("ra", "ra-3", 1),
        ]);

        assert_eq!(app.selection(), Some(0));
        app.reduce(Msg::SelectNext);
        assert_eq!(app.selection(), Some(1));
        app.reduce(Msg::SelectNext);
        assert_eq!(app.selection(), Some(2));
        // Clamps at the last row, never out of bounds.
        app.reduce(Msg::SelectNext);
        assert_eq!(app.selection(), Some(2));

        app.reduce(Msg::SelectPrev);
        assert_eq!(app.selection(), Some(1));
        app.reduce(Msg::SelectPrev);
        app.reduce(Msg::SelectPrev);
        assert_eq!(app.selection(), Some(0), "clamps at the first row");

        // Empty list: navigation is a safe no-op and nothing is selected.
        let mut empty = app_with(vec![]);
        assert_eq!(empty.selection(), None);
        empty.reduce(Msg::SelectNext);
        empty.reduce(Msg::SelectPrev);
        assert_eq!(empty.selection(), None);
        assert!(empty.selected_row().is_none());
    }

    #[test]
    fn repo_picker_confirms_repository_views() {
        let mut app = app_with(vec![
            row("repo-a", "ra-1", 1),
            row("repo-a", "ra-2", 1),
            row("repo-b", "rb-1", 1),
        ]);
        assert_eq!(app.filtered_rows().len(), 3, "All: every row visible");

        choose_repo(&mut app, RepoFilter::Only("repo-a".into()));
        assert_eq!(app.repo_view(), &RepoFilter::Only("repo-a".into()));
        assert_eq!(ids(&app.filtered_rows()), vec!["ra-1", "ra-2"]);
        assert_eq!(app.selection(), Some(0), "selection stays valid");

        choose_repo(&mut app, RepoFilter::Only("repo-b".into()));
        assert_eq!(app.repo_view(), &RepoFilter::Only("repo-b".into()));
        assert_eq!(ids(&app.filtered_rows()), vec!["rb-1"]);

        choose_repo(&mut app, RepoFilter::All);
        assert_eq!(app.repo_view(), &RepoFilter::All);
        assert_eq!(app.filtered_rows().len(), 3);
    }

    #[test]
    fn priority_filter_toggles() {
        let mut app = app_with(vec![
            row("ra", "ra-0", 0),
            row("ra", "ra-1", 1),
            row("ra", "ra-2", 2),
            row("ra", "ra-3", 3),
        ]);
        assert_eq!(app.filtered_rows().len(), 4);

        app.reduce(Msg::TogglePriorityFilter);
        assert_eq!(app.filter().priority(), PriorityFilter::HighOnly);
        assert_eq!(
            ids(&app.filtered_rows()),
            vec!["ra-0", "ra-1"],
            "only P0/P1 visible"
        );

        app.reduce(Msg::TogglePriorityFilter);
        assert_eq!(app.filter().priority(), PriorityFilter::All);
        assert_eq!(app.filtered_rows().len(), 4, "toggles back to all");
    }

    #[test]
    fn global_repo_view_combines_with_list_local_priority() {
        let mut app = app_with(vec![
            row("repo-a", "a-high", 1),
            row("repo-a", "a-low", 2),
            row("repo-b", "b-high", 0),
        ]);
        app.reduce(Msg::TogglePriorityFilter);

        choose_repo(&mut app, RepoFilter::Only("repo-a".into()));

        assert_eq!(app.filter().priority(), PriorityFilter::HighOnly);
        assert_eq!(ids(&app.filtered_rows()), vec!["a-high"]);
    }

    #[test]
    fn refresh_while_stale_keeps_rows() {
        let mut app = app_with(vec![row("ra", "ra-1", 1), row("ra", "ra-2", 1)]);
        app.reduce(Msg::SelectNext);
        assert_eq!(app.selection(), Some(1));

        app.reduce(Msg::RefreshStarted);
        assert_eq!(app.rows().len(), 2, "old rows stay visible during refresh");
        assert_eq!(app.selection(), Some(1), "selection preserved");
        assert_eq!(app.view_mode(), ViewMode::List);
        assert!(app.is_stale(), "stale flag set while refreshing");
    }

    #[test]
    fn hydrate_from_cache_paints_rows_without_clearing_the_launch_refresh_guard() {
        // A cache hit must not clear `App::new`'s born-`stale` in-flight guard:
        // the runtime spawns the real launch refresh right after hydrating, and
        // if `stale` were cleared in between, a quick `r` would slip past the
        // `Msg::Refresh` dedup check and spawn a second, overlapping worker
        // (hydrating via `reduce(Msg::RefreshCompleted { .. })` had exactly
        // this bug).
        let mut app = App::new();
        assert!(
            app.is_stale(),
            "born stale, reserving the launch refresh slot"
        );

        app.hydrate_from_cache(snapshot(vec![row("ra", "ra-1", 1)]));

        assert_eq!(app.rows().len(), 1, "cached rows are shown");
        assert_eq!(app.view_mode(), ViewMode::List, "Loading promotes to List");
        assert!(
            app.is_stale(),
            "the in-flight guard survives cache hydration"
        );
        // With the guard still armed, a racing `r` dedups to nothing, exactly
        // as it would against the real launch refresh's own `RefreshStarted`.
        assert!(app.reduce(Msg::Refresh).is_empty());
    }

    #[test]
    fn refresh_error_surfaces_in_status() {
        let mut app = app_with(vec![row("ra", "ra-1", 1)]);
        app.reduce(Msg::RefreshStarted);
        assert!(app.is_stale());

        // A refresh that succeeded but had per-repo trouble: the snapshot and its
        // warnings arrive together in one completion message.
        app.reduce(Msg::RefreshCompleted {
            snapshot: Some(snapshot(vec![row("ra", "ra-1", 1)])),
            warnings: vec![
                "export failed for repo-b".into(),
                "id prefix `dup` claimed by 2 repos".into(),
            ],
        });
        assert!(
            app.status_warnings()
                .iter()
                .any(|w| w.contains("export failed for repo-b")),
            "per-repo error surfaced: {:?}",
            app.status_warnings()
        );
        assert!(!app.is_stale(), "the refresh cycle concluded");
    }

    #[test]
    fn fatal_refresh_keeps_rows_and_surfaces_warning() {
        // A refresh that failed outright: no snapshot, but the stale view is kept
        // and the error is surfaced.
        let mut app = app_with(vec![row("ra", "ra-1", 1), row("ra", "ra-2", 1)]);
        app.reduce(Msg::RefreshStarted);
        app.reduce(Msg::RefreshCompleted {
            snapshot: None,
            warnings: vec!["hub sync failed".into()],
        });
        assert_eq!(
            app.rows().len(),
            2,
            "last-good rows kept on a failed refresh"
        );
        assert_eq!(app.view_mode(), ViewMode::List);
        assert!(!app.is_stale(), "the failed cycle still concludes");
        assert!(app.status_warnings().iter().any(|w| w.contains("hub sync")));
    }

    #[test]
    fn refresh_key_requests_refresh_effect() {
        let mut app = app_with(vec![row("ra", "ra-1", 1)]);
        let before = app.clone();

        let effects = app.reduce(Msg::Refresh);
        assert_eq!(effects, vec![Effect::Refresh(RefreshScope::Full)]);
        // Marks the shown rows stale/in-flight, but touches nothing else: the
        // runtime spawns the worker.
        assert!(app.is_stale());
        assert_eq!(app.rows().len(), before.rows().len());
        assert_eq!(app.selection(), before.selection());
        assert_eq!(app.view_mode(), before.view_mode());
        assert!(!app.is_done());
    }

    #[test]
    fn refresh_is_deduped_while_in_flight() {
        // A second `r` (or a key-repeat) while a refresh is pending must not spawn
        // an overlapping worker whose out-of-order completion could clobber a
        // newer snapshot.
        let mut app = app_with(vec![row("ra", "ra-1", 1)]);

        assert_eq!(
            app.reduce(Msg::Refresh),
            vec![Effect::Refresh(RefreshScope::Full)]
        );
        assert_eq!(
            app.reduce(Msg::Refresh),
            Vec::new(),
            "no second effect while a refresh is in flight"
        );

        // Once the cycle concludes, a fresh request is honored again.
        app.reduce(completed(vec![row("ra", "ra-1", 1)]));
        assert!(!app.is_stale());
        assert_eq!(
            app.reduce(Msg::Refresh),
            vec![Effect::Refresh(RefreshScope::Full)]
        );
    }

    #[test]
    fn success_with_warnings_completes_atomically() {
        // Regression: a successful-with-warnings refresh must conclude in ONE
        // message. If it split into snapshot-then-warnings, an `r` in the gap
        // would slip past the dedup guard and the trailing warnings message would
        // then clear the *new* refresh's in-flight flag. Here the single
        // completion clears `stale` exactly once, and the interleaved second
        // refresh is a distinct, still-guarded cycle.
        let mut app = app_with(vec![row("ra", "ra-1", 1)]);

        assert_eq!(
            app.reduce(Msg::Refresh),
            vec![Effect::Refresh(RefreshScope::Full)]
        );
        assert!(app.is_stale());
        // First cycle concludes atomically with a snapshot and warnings.
        app.reduce(Msg::RefreshCompleted {
            snapshot: Some(snapshot(vec![row("ra", "ra-2", 1)])),
            warnings: vec!["export failed for repo-b".into()],
        });
        assert!(!app.is_stale());
        assert_eq!(app.status_warnings().len(), 1);

        // A new refresh starts its own guarded cycle; no leftover completion
        // message from the first cycle exists to clear it.
        assert_eq!(
            app.reduce(Msg::Refresh),
            vec![Effect::Refresh(RefreshScope::Full)]
        );
        assert!(app.is_stale());
        assert_eq!(
            app.reduce(Msg::Refresh),
            Vec::new(),
            "the second cycle is still deduped"
        );
    }

    #[test]
    fn startup_refresh_holds_the_in_flight_slot() {
        // The runtime spawns an initial refresh at launch without going through
        // `Msg::Refresh`, so a brand-new app must already be in-flight: an `r`
        // that races the initial worker's `RefreshStarted` is deduped, not a
        // second worker.
        let mut app = App::new();
        assert!(app.is_stale(), "a fresh app is born in-flight");
        assert_eq!(
            app.reduce(Msg::Refresh),
            Vec::new(),
            "an immediate r is deduped against the startup refresh"
        );

        // When the initial refresh concludes, the slot frees and r works again.
        app.reduce(completed(vec![row("ra", "ra-1", 1)]));
        assert!(!app.is_stale());
        assert_eq!(
            app.reduce(Msg::Refresh),
            vec![Effect::Refresh(RefreshScope::Full)]
        );
    }

    #[test]
    fn quit_msg_sets_done() {
        let mut app = app_with(vec![row("ra", "ra-1", 1)]);
        assert!(!app.is_done());
        app.reduce(Msg::Quit);
        assert!(app.is_done());
    }

    #[test]
    fn filters_persist_and_recompute_across_refresh() {
        let mut app = app_with(vec![row("repo-a", "ra-1", 1), row("repo-b", "rb-1", 1)]);
        choose_repo(&mut app, RepoFilter::Only("repo-a".into()));
        assert_eq!(app.repo_view(), &RepoFilter::Only("repo-a".into()));

        // A new snapshot (different rows, still has repo-a) keeps the filter.
        app.reduce(completed(vec![
            row("repo-a", "ra-9", 1),
            row("repo-a", "ra-8", 2),
            row("repo-b", "rb-9", 1),
        ]));
        assert_eq!(
            app.repo_view(),
            &RepoFilter::Only("repo-a".into()),
            "the active filter survives a refresh"
        );
        assert_eq!(ids(&app.filtered_rows()), vec!["ra-9", "ra-8"]);
        assert_eq!(app.selection(), Some(0), "selection valid after recompute");
    }

    #[test]
    fn selection_invariant_holds_under_random_messages() {
        // A deterministic LCG (no rand dep) drives a long message sequence; after
        // every step the selection invariant must hold.
        let mut seed: u64 = 0x1234_5678_9abc_def0;
        let mut next = || {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) as u32
        };

        let sample_sets: Vec<Vec<Row>> = vec![
            vec![],
            vec![row("repo-a", "ra-1", 0)],
            vec![
                row("repo-a", "ra-1", 0),
                row("repo-a", "ra-2", 2),
                row("repo-b", "rb-1", 1),
                row("repo-b", "rb-2", 3),
            ],
            vec![row("repo-b", "rb-9", 1), row("repo-c", "rc-9", 2)],
        ];

        let mut app = App::new();
        for _ in 0..5_000 {
            let msg = match next() % 6 {
                0 => Msg::SelectNext,
                1 => Msg::SelectPrev,
                2 => Msg::OpenRepoPicker,
                3 => Msg::TogglePriorityFilter,
                4 => Msg::RefreshStarted,
                _ => {
                    let set = &sample_sets[(next() as usize) % sample_sets.len()];
                    completed(set.clone())
                }
            };
            app.reduce(msg);

            let visible = app.filtered_rows();
            match app.selection() {
                None => {
                    assert!(visible.is_empty(), "no selection only when nothing visible");
                    assert!(app.selected_row().is_none());
                }
                Some(i) => {
                    assert!(
                        i < visible.len(),
                        "selection {i} within {} rows",
                        visible.len()
                    );
                    assert_eq!(
                        app.selected_row().map(|r| &r.issue.id),
                        Some(&visible[i].issue.id),
                        "selected_row agrees with the selection index"
                    );
                }
            }
        }
    }

    fn repos(paths: &[&str]) -> RefreshScope {
        RefreshScope::Repos(paths.iter().map(PathBuf::from).collect())
    }

    #[test]
    fn watch_change_while_idle_refreshes_just_those_repos() {
        let mut app = app_with(vec![row("ra", "ra-1", 1)]);
        assert_eq!(
            app.reduce(Msg::WatchChanged(repos(&["/a"]))),
            vec![Effect::Refresh(repos(&["/a"]))]
        );
        assert!(
            app.is_stale(),
            "the watcher's refresh holds the in-flight slot"
        );
        assert!(app.reduce(Msg::Refresh).is_empty(), "r dedups against it");
    }

    #[test]
    fn watch_changes_during_a_refresh_queue_and_merge_into_one_follow_up() {
        let mut app = App::new();
        assert!(app.reduce(Msg::WatchChanged(repos(&["/a"]))).is_empty());
        assert!(app.reduce(Msg::WatchChanged(repos(&["/b"]))).is_empty());

        assert_eq!(
            app.reduce(completed(vec![row("ra", "ra-1", 1)])),
            vec![Effect::Refresh(repos(&["/a", "/b"]))],
            "changes seen mid-cycle get a cycle of their own"
        );
        assert!(app.is_stale());
        assert!(
            app.reduce(completed(vec![row("ra", "ra-1", 1)])).is_empty(),
            "nothing left queued"
        );
        assert!(!app.is_stale());
    }

    #[test]
    fn a_queued_full_refresh_absorbs_scoped_ones() {
        let mut app = App::new();
        app.reduce(Msg::WatchChanged(repos(&["/a"])));
        app.reduce(Msg::WatchChanged(RefreshScope::Full));
        app.reduce(Msg::WatchChanged(repos(&["/b"])));
        assert_eq!(
            app.reduce(completed(Vec::new())),
            vec![Effect::Refresh(RefreshScope::Full)]
        );
    }

    #[test]
    fn watch_warnings_dedup_and_outlive_refresh_cycles() {
        let mut app = App::new();
        app.reduce(Msg::WatchWarning("journal off for /a".into()));
        app.reduce(Msg::WatchWarning("journal off for /a".into()));
        app.reduce(completed(Vec::new()));
        assert_eq!(app.watch_warnings(), ["journal off for /a".to_string()]);
        assert!(app.status_warnings().is_empty());
    }

    fn failed() -> Msg {
        Msg::RefreshCompleted {
            snapshot: None,
            warnings: vec!["another hank is refreshing this hub".into()],
        }
    }

    #[test]
    fn a_failed_watch_refresh_is_retried_with_backoff() {
        let mut app = app_with(vec![row("ra", "ra-1", 1)]);
        app.reduce(Msg::WatchChanged(repos(&["/a"])));
        assert_eq!(
            app.reduce(failed()),
            vec![Effect::RetryWatch {
                scope: repos(&["/a"]),
                after: Duration::from_secs(5)
            }]
        );
        app.reduce(Msg::WatchChanged(repos(&["/a"])));
        assert_eq!(
            app.reduce(failed()),
            vec![Effect::RetryWatch {
                scope: repos(&["/a"]),
                after: Duration::from_secs(10)
            }],
            "consecutive failures back off"
        );
        app.reduce(Msg::WatchChanged(repos(&["/a"])));
        assert!(app.reduce(completed(vec![row("ra", "ra-1", 1)])).is_empty());
        app.reduce(Msg::WatchChanged(repos(&["/a"])));
        assert_eq!(
            app.reduce(failed()),
            vec![Effect::RetryWatch {
                scope: repos(&["/a"]),
                after: Duration::from_secs(5)
            }],
            "a success resets the backoff"
        );
    }

    #[test]
    fn a_failed_watch_refresh_folds_into_a_queued_one() {
        let mut app = app_with(Vec::new());
        app.reduce(Msg::WatchChanged(repos(&["/a"])));
        app.reduce(Msg::WatchChanged(repos(&["/b"])));
        assert_eq!(
            app.reduce(failed()),
            vec![Effect::Refresh(repos(&["/a", "/b"]))]
        );
    }

    #[test]
    fn a_failed_manual_refresh_is_not_retried() {
        let mut app = app_with(Vec::new());
        app.reduce(Msg::Refresh);
        assert!(app.reduce(failed()).is_empty());
    }

    fn sync(path: &str, outcome: RepoSyncOutcome) -> RepoSync {
        RepoSync {
            path: PathBuf::from(path),
            prefix: Some(path.trim_start_matches('/').to_string()),
            outcome,
        }
    }

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn health<'a>(app: &'a App, path: &str) -> &'a RepoHealth {
        app.repo_health()
            .iter()
            .find(|health| health.path == Path::new(path))
            .expect("repo reported")
    }

    #[test]
    fn repo_syncs_track_freshness_and_failures_across_cycles() {
        let mut app = app_with(Vec::new());
        app.reduce(Msg::RepoSyncs {
            synced_at: at(100),
            repos: vec![
                sync("/a", RepoSyncOutcome::Exported),
                sync("/b", RepoSyncOutcome::Exported),
            ],
        });
        assert_eq!(app.stale_repos().count(), 0);
        assert_eq!(health(&app, "/b").synced_at, Some(at(100)));

        // /b fails: the hub keeps its older export, so it is stale since 100.
        app.reduce(Msg::RepoSyncs {
            synced_at: at(200),
            repos: vec![
                sync("/a", RepoSyncOutcome::Exported),
                sync("/b", RepoSyncOutcome::Failed("export failed".into())),
            ],
        });
        assert_eq!(health(&app, "/a").synced_at, Some(at(200)));
        let b = health(&app, "/b");
        assert_eq!(b.synced_at, Some(at(100)), "last clean sync kept");
        assert_eq!(b.problem.as_deref(), Some("export failed"));
        assert_eq!(app.stale_repos().count(), 1);

        // A live refresh that carries both over exports neither: each keeps
        // its last clean export time, and /b stays stale.
        app.reduce(Msg::RepoSyncs {
            synced_at: at(300),
            repos: vec![
                sync("/a", RepoSyncOutcome::Carried),
                sync("/b", RepoSyncOutcome::Carried),
            ],
        });
        assert_eq!(health(&app, "/a").synced_at, Some(at(200)));
        assert!(!health(&app, "/a").is_stale());
        assert!(health(&app, "/b").is_stale(), "carrying over never clears");
        assert_eq!(health(&app, "/b").synced_at, Some(at(100)));

        // /b recovers; a repo removed from the roster drops out.
        app.reduce(Msg::RepoSyncs {
            synced_at: at(400),
            repos: vec![sync("/b", RepoSyncOutcome::Exported)],
        });
        assert_eq!(app.repo_health().len(), 1);
        assert_eq!(health(&app, "/b").synced_at, Some(at(400)));
        assert_eq!(app.stale_repos().count(), 0);
    }

    #[test]
    fn a_repo_failing_its_first_sync_was_never_synced() {
        let mut app = app_with(Vec::new());
        app.reduce(Msg::RepoSyncs {
            synced_at: at(100),
            repos: vec![sync("/a", RepoSyncOutcome::Failed("gone".into()))],
        });
        let a = health(&app, "/a");
        assert_eq!(a.synced_at, None);
        assert!(a.is_stale());
    }

    #[test]
    fn repo_syncs_leave_the_refresh_in_flight() {
        let mut app = app_with(Vec::new());
        app.reduce(Msg::Refresh);
        app.reduce(Msg::RepoSyncs {
            synced_at: at(100),
            repos: vec![sync("/a", RepoSyncOutcome::Exported)],
        });
        assert!(app.is_stale(), "only RefreshCompleted ends the cycle");
        assert!(app.reduce(Msg::Refresh).is_empty(), "still deduped");
    }

    #[test]
    fn health_panel_opens_runs_doctor_and_closes() {
        let mut app = app_with(Vec::new());
        assert_eq!(
            app.reduce(Msg::ToggleHealth),
            vec![Effect::CheckHealth { token: 1 }]
        );
        assert!(app.health_open());
        assert_eq!(app.input_context(), InputContext::Health);
        assert_eq!(app.health_doctor(), Some(&DoctorState::Running));

        app.reduce(Msg::HealthReport {
            token: 1,
            report: Ok("gate: OK".into()),
        });
        assert_eq!(
            app.health_doctor(),
            Some(&DoctorState::Done("gate: OK".into()))
        );

        assert!(app.reduce(Msg::Back).is_empty());
        assert!(!app.health_open(), "esc closes the panel");
        assert_eq!(app.view_mode(), ViewMode::List, "and nothing beneath it");
        assert_eq!(app.input_context(), InputContext::Normal);

        // Reopening after the run finished starts a fresh one.
        assert_eq!(
            app.reduce(Msg::ToggleHealth),
            vec![Effect::CheckHealth { token: 2 }]
        );
        assert!(app.reduce(Msg::ToggleHealth).is_empty(), "h closes it too");
        assert!(!app.health_open());
    }

    #[test]
    fn reopening_the_health_panel_shares_the_doctor_run_in_flight() {
        let mut app = app_with(Vec::new());
        assert_eq!(
            app.reduce(Msg::ToggleHealth),
            vec![Effect::CheckHealth { token: 1 }]
        );
        // A held `h` toggles close/open repeatedly: no new runs start.
        for _ in 0..5 {
            assert!(app.reduce(Msg::ToggleHealth).is_empty());
            assert!(app.reduce(Msg::ToggleHealth).is_empty());
        }
        assert!(app.health_open());
        app.reduce(Msg::HealthReport {
            token: 1,
            report: Ok("gate: OK".into()),
        });
        assert_eq!(
            app.health_doctor(),
            Some(&DoctorState::Done("gate: OK".into())),
            "the shared run fills the reopened panel"
        );

        // A run whose panel was closed still clears the in-flight slot.
        app.reduce(Msg::Back);
        assert_eq!(
            app.reduce(Msg::ToggleHealth),
            vec![Effect::CheckHealth { token: 2 }]
        );
        app.reduce(Msg::Back);
        app.reduce(Msg::HealthReport {
            token: 2,
            report: Err("late".into()),
        });
        assert_eq!(
            app.reduce(Msg::ToggleHealth),
            vec![Effect::CheckHealth { token: 3 }]
        );
    }

    #[test]
    fn health_panel_scroll_saturates_and_is_clamped_by_the_view() {
        let mut app = app_with(Vec::new());
        app.reduce(Msg::ToggleHealth);
        app.reduce(Msg::HealthScroll(-1));
        assert_eq!(app.health_scroll(), 0);
        app.reduce(Msg::HealthScroll(10));
        app.reduce(Msg::DetailScrollBounds { max_scroll: 4 });
        assert_eq!(app.health_scroll(), 4);
        app.reduce(Msg::HealthScroll(-1));
        assert_eq!(app.health_scroll(), 3);
    }
}
