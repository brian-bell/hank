//! The headless command runners behind hank's clap CLI: `snapshot`, `doctor`,
//! and `reset`, plus the startup version gate and the shared row formatter.
//!
//! Every runner takes an injected `&impl BdClient`, `&Paths`, and explicit
//! `&mut impl Write` sinks — no process spawning, no XDG reads, no hidden clock —
//! so the whole surface is unit-tested against `FakeBdClient` and driven
//! end-to-end by the gated integration suite against the real `BdCli`. `main` is
//! the only caller that resolves real paths and wires stdout/stderr.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::bd::{BdClient, BdError, BdVersion};
use crate::cache;
use crate::config::{self, Config, Paths, RepoEntry};
use crate::hub::{self, HubError, hub_dir};
use crate::refresh::{self, PrefixMap, RefreshError};
use crate::snapshot::{self, Row};

/// The minimum bd the version gate accepts.
const MIN_BD_VERSION: (u64, u64, u64) = (1, 1, 0);
/// The bd `schema_version` hank's `--json` parsing is written against.
const REQUIRED_SCHEMA: i64 = 1;

/// A fatal command failure. `Ok(())` maps to exit 0 in `main`; any `Err` prints
/// `error: <e>` to stderr and exits nonzero.
#[derive(Debug, thiserror::Error)]
pub enum CliError {
    /// The startup version gate rejected the installed bd. The message is
    /// actionable and printed verbatim.
    #[error("{0}")]
    VersionGate(String),
    /// Hub lifecycle failed fatally (e.g. init).
    #[error(transparent)]
    Hub(#[from] HubError),
    /// Refresh failed fatally (sync/lock/io — `AlreadyRefreshing` is handled as a
    /// non-fatal degraded path inside `run_snapshot`, never surfaced here).
    #[error(transparent)]
    Refresh(#[from] RefreshError),
    /// A `bd` read (version/ready) failed fatally.
    #[error(transparent)]
    Bd(#[from] BdError),
    /// A roster edit was rejected with an actionable, printed-verbatim message
    /// (e.g. `repos add` on a directory that is not a beads repo).
    #[error("{0}")]
    Roster(String),
    /// Legacy user state could not be validated or published safely.
    #[error("{0}")]
    Migration(String),
    /// Writing to an output sink failed.
    #[error("writing output: {0}")]
    Io(#[from] std::io::Error),
}

/// Format one snapshot row for display: `[<repo>] P<priority> <id> <title>`.
///
/// Pure `Row → String`; Slice 9's ready-list view reuses it so the headless and
/// TUI renderings of a row never drift.
///
/// bd-sourced fields (repo name, id, title) are [`sanitize`]d: they are attacker-
/// influenceable data (an issue title in a federated repo you don't control), and
/// this string is written straight to a terminal. Left raw, a title carrying
/// newlines or ANSI/OSC escapes could forge extra rows or drive the terminal
/// (cursor moves, or an OSC 52 clipboard write). JSON output is unaffected —
/// serde escapes control characters on its own.
pub fn format_row(row: &Row) -> String {
    format!("[{}] {}", sanitize(&row.repo_name), format_row_body(row))
}

/// Format the repo-independent body of a row: `P<priority> <id> <title>`, with
/// bd-sourced `id`/`title` [`sanitize`]d. Shared with Slice 9's TUI view, which
/// draws the repo in a group header instead of inline — so the headless
/// (`hank snapshot`) and TUI renderings of a row can never drift.
pub fn format_row_body(row: &Row) -> String {
    format!(
        "P{} {} {}",
        row.issue.priority,
        sanitize(&row.issue.id),
        sanitize(&row.issue.title),
    )
}

/// Replace every control character (C0/C1, DEL, and the line breaks that would
/// let a value span rows) with the Unicode replacement character, so bd-sourced
/// text cannot inject terminal-control sequences into human-readable output.
///
/// `pub(crate)` so the TUI view can neutralize the same bd-sourced text (repo
/// names, ids, titles) it renders straight into the terminal buffer.
pub(crate) fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { '\u{FFFD}' } else { c })
        .collect()
}

/// Accept a bd version iff `schema_version == 1` and `version >= 1.1.0`.
/// On rejection returns an actionable message naming both the requirement and
/// what was found.
pub fn version_gate(v: &BdVersion) -> Result<(), String> {
    let version_ok = parse_version(&v.version).is_some_and(|got| got >= MIN_BD_VERSION);
    let schema_ok = v.schema_version == REQUIRED_SCHEMA;
    if version_ok && schema_ok {
        return Ok(());
    }
    let (maj, min, pat) = MIN_BD_VERSION;
    Err(format!(
        "hank requires bd >= {maj}.{min}.{pat} with schema_version {REQUIRED_SCHEMA}, \
         but found bd {} (schema_version {}). Upgrade bd \
         (https://github.com/gastownhall/beads).",
        v.version, v.schema_version,
    ))
}

/// Parse the leading `major.minor.patch` of a version string into a comparable
/// tuple, ignoring any `-pre`/`+build` suffix. Requires all three numeric
/// components — an incomplete or non-numeric version (`1.1`, `2`, `x`) yields
/// `None` and fails the gate closed, since the gate exists to refuse a bd whose
/// schema hank cannot vouch for.
fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let core = s.split(['-', '+']).next().unwrap_or(s);
    let mut parts = core.split('.');
    let major = parts.next()?.trim().parse().ok()?;
    let minor = parts.next()?.trim().parse().ok()?;
    let patch = parts.next()?.trim().parse().ok()?;
    Some((major, minor, patch))
}

/// `ensure_hub → refresh → fetch → print`. See `plans/slices/slice-6.md` for the
/// full control flow (version gate fatal; per-repo errors and `AlreadyRefreshing`
/// degrade with a warning; sync/hub/ready failures fatal).
pub fn run_snapshot(
    roster: &Config,
    bd: &impl BdClient,
    paths: &Paths,
    json: bool,
    out: &mut impl Write,
    err: &mut impl Write,
) -> Result<(), CliError> {
    // The gate protects the whole data-reading path: reject a bd whose schema
    // hank's `--json` parsing was not written against, before touching the hub.
    version_gate(&bd.version()?).map_err(CliError::VersionGate)?;

    let status = hub::ensure_hub(bd, paths, roster)?;
    for warning in &status.warnings {
        // Warnings embed config/repo-derived text (paths, bd stderr, prefixes)
        // and go to a terminal, so sanitize them like rows and doctor output.
        writeln!(err, "warning: {}", sanitize(warning))?;
    }

    let hub = hub_dir(paths);
    let fallback_map = PrefixMap::default();
    let (snapshot, refresh_warnings) = match refresh::run_with_state(bd, roster, paths, None, 4) {
        Ok(synced) => {
            // Per-repo failures and prefix collisions are surfaced but never fatal
            // — the hub still synced whatever exported cleanly.
            let mut warnings = synced
                .outcome()
                .errors
                .iter()
                .map(|repo_error| sanitize(&repo_error.to_string()))
                .collect::<Vec<_>>();
            for collision in synced.outcome().prefix_map.collisions() {
                warnings.push(format!(
                    "id prefix `{}` is claimed by {} repos; its issues show as `{}`",
                    sanitize(&collision.prefix),
                    collision.repos.len(),
                    snapshot::UNKNOWN_REPO,
                ));
            }
            refresh::publish_hub_generation(paths, &refresh::HubGenerationToken::fresh())?;
            let snapshot = snapshot::fetch(
                bd,
                &hub,
                &synced.outcome().prefix_map,
                synced.outcome().synced_at,
            )?;
            drop(synced);
            (snapshot, warnings)
        }
        // Degraded, not fatal: another hank holds the lock, so print the last
        // synced data (attribution unavailable → every row falls to `unknown`).
        Err(RefreshError::AlreadyRefreshing) => (
            snapshot::fetch(bd, &hub, &fallback_map, SystemTime::now())?,
            vec!["another hank is refreshing this hub; showing the last synced data".to_string()],
        ),
        Err(fatal) => return Err(fatal.into()),
    };

    for warning in refresh_warnings {
        writeln!(err, "warning: {warning}")?;
    }

    if json {
        serde_json::to_writer_pretty(&mut *out, &snapshot)
            .map_err(|e| CliError::Io(std::io::Error::other(e)))?;
        writeln!(out)?;
    } else {
        for row in &snapshot.rows {
            writeln!(out, "{}", format_row(row))?;
        }
    }
    Ok(())
}

/// Report environment health: bd version + gate status, config/hub paths, and
/// per-repo roster existence + prefix. Deliberately **not** version-gated.
///
/// Doctor loads the roster itself (rather than being handed one) precisely so a
/// malformed config becomes a *reported* diagnostic instead of an error that
/// aborts the command before it can diagnose anything — the config being broken
/// is one of the things you run doctor to discover.
pub fn run_doctor(bd: &impl BdClient, paths: &Paths, out: &mut impl Write) -> Result<(), CliError> {
    let migration = config::migrate_legacy_state(paths);
    if migration.problems().next().is_none() {
        writeln!(
            out,
            "legacy migration: OK ({} file(s) migrated)",
            migration.migrated().len()
        )?;
    } else {
        if let Some(problem) = migration.fatal_problem() {
            writeln!(out, "legacy migration: ERROR {problem}")?;
        }
        for warning in migration.warnings() {
            writeln!(out, "legacy migration: WARNING {warning}")?;
        }
    }

    // Doctor is the diagnostic you run *because* something is wrong, so it never
    // gates: it reports the version and whether the gate would pass, and tolerates
    // bd being absent entirely.
    match bd.version() {
        Ok(v) => {
            write!(
                out,
                "bd version: {} (schema {})",
                v.version, v.schema_version
            )?;
            match version_gate(&v) {
                Ok(()) => writeln!(out, "  gate: OK")?,
                Err(msg) => writeln!(out, "  gate: FAIL — {msg}")?,
            }
        }
        Err(e) => writeln!(out, "bd version: ERROR {e}")?,
    }

    let config_file = paths.config_file();
    writeln!(out, "config: {}", config_file.display())?;
    let hub = hub_dir(paths);
    let initialized = hub.join(".beads").join("embeddeddolt").is_dir();
    writeln!(
        out,
        "hub: {} ({})",
        hub.display(),
        if initialized {
            "initialized"
        } else {
            "not created yet"
        },
    )?;

    // Load the roster here so a missing or malformed config is a reported line,
    // not a failure that would defeat doctor's whole purpose.
    match load_roster(paths) {
        Ok(roster) => {
            writeln!(out, "roster ({} repos):", roster.repos.len())?;
            for entry in &roster.repos {
                // Both the path (from config) and the prefix (from repo metadata)
                // are repo/config-influenceable and go to a terminal, so apply the
                // same control-char sanitizer format_row uses.
                let shown = sanitize(&entry.path.display().to_string());
                if entry.path.exists() {
                    let prefix = bd
                        .issue_prefix(&entry.path)
                        .unwrap_or_else(|_| "?".to_string());
                    writeln!(out, "  {}  OK  [prefix: {}]", shown, sanitize(&prefix))?;
                } else {
                    writeln!(out, "  {}  MISSING", shown)?;
                }
            }
        }
        Err(e) => writeln!(out, "roster: ERROR reading {}: {e}", config_file.display())?,
    }
    Ok(())
}

/// Run the launch-time legacy migration for commands other than `doctor`.
/// Invalid config or migration infrastructure is fatal; invalid legacy UI state
/// is reported and the TUI safely falls back to All repositories.
pub fn prepare_legacy_state(paths: &Paths, err: &mut impl Write) -> Result<(), CliError> {
    let migration = config::migrate_legacy_state(paths);
    if let Some(problem) = migration.fatal_problem() {
        return Err(CliError::Migration(problem.to_string()));
    }
    for warning in migration.warnings() {
        writeln!(err, "warning: {warning}")?;
    }
    Ok(())
}

/// Load the roster from `<config>/config.toml`, treating an absent file as an
/// empty roster (first run) while surfacing a present-but-invalid file as an
/// error. Shared by `main`'s snapshot path (where a bad config is fatal) and
/// `run_doctor` (where it is reported, not fatal). Relative entries are
/// absolutized against the config directory here so every lower-level consumer
/// operates on the same repository regardless of the process CWD.
pub fn load_roster(paths: &Paths) -> Result<Config, CliError> {
    let config_file = paths.config_file();
    // `Path::exists` collapses "absent" and "present but unreadable" (permission
    // error, dangling symlink) into the same `false`, which would silently
    // downgrade a broken config to an empty first-run roster. Distinguish with
    // `symlink_metadata`: only a genuine `NotFound` is first-run; anything else is
    // attempted so a real error surfaces per this function's contract.
    match std::fs::symlink_metadata(config_file) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        _ => {
            let mut roster =
                Config::load(config_file).map_err(|e| CliError::Io(std::io::Error::other(e)))?;
            for entry in &mut roster.repos {
                let expanded = expand_tilde(&entry.path);
                entry.path = store_path(&paths.resolve_roster_path(&expanded));
            }
            Ok(roster)
        }
    }
}

/// Delete the hub dir (rebuilt on the next snapshot/launch) and report.
///
/// Also clears the on-disk snapshot cache: a reset discards the hub's state,
/// and a cache left behind would otherwise let the very next launch paint
/// rows from the just-discarded hub for up to [`cache::MAX_AGE`].
pub fn run_reset(paths: &Paths, out: &mut impl Write) -> Result<(), CliError> {
    let hub = hub_dir(paths);
    // Mirror `hub::reset`'s own `symlink_metadata` test rather than `Path::exists`:
    // exists() follows symlinks and reports false for a dangling hub symlink that
    // reset nonetheless removes, which would misreport it as "nothing to remove".
    let existed = std::fs::symlink_metadata(&hub).is_ok();
    hub::reset(paths)?;
    cache::clear(paths.cache_file())?;
    if existed {
        writeln!(out, "hub reset: removed {}", hub.display())?;
    } else {
        writeln!(out, "hub reset: nothing to remove ({})", hub.display())?;
    }
    Ok(())
}

/// Expand a leading `~` / `~/…` to `$HOME`. clap passes `~` through literally, so
/// callers must expand it explicitly. Anything else (no leading `~`, `~user`, or an
/// unresolvable home) is returned unchanged.
fn expand_tilde(p: &Path) -> PathBuf {
    let Ok(rest) = p.strip_prefix("~") else {
        return p.to_path_buf();
    };
    match dirs::home_dir() {
        Some(home) => home.join(rest),
        None => p.to_path_buf(),
    }
}

/// The canonical, absolute key a roster entry is compared and stored under: expand
/// `~`, then canonicalize. Used both when storing a new entry and when normalizing
/// existing entries for comparison, so `add`/`remove`/`discover` all match on the
/// same key regardless of how a path was spelled (relative, aliased, `~`).
///
/// When the target itself is gone from disk (e.g. `remove` of a deleted repo, which
/// is exactly when you want to remove it), `canonicalize` fails; the fallback
/// canonicalizes the *parent* and rejoins the final component, so a repo added as
/// `./repo` and stored as its resolved absolute path still matches after deletion —
/// the parent's symlinks are resolved the same way the original store did. Only when
/// even the parent is unresolvable does it fall back to a plain cwd-absolutized (then
/// expanded) path.
///
/// Limitation: a repo added *through a symlink* is stored under the symlink's
/// target; if that symlink later dangles, this fallback resolves to the link path,
/// not the stored target, so `remove` by the original link spelling no longer
/// matches. Removing it by the canonical path shown in `hank repos list` still works.
fn store_path(p: &Path) -> PathBuf {
    let expanded = expand_tilde(p);
    if let Ok(canonical) = std::fs::canonicalize(&expanded) {
        return canonical;
    }
    let parent_canonical = expanded
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .zip(expanded.file_name())
        .and_then(|(parent, name)| Some(std::fs::canonicalize(parent).ok()?.join(name)));
    if let Some(parent_canonical) = parent_canonical {
        return parent_canonical;
    }
    std::path::absolute(&expanded).unwrap_or(expanded)
}

/// Save a mutated roster, mapping the config layer's `anyhow` error into the CLI's
/// `Io` variant (the same bridge `load_roster` uses).
fn save_roster(roster: &Config, paths: &Paths) -> Result<(), CliError> {
    roster
        .save(paths.config_file())
        .map_err(|e| CliError::Io(std::io::Error::other(e)))
}

/// `hank repos add <path>`: canonicalize and append a beads repo to the roster.
///
/// Rejects a directory without a `.beads/` subdir (naming the path and pointing at
/// `bd init`); a path already present (by canonical form) is reported and left as a
/// single entry rather than duplicated.
pub fn run_repos_add(paths: &Paths, path: &Path, out: &mut impl Write) -> Result<(), CliError> {
    let expanded = expand_tilde(path);
    if !expanded.join(".beads").is_dir() {
        return Err(CliError::Roster(format!(
            "not a beads repo: {} has no .beads directory — run `bd init` there first",
            sanitize(&expanded.display().to_string()),
        )));
    }
    let canonical = store_path(path);
    let mut roster = load_roster(paths)?;
    // Normalize each existing entry too, not just the new path: a hand-edited
    // config.toml may hold relative or symlink-aliased entries, and adding the same
    // repo by its canonical spelling must still dedupe (mirrors ensure_hub, which
    // normalizes both sides before comparing).
    if roster
        .repos
        .iter()
        .any(|r| store_path(&r.path) == canonical)
    {
        writeln!(
            out,
            "already in the roster: {}",
            sanitize(&canonical.display().to_string())
        )?;
        return Ok(());
    }
    roster.repos.push(RepoEntry {
        path: canonical.clone(),
    });
    save_roster(&roster, paths)?;
    writeln!(
        out,
        "added {} to the roster",
        sanitize(&canonical.display().to_string())
    )?;
    Ok(())
}

/// `hank repos remove <path>`: drop the entry naming `path` and hint that the hub
/// needs a `hank reset` to forget it. Removing an entry that is not present is a
/// friendly no-op (idempotent), not an error.
///
/// Matching normalizes both the input and each stored entry through `store_path`, so
/// a repo added by one spelling is removed by another (relative or `~`), and a repo
/// directly added and since deleted still matches via the parent-canonicalize
/// fallback. A raw path-string equality is kept as a last resort. The one gap is a
/// repo added via a now-dangling symlink (see `store_path`); remove it by the
/// canonical path from `hank repos list`.
pub fn run_repos_remove(paths: &Paths, path: &Path, out: &mut impl Write) -> Result<(), CliError> {
    let canonical = store_path(path);
    let expanded = expand_tilde(path);
    let mut roster = load_roster(paths)?;
    let before = roster.repos.len();
    roster
        .repos
        .retain(|r| store_path(&r.path) != canonical && r.path != path);
    if roster.repos.len() == before {
        writeln!(
            out,
            "not in the roster: {}",
            sanitize(&expanded.display().to_string())
        )?;
        return Ok(());
    }
    save_roster(&roster, paths)?;
    writeln!(
        out,
        "removed {} from the roster",
        sanitize(&expanded.display().to_string())
    )?;
    // The hub (Slice 3) is additive-only and keeps a removed repo until rebuilt, so
    // point the user at the disposable-rebuild path. Hooks federated-beads-dxh.15
    // without pruning the hub here.
    writeln!(
        out,
        "note: run `hank reset` so the hub drops this repo (the hub is rebuilt from the roster)",
    )?;
    Ok(())
}

/// `hank repos list`: print the roster, one path per line, or a guiding hint when it
/// is empty.
pub fn run_repos_list(paths: &Paths, out: &mut impl Write) -> Result<(), CliError> {
    let roster = load_roster(paths)?;
    if roster.repos.is_empty() {
        writeln!(
            out,
            "roster is empty; add repos with `hank repos add <path>`"
        )?;
        return Ok(());
    }
    writeln!(out, "roster ({} repos):", roster.repos.len())?;
    for entry in &roster.repos {
        writeln!(out, "  {}", sanitize(&entry.path.display().to_string()))?;
    }
    Ok(())
}

/// `hank repos discover <root>`: scan `<root>/*/.beads` one level deep. Lists the new
/// beads repos found (those not already in the roster); with `add`, appends them.
///
/// Preview-first by design (see slice-7 plan): a bare `discover` mutates nothing, so
/// the user sees what a scan turned up before opting into the change with `--add`.
pub fn run_repos_discover(
    paths: &Paths,
    root: &Path,
    add: bool,
    out: &mut impl Write,
) -> Result<(), CliError> {
    let root = expand_tilde(root);
    let mut roster = load_roster(paths)?;
    // Normalize stored entries so a candidate already in the roster under a different
    // spelling (relative/aliased/`~`, e.g. a hand-edited config) is still skipped.
    let known: std::collections::HashSet<PathBuf> =
        roster.repos.iter().map(|r| store_path(&r.path)).collect();

    // One level deep: a child directory is a candidate iff it holds `.beads/`.
    let mut found: Vec<PathBuf> = Vec::new();
    let entries = std::fs::read_dir(&root).map_err(|e| {
        CliError::Roster(format!(
            "cannot scan {}: {e}",
            sanitize(&root.display().to_string())
        ))
    })?;
    for entry in entries {
        let child = entry.map_err(CliError::Io)?.path();
        if child.join(".beads").is_dir() {
            let canonical = store_path(&child);
            if !known.contains(&canonical) && !found.contains(&canonical) {
                found.push(canonical);
            }
        }
    }
    found.sort();

    if found.is_empty() {
        writeln!(
            out,
            "no new beads repos found under {}",
            sanitize(&root.display().to_string())
        )?;
        return Ok(());
    }

    if add {
        for path in &found {
            roster.repos.push(RepoEntry { path: path.clone() });
        }
        save_roster(&roster, paths)?;
        writeln!(
            out,
            "added {} repo(s) from {}:",
            found.len(),
            sanitize(&root.display().to_string())
        )?;
        for path in &found {
            writeln!(out, "  {}", sanitize(&path.display().to_string()))?;
        }
    } else {
        writeln!(
            out,
            "found {} beads repo(s) under {}:",
            found.len(),
            sanitize(&root.display().to_string())
        )?;
        for path in &found {
            writeln!(out, "  {}", sanitize(&path.display().to_string()))?;
        }
        writeln!(out, "re-run with --add to add them")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bd::{BdErrorKind, FakeBdClient, Issue};
    use crate::config::RepoEntry;
    use crate::refresh::HubLock;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    /// Serializes the tests that mutate process-global state (`cwd`, `$HOME`), which
    /// cargo would otherwise run concurrently on separate threads of one process.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn version(v: &str, schema: i64) -> BdVersion {
        BdVersion {
            version: v.to_string(),
            schema_version: schema,
            build: None,
            commit: None,
            branch: None,
        }
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

    /// A repo dir under `base` with a seeded `.beads/metadata.json` prefix, so
    /// refresh/doctor can read a real prefix while bd itself stays faked.
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

    fn row(repo_name: &str, id: &str, priority: i64, title: &str) -> Row {
        Row {
            issue: issue(id, priority, title),
            repo_id: None,
            repo_name: repo_name.to_string(),
            attribution_generation: None,
        }
    }

    #[test]
    fn format_row_matches_spec() {
        let r = row("ra", "ra-2hc", 1, "Ready task one");
        assert_eq!(format_row(&r), "[ra] P1 ra-2hc Ready task one");
    }

    #[test]
    fn format_row_neutralizes_terminal_control_chars() {
        // A hostile title: an OSC 52 clipboard-write escape plus a newline that
        // would otherwise forge a second row.
        let r = row("ra", "ra-2hc", 1, "pwn\u{1b}]52;c;aGk=\u{07}\nfake row");
        let line = format_row(&r);
        assert!(
            !line.contains('\u{1b}') && !line.contains('\n') && !line.contains('\u{07}'),
            "no raw control chars survive: {line:?}"
        );
        assert!(
            line.starts_with("[ra] P1 ra-2hc "),
            "the row prefix is intact: {line:?}"
        );
    }

    #[test]
    fn version_gate_accepts_supported() {
        assert!(version_gate(&version("1.1.0", 1)).is_ok());
        assert!(version_gate(&version("1.2.0", 1)).is_ok());
        assert!(version_gate(&version("2.0.0", 1)).is_ok());
    }

    #[test]
    fn version_gate_rejects_old_version() {
        let msg = version_gate(&version("1.0.0", 1)).expect_err("too old");
        assert!(msg.contains("1.1.0"), "names the requirement: {msg}");
        assert!(msg.contains("1.0.0"), "names what was found: {msg}");
    }

    #[test]
    fn version_gate_rejects_wrong_schema() {
        let msg = version_gate(&version("1.1.0", 2)).expect_err("wrong schema");
        assert!(
            msg.to_lowercase().contains("schema"),
            "message mentions schema: {msg}"
        );
    }

    #[test]
    fn version_gate_rejects_incomplete_version() {
        // Fail closed: an incomplete or non-numeric version must not be trusted
        // even though its numeric prefix would compare >= the minimum.
        for bad in ["1.1", "2", "1.x.0", "", "v1.1.0"] {
            assert!(
                version_gate(&version(bad, 1)).is_err(),
                "incomplete/non-numeric version {bad:?} must fail the gate"
            );
        }
    }

    #[test]
    fn snapshot_prints_rows() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let ra = seed_repo(tmp.path(), "ra", "ra");
        let bd = FakeBdClient::new().with_ready(vec![issue("ra-2hc", 1, "Ready task one")]);
        let mut out = Vec::new();
        let mut err = Vec::new();

        run_snapshot(&roster(&[&ra]), &bd, &paths, false, &mut out, &mut err).expect("ok");

        let stdout = String::from_utf8(out).unwrap();
        assert!(
            stdout.contains("[ra] P1 ra-2hc Ready task one"),
            "human row present: {stdout:?}"
        );
    }

    #[test]
    fn snapshot_releases_hub_lock_before_writing_output() {
        struct LockCheckingWriter {
            hub: PathBuf,
            lock_was_available: bool,
        }

        impl Write for LockCheckingWriter {
            fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
                self.lock_was_available |= HubLock::try_acquire(&self.hub)
                    .expect("lock check")
                    .is_some();
                Ok(buffer.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let ra = seed_repo(tmp.path(), "ra", "ra");
        let bd = FakeBdClient::new().with_ready(vec![issue("ra-2hc", 1, "Ready task one")]);
        let mut out = LockCheckingWriter {
            hub: hub_dir(&paths),
            lock_was_available: false,
        };
        let mut err = Vec::new();

        run_snapshot(&roster(&[&ra]), &bd, &paths, false, &mut out, &mut err).expect("ok");

        assert!(
            out.lock_was_available,
            "snapshot output must not retain the completed refresh lock"
        );
    }

    #[test]
    fn snapshot_json_emits_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let ra = seed_repo(tmp.path(), "ra", "ra");
        let bd = FakeBdClient::new().with_ready(vec![issue("ra-2hc", 1, "Ready task one")]);
        let mut out = Vec::new();
        let mut err = Vec::new();

        run_snapshot(&roster(&[&ra]), &bd, &paths, true, &mut out, &mut err).expect("ok");

        // stdout must be clean JSON (no warning leaked in).
        let v: serde_json::Value =
            serde_json::from_slice(&out).expect("stdout parses as JSON snapshot");
        assert_eq!(
            v["rows"][0]["issue"]["id"].as_str(),
            Some("ra-2hc"),
            "serialized snapshot exposes the row's issue id: {v}"
        );
        assert!(v.get("fetched_at").is_some(), "fetch time serialized: {v}");
    }

    #[test]
    fn snapshot_surfaces_per_repo_warnings_without_aborting() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let ra = seed_repo(tmp.path(), "ra", "ra");
        let rb = seed_repo(tmp.path(), "rb", "rb");
        let bd = FakeBdClient::new()
            .with_ready(vec![issue("ra-2hc", 1, "Ready task one")])
            .with_export_err(
                rb.clone(),
                BdError {
                    command: "bd export".into(),
                    stderr: "disk full".into(),
                    kind: BdErrorKind::NonZeroExit { code: Some(1) },
                },
            );
        let mut out = Vec::new();
        let mut err = Vec::new();

        run_snapshot(&roster(&[&ra, &rb]), &bd, &paths, false, &mut out, &mut err).expect("ok");

        let stdout = String::from_utf8(out).unwrap();
        let stderr = String::from_utf8(err).unwrap();
        assert!(
            stdout.contains("ra-2hc"),
            "healthy rows still printed: {stdout:?}"
        );
        assert!(
            stderr.contains("rb"),
            "the failed repo is warned about on err: {stderr:?}"
        );
    }

    #[test]
    fn snapshot_already_refreshing_degrades() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let ra = seed_repo(tmp.path(), "ra", "ra");
        // Pre-hold the hub lock so refresh declines.
        let hub = hub_dir(&paths);
        fs::create_dir_all(&hub).unwrap();
        let _held = HubLock::try_acquire(&hub).unwrap().expect("acquired lock");

        let bd = FakeBdClient::new().with_ready(vec![issue("ra-2hc", 1, "Ready task one")]);
        let mut out = Vec::new();
        let mut err = Vec::new();

        run_snapshot(&roster(&[&ra]), &bd, &paths, false, &mut out, &mut err)
            .expect("degrades, ok");

        let stdout = String::from_utf8(out).unwrap();
        let stderr = String::from_utf8(err).unwrap();
        assert!(
            stdout.contains("ra-2hc"),
            "stale rows still printed: {stdout:?}"
        );
        assert!(
            stderr.to_lowercase().contains("refreshing"),
            "AlreadyRefreshing warned on err: {stderr:?}"
        );
    }

    #[test]
    fn snapshot_version_gate_failure_is_fatal() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let ra = seed_repo(tmp.path(), "ra", "ra");
        let bd = FakeBdClient::new().with_version(version("1.0.0", 1));
        let mut out = Vec::new();
        let mut err = Vec::new();

        let e = run_snapshot(&roster(&[&ra]), &bd, &paths, false, &mut out, &mut err)
            .expect_err("gate is fatal");
        assert!(matches!(e, CliError::VersionGate(_)), "got {e:?}");
        assert!(out.is_empty(), "no snapshot printed when the gate fails");
    }

    #[test]
    fn snapshot_sync_failure_is_fatal() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let ra = seed_repo(tmp.path(), "ra", "ra");
        let bd = FakeBdClient::new().with_repo_sync_err(BdError {
            command: "bd repo sync".into(),
            stderr: "boom".into(),
            kind: BdErrorKind::NonZeroExit { code: Some(1) },
        });
        let mut out = Vec::new();
        let mut err = Vec::new();

        let e = run_snapshot(&roster(&[&ra]), &bd, &paths, false, &mut out, &mut err)
            .expect_err("sync failure is fatal");
        assert!(
            matches!(e, CliError::Refresh(RefreshError::Sync(_))),
            "got {e:?}"
        );
    }

    #[test]
    #[allow(non_snake_case)] // name mirrors the `MISSING` marker under test
    fn doctor_lists_missing_repo_as_MISSING() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let ra = seed_repo(tmp.path(), "ra", "ra");
        let gone = tmp.path().join("gone");
        // Doctor loads the roster from the config file itself, so persist one.
        roster(&[&ra, &gone]).save(paths.config_file()).unwrap();
        let bd = FakeBdClient::new(); // default version 1.1.0 / schema 1
        let mut out = Vec::new();

        run_doctor(&bd, &paths, &mut out).expect("ok");

        let stdout = String::from_utf8(out).unwrap();
        assert!(stdout.contains("1.1.0"), "bd version reported: {stdout}");
        assert!(
            stdout.contains(&paths.config_file().display().to_string()),
            "config path reported: {stdout}"
        );
        assert!(
            stdout.contains(&hub_dir(&paths).display().to_string()),
            "hub path reported: {stdout}"
        );
        assert!(
            stdout.contains("ra"),
            "existing repo prefix reported: {stdout}"
        );
        assert!(
            stdout.contains("MISSING"),
            "absent repo flagged MISSING: {stdout}"
        );
    }

    #[test]
    fn doctor_reports_gate_status() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let ok = FakeBdClient::new().with_version(version("1.1.0", 1));
        let bad = FakeBdClient::new().with_version(version("1.0.0", 1));

        // No config file present: doctor loads an empty roster and still runs.
        let mut out_ok = Vec::new();
        run_doctor(&ok, &paths, &mut out_ok).expect("ok");
        assert!(
            String::from_utf8(out_ok).unwrap().contains("gate: OK"),
            "supported bd shows gate: OK"
        );

        let mut out_bad = Vec::new();
        run_doctor(&bad, &paths, &mut out_bad).expect("ok");
        assert!(
            String::from_utf8(out_bad).unwrap().contains("gate: FAIL"),
            "old bd shows gate: FAIL"
        );
    }

    #[test]
    fn doctor_reports_malformed_config_without_failing() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        // A present-but-invalid config must be reported, not abort doctor.
        let config_file = paths.config_file();
        fs::create_dir_all(config_file.parent().unwrap()).unwrap();
        fs::write(config_file, "this is not = valid toml [[[").unwrap();
        let bd = FakeBdClient::new();
        let mut out = Vec::new();

        run_doctor(&bd, &paths, &mut out).expect("doctor still succeeds");

        let stdout = String::from_utf8(out).unwrap();
        assert!(stdout.contains("1.1.0"), "version still reported: {stdout}");
        assert!(
            stdout.contains("roster: ERROR"),
            "malformed config surfaced as a reported line: {stdout}"
        );
    }

    #[test]
    fn doctor_reports_legacy_migration_problems_without_silently_emptying_roster() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let legacy = tmp.path().join("federated-beads/config.toml");
        fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        fs::write(&legacy, "repos = [not valid TOML").unwrap();
        let bd = FakeBdClient::new();
        let mut out = Vec::new();

        run_doctor(&bd, &paths, &mut out).expect("doctor still succeeds");

        let stdout = String::from_utf8(out).unwrap();
        assert!(stdout.contains("legacy migration: ERROR"), "{stdout}");
        assert!(stdout.contains(&legacy.display().to_string()), "{stdout}");
        assert!(
            stdout.contains("roster (0 repos)") || stdout.contains("roster: ERROR"),
            "doctor continues through the complete report: {stdout}"
        );
        assert!(!paths.config_file().exists());
    }

    #[test]
    fn load_roster_absent_is_empty_but_invalid_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        // No config file: a genuine first run yields an empty roster.
        assert_eq!(load_roster(&paths).unwrap(), Config::default());

        // Present but malformed: surfaced as an error, never silently empty.
        let config_file = paths.config_file();
        fs::create_dir_all(config_file.parent().unwrap()).unwrap();
        fs::write(config_file, "not = [valid").unwrap();
        assert!(load_roster(&paths).is_err(), "invalid config must error");
    }

    #[test]
    fn load_roster_absolutizes_relative_entries_at_the_config_boundary() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let config_dir = paths
            .config_file()
            .parent()
            .expect("config file has a parent");
        let repo = seed_repo(config_dir, "repo", "repo");
        let launch_dir = tmp.path().join("elsewhere");
        fs::create_dir_all(&launch_dir).unwrap();
        Config {
            repos: vec![RepoEntry {
                path: PathBuf::from("repo"),
            }],
        }
        .save(paths.config_file())
        .unwrap();

        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let previous_dir = std::env::current_dir().unwrap();
        std::env::set_current_dir(launch_dir).unwrap();
        let loaded = load_roster(&paths);
        std::env::set_current_dir(previous_dir).unwrap();

        assert_eq!(
            loaded.unwrap().repos[0].path,
            fs::canonicalize(repo).unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn load_roster_dangling_symlink_errors_not_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let config_file = paths.config_file();
        fs::create_dir_all(config_file.parent().unwrap()).unwrap();
        // A config symlink to a nonexistent target: `Path::exists()` is false, but
        // it is not a first run — the misconfiguration must surface, not be masked.
        std::os::unix::fs::symlink(tmp.path().join("nowhere.toml"), config_file).unwrap();
        assert!(!config_file.exists(), "precondition: dangling symlink");

        assert!(
            load_roster(&paths).is_err(),
            "a dangling config symlink must error, not read as empty"
        );
    }

    #[test]
    fn reset_removes_hub_and_reports() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let hub = hub_dir(&paths);
        fs::create_dir_all(hub.join(".beads")).unwrap();
        fs::write(hub.join(".beads").join("marker"), "x").unwrap();
        let mut out = Vec::new();

        run_reset(&paths, &mut out).expect("ok");

        assert!(!hub.exists(), "hub dir removed");
        let stdout = String::from_utf8(out).unwrap();
        assert!(
            stdout.contains(&hub.display().to_string()),
            "reset reports the removed path: {stdout}"
        );
    }

    #[test]
    fn reset_clears_the_snapshot_cache() {
        // A launch right after `hank reset` must not paint rows from the
        // just-discarded hub, so reset clears the cache alongside it
        // (federated-beads review finding: cache survived reset otherwise).
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let hub = hub_dir(&paths);
        fs::create_dir_all(&hub).unwrap();
        crate::cache::save(
            paths.cache_file(),
            &crate::snapshot::Snapshot {
                rows: Vec::new(),
                fetched_at: SystemTime::now(),
            },
            &Config::default(),
        )
        .expect("seed cache");
        crate::ui_state::save(
            paths.ui_state_file(),
            &crate::app::RepoFilter::Only("repo-a".into()),
        )
        .expect("seed UI state");
        let mut out = Vec::new();

        run_reset(&paths, &mut out).expect("ok");

        assert!(
            !paths.cache_file().exists(),
            "reset removes the snapshot cache along with the hub"
        );
        assert_eq!(
            crate::ui_state::load(paths.ui_state_file()),
            crate::app::RepoFilter::Only("repo-a".into()),
            "reset preserves the user's repository preference"
        );
    }

    #[test]
    fn reset_never_touches_legacy_user_or_derived_state() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let legacy = tmp.path().join("federated-beads");
        fs::create_dir_all(legacy.join("hub")).unwrap();
        fs::write(legacy.join("config.toml"), "repos = []\n").unwrap();
        fs::write(legacy.join("ui_state.json"), r#"{"version":2}"#).unwrap();
        fs::write(legacy.join("snapshot_cache.json"), "legacy cache").unwrap();
        fs::create_dir_all(hub_dir(&paths)).unwrap();
        fs::write(paths.cache_file(), "canonical cache").unwrap();
        let mut out = Vec::new();

        run_reset(&paths, &mut out).expect("ok");

        assert!(legacy.join("hub").exists());
        assert!(legacy.join("config.toml").exists());
        assert!(legacy.join("ui_state.json").exists());
        assert!(legacy.join("snapshot_cache.json").exists());
        assert!(!hub_dir(&paths).exists());
        assert!(!paths.cache_file().exists());
    }

    #[cfg(unix)]
    #[test]
    fn reset_reports_removed_for_dangling_hub_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let hub = hub_dir(&paths);
        fs::create_dir_all(hub.parent().unwrap()).unwrap();
        // A dangling symlink: `exists()` is false, but reset removes it, so the
        // report must say "removed", not "nothing to remove".
        std::os::unix::fs::symlink(tmp.path().join("nowhere"), &hub).unwrap();
        let mut out = Vec::new();

        run_reset(&paths, &mut out).expect("ok");

        let stdout = String::from_utf8(out).unwrap();
        assert!(
            stdout.contains("removed") && !stdout.contains("nothing to remove"),
            "a removed dangling symlink is reported as removed: {stdout}"
        );
        assert!(
            fs::symlink_metadata(&hub).is_err(),
            "the dangling symlink was actually removed"
        );
    }

    /// Load the persisted roster back from disk to assert on what was saved.
    fn reload(paths: &Paths) -> Config {
        Config::load(paths.config_file()).unwrap()
    }

    #[test]
    fn add_appends_and_dedupes() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let ra = seed_repo(tmp.path(), "ra", "ra");
        let mut out = Vec::new();

        run_repos_add(&paths, &ra, &mut out).expect("add ok");
        assert_eq!(reload(&paths).repos.len(), 1, "one entry after first add");
        assert!(
            String::from_utf8(out).unwrap().contains("added"),
            "first add reports success"
        );

        // Adding the same path again must not duplicate it.
        let mut out2 = Vec::new();
        run_repos_add(&paths, &ra, &mut out2).expect("second add ok");
        assert_eq!(
            reload(&paths).repos.len(),
            1,
            "duplicate add does not grow the roster"
        );
        assert!(
            String::from_utf8(out2)
                .unwrap()
                .contains("already in the roster"),
            "duplicate add is reported, not silent"
        );
    }

    #[test]
    fn add_rejects_dir_without_beads() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let plain = tmp.path().join("not-a-repo");
        fs::create_dir_all(&plain).unwrap();
        let mut out = Vec::new();

        let e = run_repos_add(&paths, &plain, &mut out).expect_err("rejects non-repo");
        let msg = match &e {
            CliError::Roster(m) => m.clone(),
            other => panic!("expected Roster error, got {other:?}"),
        };
        assert!(
            msg.contains(&plain.display().to_string()),
            "error names the offending path: {msg}"
        );
        assert!(msg.contains("bd init"), "error hints at `bd init`: {msg}");
        // A rejected add writes no config.
        assert!(
            std::fs::symlink_metadata(paths.config_file()).is_err(),
            "rejected add persists nothing"
        );
    }

    #[test]
    fn remove_by_path() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let ra = seed_repo(tmp.path(), "ra", "ra");
        let rb = seed_repo(tmp.path(), "rb", "rb");
        run_repos_add(&paths, &ra, &mut Vec::new()).unwrap();
        run_repos_add(&paths, &rb, &mut Vec::new()).unwrap();

        let mut out = Vec::new();
        run_repos_remove(&paths, &ra, &mut out).expect("remove ok");

        let remaining = reload(&paths);
        assert_eq!(remaining.repos.len(), 1, "one entry left");
        assert!(
            remaining.repos[0].path.ends_with("rb"),
            "the untouched repo remains: {:?}",
            remaining.repos
        );
        assert!(
            String::from_utf8(out).unwrap().contains("removed"),
            "removal is reported"
        );
    }

    #[test]
    fn remove_hints_reset() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let ra = seed_repo(tmp.path(), "ra", "ra");
        run_repos_add(&paths, &ra, &mut Vec::new()).unwrap();

        let mut out = Vec::new();
        run_repos_remove(&paths, &ra, &mut out).expect("remove ok");
        assert!(
            String::from_utf8(out).unwrap().contains("hank reset"),
            "remove hints that the hub needs a reset (federated-beads-dxh.15)"
        );
    }

    #[test]
    fn remove_missing_is_friendly() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let ghost = tmp.path().join("never-added");
        let mut out = Vec::new();

        run_repos_remove(&paths, &ghost, &mut out).expect("missing remove is not fatal");
        assert!(
            String::from_utf8(out)
                .unwrap()
                .contains("not in the roster"),
            "removing an absent entry is reported, not an error"
        );
    }

    #[test]
    fn list_prints_roster() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());

        // Empty roster: the hint, not a bare blank.
        let mut empty = Vec::new();
        run_repos_list(&paths, &mut empty).expect("list ok");
        assert!(
            String::from_utf8(empty)
                .unwrap()
                .contains("roster is empty"),
            "empty roster prints a guiding hint"
        );

        let ra = seed_repo(tmp.path(), "ra", "ra");
        let rb = seed_repo(tmp.path(), "rb", "rb");
        run_repos_add(&paths, &ra, &mut Vec::new()).unwrap();
        run_repos_add(&paths, &rb, &mut Vec::new()).unwrap();

        let mut out = Vec::new();
        run_repos_list(&paths, &mut out).expect("list ok");
        let stdout = String::from_utf8(out).unwrap();
        assert!(stdout.contains("roster (2 repos)"), "count shown: {stdout}");
        assert!(
            stdout.contains(&ra.canonicalize().unwrap().display().to_string())
                && stdout.contains(&rb.canonicalize().unwrap().display().to_string()),
            "both roster paths listed: {stdout}"
        );
    }

    /// A discovery tree: `<root>/{x/.beads, y/.beads, z}` — z has no `.beads`.
    fn discovery_tree(base: &Path) -> (PathBuf, PathBuf, PathBuf) {
        let root = base.join("scan-root");
        let x = seed_repo(&root, "x", "x");
        let y = seed_repo(&root, "y", "y");
        fs::create_dir_all(root.join("z")).unwrap();
        (root, x, y)
    }

    #[test]
    fn discover_finds_beads_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let (root, x, y) = discovery_tree(tmp.path());
        let mut out = Vec::new();

        run_repos_discover(&paths, &root, false, &mut out).expect("discover ok");

        let stdout = String::from_utf8(out).unwrap();
        assert!(
            stdout.contains(&x.canonicalize().unwrap().display().to_string()),
            "finds x: {stdout}"
        );
        assert!(
            stdout.contains(&y.canonicalize().unwrap().display().to_string()),
            "finds y: {stdout}"
        );
        assert!(
            !stdout.contains("/z\n") && !stdout.ends_with("/z"),
            "skips z: {stdout}"
        );
        // Default discover is preview-only: nothing persisted.
        assert!(
            std::fs::symlink_metadata(paths.config_file()).is_err(),
            "discover without --add mutates nothing"
        );
    }

    #[test]
    fn discover_skips_already_added() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let (root, x, y) = discovery_tree(tmp.path());
        run_repos_add(&paths, &x, &mut Vec::new()).unwrap();

        let mut out = Vec::new();
        run_repos_discover(&paths, &root, false, &mut out).expect("discover ok");

        let stdout = String::from_utf8(out).unwrap();
        assert!(
            !stdout.contains(&x.canonicalize().unwrap().display().to_string()),
            "an already-rostered repo is not re-offered: {stdout}"
        );
        assert!(
            stdout.contains(&y.canonicalize().unwrap().display().to_string()),
            "the new repo is still offered: {stdout}"
        );
    }

    #[test]
    fn discover_add_persists() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let (root, x, y) = discovery_tree(tmp.path());
        let mut out = Vec::new();

        run_repos_discover(&paths, &root, true, &mut out).expect("discover --add ok");

        let saved: Vec<PathBuf> = reload(&paths).repos.into_iter().map(|r| r.path).collect();
        assert!(
            saved.contains(&x.canonicalize().unwrap())
                && saved.contains(&y.canonicalize().unwrap()),
            "both discovered repos persisted (canonical): {saved:?}"
        );
        assert!(
            String::from_utf8(out).unwrap().contains("added"),
            "add mode reports what it added"
        );
    }

    #[test]
    fn remove_after_repo_deleted_still_matches() {
        // The remove-after-deletion case: add a repo (stored canonical), delete it
        // from disk, then remove it by its original relative spelling from the parent.
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let repo = seed_repo(tmp.path(), "doomed", "dm");
        run_repos_add(&paths, &repo, &mut Vec::new()).unwrap();
        fs::remove_dir_all(&repo).unwrap();

        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Remove by a relative path resolved from the parent dir (repo is gone).
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();
        let mut out = Vec::new();
        let result = run_repos_remove(&paths, Path::new("doomed"), &mut out);
        std::env::set_current_dir(prev).unwrap();
        result.expect("remove ok");

        assert!(
            reload(&paths).repos.is_empty(),
            "the stale entry was removed"
        );
        assert!(
            String::from_utf8(out).unwrap().contains("removed"),
            "deleted repo removed by its original relative spelling"
        );
    }

    #[test]
    fn add_dedupes_against_relative_hand_edited_entry() {
        // A config a user hand-edited to hold a relative entry: adding the same repo
        // by its canonical absolute path must dedupe against it, not duplicate.
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let config_dir = paths
            .config_file()
            .parent()
            .expect("config file has a parent");
        let repo = seed_repo(config_dir, "r", "r");
        let launch_dir = tmp.path().join("elsewhere");
        fs::create_dir_all(&launch_dir).unwrap();
        // Persist a roster whose entry is relative to the config directory.
        Config {
            repos: vec![RepoEntry {
                path: PathBuf::from("r"),
            }],
        }
        .save(paths.config_file())
        .unwrap();

        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(launch_dir).unwrap();
        let mut out = Vec::new();
        // Add by the canonical absolute path.
        let result = run_repos_add(&paths, &repo.canonicalize().unwrap(), &mut out);
        std::env::set_current_dir(prev).unwrap();
        result.expect("add ok");

        assert_eq!(
            reload(&paths).repos.len(),
            1,
            "aliased hand-edited entry is deduped, not duplicated"
        );
        assert!(
            String::from_utf8(out)
                .unwrap()
                .contains("already in the roster"),
            "the pre-existing aliased entry is recognized"
        );
    }

    #[test]
    fn discover_skips_relative_hand_edited_entry() {
        // discover must also normalize stored entries: an already-added repo spelled
        // relatively in the config should not be re-offered.
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        let (root, x, _y) = discovery_tree(tmp.path());
        // Hand-edited roster: x stored relative to the config directory, which
        // is one level below the injected base.
        let rel_x = PathBuf::from("..").join(x.strip_prefix(tmp.path()).expect("x is under base"));
        Config {
            repos: vec![RepoEntry { path: rel_x }],
        }
        .save(paths.config_file())
        .unwrap();

        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();
        let mut out = Vec::new();
        let result = run_repos_discover(&paths, &root, false, &mut out);
        std::env::set_current_dir(prev).unwrap();
        result.expect("discover ok");

        assert!(
            !String::from_utf8(out)
                .unwrap()
                .contains(&x.canonicalize().unwrap().display().to_string()),
            "an already-rostered repo (relative spelling) is not re-offered"
        );
    }

    #[test]
    fn add_expands_tilde_and_canonicalizes() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        // Point HOME at a tempdir holding a seeded repo, then add it via `~/r`.
        let home = tmp.path().join("home");
        let r = seed_repo(&home, "r", "r");
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var_os("HOME");
        // SAFETY: ENV_LOCK serializes HOME/cwd mutation across tests; restored below.
        unsafe { std::env::set_var("HOME", &home) };
        let mut out = Vec::new();
        let result = run_repos_add(&paths, Path::new("~/r"), &mut out);
        match prev {
            Some(v) => unsafe { std::env::set_var("HOME", v) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        result.expect("tilde add ok");

        let stored = &reload(&paths).repos[0].path;
        assert!(stored.is_absolute(), "stored path is absolute: {stored:?}");
        assert!(
            !stored.to_string_lossy().contains('~'),
            "tilde expanded, not stored literally: {stored:?}"
        );
        assert_eq!(
            stored,
            &r.canonicalize().unwrap(),
            "stored path is the canonical repo path"
        );
    }

    #[test]
    fn snapshot_warnings_are_sanitized() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(tmp.path());
        // A roster path carrying terminal-control bytes: it does not exist, so
        // ensure_hub emits a "does not exist" warning that echoes the raw path.
        let hostile = tmp.path().join("evil\u{1b}]52;c;x\u{07}\nforged");
        let bd = FakeBdClient::new().with_ready(vec![issue("ra-2hc", 1, "t")]);
        let mut out = Vec::new();
        let mut err = Vec::new();

        run_snapshot(&roster(&[&hostile]), &bd, &paths, false, &mut out, &mut err).expect("ok");

        let stderr = String::from_utf8(err).unwrap();
        assert!(
            !stderr.contains('\u{1b}') && !stderr.contains('\u{07}'),
            "warning output carries no raw terminal-control bytes: {stderr:?}"
        );
    }
}
