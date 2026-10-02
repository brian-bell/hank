//! Gated end-to-end tests against a real `bd` binary.
//!
//! Every test here first checks whether `bd` is installed; if not, it prints
//! `SKIP: bd not installed` and returns early so `cargo test --test
//! bd_integration` is always green regardless of environment. These tests are
//! the schema-drift tripwire: they drive the real `BdCli` against real repos.

mod helpers;

use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, SystemTime};

use hank::app::{Msg, RefreshScope};
use hank::bd::{BdCli, BdClient, RepoSyncReport};
use hank::cli::{load_roster, run_repos_add, run_repos_remove, run_repos_watch, run_snapshot};
use hank::config::{Config, Paths, RepoEntry};
use hank::hub::{ensure_hub, hub_dir, read_hub_roster};
use hank::watch::{BdJournal, Checkpoints, WatchList, Watcher};
use hank::{refresh, snapshot};
use helpers::{
    bd_available, bd_has_events, bd_in, build_ready_fixture_repo,
    build_ready_fixture_repo_with_prefix, create_closed_issue,
};

#[test]
fn bd_probe_skips_cleanly_when_absent() {
    if !bd_available() {
        eprintln!("SKIP: bd not installed");
    }
}

#[test]
fn version_and_ready_roundtrip() {
    if !bd_available() {
        eprintln!("SKIP: bd not installed");
        return;
    }

    let cli = BdCli::new();

    // Version gate parses and reports the expected schema.
    let v = cli.version().expect("bd version --json");
    assert_eq!(v.schema_version, 1, "unexpected bd schema_version");

    // Build a real fixture repo: 3 issues, the third blocked by the second.
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("ra");
    std::fs::create_dir_all(&repo).expect("mkdir repo");
    build_ready_fixture_repo(&repo);

    // `ready` reads the repo's own hydrated data; the blocked issue is excluded.
    let ready = cli.ready(&repo).expect("bd ready --json");
    assert_eq!(
        ready.len(),
        2,
        "expected 2 of 3 issues ready (blocked excluded), got: {:?}",
        ready.iter().map(|i| &i.id).collect::<Vec<_>>()
    );
    assert!(
        ready.iter().all(|i| i.id.starts_with("ra-")),
        "ids carry the configured prefix"
    );
}

#[test]
fn ensure_hub_end_to_end() {
    if !bd_available() {
        eprintln!("SKIP: bd not installed");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    // Two fixture repos with distinct prefixes.
    let ra = tmp.path().join("ra");
    let rb = tmp.path().join("rb");
    std::fs::create_dir_all(&ra).expect("mkdir ra");
    std::fs::create_dir_all(&rb).expect("mkdir rb");
    build_ready_fixture_repo_with_prefix(&ra, "ra");
    build_ready_fixture_repo_with_prefix(&rb, "rb");

    // Hub lives under the injected data dir; roster names both repos.
    let paths = Paths::with_base(tmp.path());
    let roster = Config {
        watch: false,
        repos: vec![RepoEntry::new(ra.clone()), RepoEntry::new(rb.clone())],
    };

    let status = ensure_hub(&BdCli::new(), &paths, &roster).expect("ensure_hub");
    assert!(
        status.warnings.is_empty(),
        "both repos exist, so no warnings: {:?}",
        status.warnings
    );

    // The chosen roster-read path (config.yaml) reflects both repos, canonicalized
    // as bd stores them.
    let hub = hub_dir(&paths);
    let tracked = read_hub_roster(&hub).expect("read hub roster");
    let canon = |p: &std::path::Path| std::fs::canonicalize(p).unwrap();
    assert!(
        tracked.contains(&canon(&ra)),
        "hub roster lists ra: {tracked:?}"
    );
    assert!(
        tracked.contains(&canon(&rb)),
        "hub roster lists rb: {tracked:?}"
    );

    // Idempotent: a second ensure_hub adds nothing and stays clean.
    let again = ensure_hub(&BdCli::new(), &paths, &roster).expect("ensure_hub again");
    assert!(again.warnings.is_empty());
    let tracked_again = read_hub_roster(&hub).expect("read hub roster again");
    assert_eq!(
        tracked_again.len(),
        tracked.len(),
        "second ensure_hub must not duplicate repos"
    );
}

#[test]
fn refresh_two_repos() {
    if !bd_available() {
        eprintln!("SKIP: bd not installed");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    // Two fixture repos with distinct prefixes.
    let ra = tmp.path().join("ra");
    let rb = tmp.path().join("rb");
    std::fs::create_dir_all(&ra).expect("mkdir ra");
    std::fs::create_dir_all(&rb).expect("mkdir rb");
    build_ready_fixture_repo_with_prefix(&ra, "ra");
    build_ready_fixture_repo_with_prefix(&rb, "rb");

    let paths = Paths::with_base(tmp.path());
    let roster = Config {
        watch: false,
        repos: vec![RepoEntry::new(ra.clone()), RepoEntry::new(rb.clone())],
    };

    // The hub must be registered before a refresh can sync it.
    ensure_hub(&BdCli::new(), &paths, &roster).expect("ensure_hub");

    // Create an issue in `ra` AFTER the fixture helper's own export. Only
    // refresh's export (writing into ra's own .beads) can carry this into the
    // hub — so its appearance below proves refresh exported to the right repo,
    // not the caller's cwd (regression guard for the relative-`-o` bug).
    let marker = "refresh-export-marker";
    let created = std::process::Command::new("bd")
        .arg("-C")
        .arg(&ra)
        .args(["create", marker, "-p", "1", "--json"])
        .output()
        .expect("bd create marker");
    assert!(
        created.status.success(),
        "bd create marker failed: {}",
        String::from_utf8_lossy(&created.stderr)
    );

    let outcome = refresh::run(&BdCli::new(), &roster, &paths).expect("refresh runs");
    assert!(
        outcome.errors.is_empty(),
        "both repos are healthy, so no per-repo errors: {:?}",
        outcome.errors
    );
    assert!(
        outcome.prefix_map.collisions().is_empty(),
        "distinct prefixes, so no collisions: {:?}",
        outcome.prefix_map.collisions()
    );

    // The hub now hydrates issues from both repos (blocked ones excluded).
    let hub = hub_dir(&paths);
    let ready = BdCli::new().ready(&hub).expect("bd ready on hub");
    assert!(
        ready.iter().any(|i| i.title == marker),
        "refresh's export must carry the post-ensure marker into the hub: {:?}",
        ready.iter().map(|i| &i.title).collect::<Vec<_>>()
    );
    let ra_id = ready
        .iter()
        .find(|i| i.id.starts_with("ra-"))
        .map(|i| i.id.clone())
        .expect("an ra- issue is ready in the hub");
    let rb_id = ready
        .iter()
        .find(|i| i.id.starts_with("rb-"))
        .map(|i| i.id.clone())
        .expect("an rb- issue is ready in the hub");

    // The prefix map attributes each hub id back to its source repo.
    let canon = |p: &std::path::Path| std::fs::canonicalize(p).unwrap();
    assert_eq!(
        outcome.prefix_map.repo_for(&ra_id).map(|r| canon(&r.path)),
        Some(canon(&ra)),
        "ra id attributes to the ra repo"
    );
    assert_eq!(
        outcome.prefix_map.repo_for(&rb_id).map(|r| canon(&r.path)),
        Some(canon(&rb)),
        "rb id attributes to the rb repo"
    );
}

#[test]
fn unchanged_refresh_preserves_export_mtime_and_uses_sync_cache() {
    if !bd_available() {
        eprintln!("SKIP: bd not installed");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).expect("mkdir repo");
    build_ready_fixture_repo_with_prefix(&repo, "ra");
    let paths = Paths::with_base(tmp.path());
    let roster = Config {
        watch: false,
        repos: vec![RepoEntry::new(repo.clone())],
    };
    ensure_hub(&BdCli::new(), &paths, &roster).expect("ensure hub");

    refresh::run(&BdCli::new(), &roster, &paths).expect("initial refresh");
    let canonical = repo.join(".beads").join("issues.jsonl");
    let before = std::fs::metadata(&canonical)
        .expect("canonical metadata")
        .modified()
        .expect("canonical mtime");

    let warm = refresh::run(&BdCli::new(), &roster, &paths).expect("warm refresh");

    assert_eq!(
        std::fs::metadata(&canonical)
            .expect("canonical metadata after warm refresh")
            .modified()
            .expect("canonical mtime after warm refresh"),
        before,
        "byte-identical exports preserve the canonical inode mtime"
    );
    assert_eq!(warm.sync_report, RepoSyncReport::UpToDate);
}

#[test]
fn refresh_attributes_hyphenated_repo() {
    // Regression for dxh.17: a repo whose real id prefix contains a hyphen
    // (`ready-fix`) is stored by bd with an underscore-sanitized dolt_database
    // (`ready_fix`). Attribution must key off the real id prefix, not the
    // sanitized DB name, so its ids don't fall into the unknown bucket.
    if !bd_available() {
        eprintln!("SKIP: bd not installed");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("reading-lite");
    std::fs::create_dir_all(&repo).expect("mkdir repo");
    build_ready_fixture_repo_with_prefix(&repo, "ready-fix");

    let paths = Paths::with_base(tmp.path());
    let roster = Config {
        watch: false,
        repos: vec![RepoEntry::new(repo.clone())],
    };

    ensure_hub(&BdCli::new(), &paths, &roster).expect("ensure_hub");
    let outcome = refresh::run(&BdCli::new(), &roster, &paths).expect("refresh runs");
    assert!(
        outcome.errors.is_empty(),
        "the repo is healthy, so no per-repo errors: {:?}",
        outcome.errors
    );
    assert!(
        outcome.prefix_map.collisions().is_empty(),
        "single repo, so no collisions: {:?}",
        outcome.prefix_map.collisions()
    );

    let hub = hub_dir(&paths);
    let ready = BdCli::new().ready(&hub).expect("bd ready on hub");
    let id = ready
        .iter()
        .find(|i| i.id.starts_with("ready-fix-"))
        .map(|i| i.id.clone())
        .expect("a hyphenated ready-fix- id is ready in the hub");

    let canon = |p: &std::path::Path| std::fs::canonicalize(p).unwrap();
    assert_eq!(
        outcome.prefix_map.repo_for(&id).map(|r| canon(&r.path)),
        Some(canon(&repo)),
        "a hyphenated id attributes to its repo, not the unknown bucket"
    );
}

#[test]
fn snapshot_command_end_to_end() {
    if !bd_available() {
        eprintln!("SKIP: bd not installed");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    // Two fixture repos with distinct prefixes and matching directory basenames.
    let ra = tmp.path().join("ra");
    let rb = tmp.path().join("rb");
    std::fs::create_dir_all(&ra).expect("mkdir ra");
    std::fs::create_dir_all(&rb).expect("mkdir rb");
    build_ready_fixture_repo_with_prefix(&ra, "ra");
    build_ready_fixture_repo_with_prefix(&rb, "rb");

    let paths = Paths::with_base(tmp.path());
    let roster = Config {
        watch: false,
        repos: vec![RepoEntry::new(ra.clone()), RepoEntry::new(rb.clone())],
    };

    // Drive the full ensure_hub -> refresh -> fetch -> print path via the real
    // CLI runner and the real BdCli.
    let mut out = Vec::new();
    let mut err = Vec::new();
    run_snapshot(&roster, &BdCli::new(), &paths, false, &mut out, &mut err)
        .expect("run_snapshot succeeds against real fixture repos");

    let stdout = String::from_utf8(out).expect("utf8 stdout");
    // Both repos' ready issues appear, attributed by directory basename, with the
    // shared fixture title.
    assert!(
        stdout
            .lines()
            .any(|l| l.starts_with("[ra] ") && l.contains("ra-")),
        "an ra-attributed row is present: {stdout:?}"
    );
    assert!(
        stdout
            .lines()
            .any(|l| l.starts_with("[rb] ") && l.contains("rb-")),
        "an rb-attributed row is present: {stdout:?}"
    );
    assert!(
        stdout.contains("Ready task one"),
        "the fixture's ready title is printed: {stdout:?}"
    );
}

#[test]
fn repos_remove_prunes_the_hub_end_to_end() {
    if !bd_available() {
        eprintln!("SKIP: bd not installed");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let ra = tmp.path().join("ra");
    let rb = tmp.path().join("rb");
    std::fs::create_dir_all(&ra).expect("mkdir ra");
    std::fs::create_dir_all(&rb).expect("mkdir rb");
    build_ready_fixture_repo_with_prefix(&ra, "ra");
    build_ready_fixture_repo_with_prefix(&rb, "rb");

    let paths = Paths::with_base(tmp.path());
    run_repos_add(&BdCli::new(), &paths, &ra, &mut Vec::new()).expect("add ra");
    run_repos_add(&BdCli::new(), &paths, &rb, &mut Vec::new()).expect("add rb");
    let roster = load_roster(&paths).expect("roster");
    run_snapshot(
        &roster,
        &BdCli::new(),
        &paths,
        false,
        &mut Vec::new(),
        &mut Vec::new(),
    )
    .expect("build and sync the hub");
    let hub = hub_dir(&paths);
    let ready = BdCli::new().ready(&hub).expect("bd ready on hub");
    assert!(ready.iter().any(|i| i.id.starts_with("ra-")), "ra hydrated");

    let mut out = Vec::new();
    run_repos_remove(&BdCli::new(), &paths, &ra, &mut out).expect("remove ra");

    let ra = std::fs::canonicalize(&ra).unwrap();
    let rb = std::fs::canonicalize(&rb).unwrap();
    assert_eq!(read_hub_roster(&hub).expect("hub roster"), vec![rb]);
    let ready = BdCli::new().ready(&hub).expect("bd ready on hub");
    assert!(
        !ready.iter().any(|i| i.id.starts_with("ra-")),
        "ra's hydrated issues are gone without a reset: {:?}",
        ready.iter().map(|i| &i.id).collect::<Vec<_>>()
    );
    assert!(
        ready.iter().any(|i| i.id.starts_with("rb-")),
        "rb untouched"
    );
    let out = String::from_utf8(out).unwrap();
    assert!(
        out.contains(&format!("dropped {} from the hub", ra.display())),
        "{out}"
    );

    // A follow-up snapshot (sync) must not resurrect the removed repo.
    let roster = load_roster(&paths).expect("roster");
    let mut stdout = Vec::new();
    run_snapshot(
        &roster,
        &BdCli::new(),
        &paths,
        false,
        &mut stdout,
        &mut Vec::new(),
    )
    .expect("snapshot after remove");
    let stdout = String::from_utf8(stdout).unwrap();
    assert!(!stdout.contains("ra-"), "{stdout}");
}

#[test]
fn search_end_to_end() {
    // The schema-drift tripwire for `bd search --json` (Slice 11): drive the exact
    // search-worker path — `bd search` on the hub, then the shared attribution —
    // and assert cross-repo results come back attributed like ready rows.
    if !bd_available() {
        eprintln!("SKIP: bd not installed");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let ra = tmp.path().join("ra");
    let rb = tmp.path().join("rb");
    std::fs::create_dir_all(&ra).expect("mkdir ra");
    std::fs::create_dir_all(&rb).expect("mkdir rb");
    build_ready_fixture_repo_with_prefix(&ra, "ra");
    build_ready_fixture_repo_with_prefix(&rb, "rb");
    // bd >= 1.3.0 includes closed issues in `bd search` by default; hank's search
    // is for finding live work, so a closed match must not come back.
    let closed = create_closed_issue(&ra, "Closed task");

    let paths = Paths::with_base(tmp.path());
    let roster = Config {
        watch: false,
        repos: vec![RepoEntry::new(ra.clone()), RepoEntry::new(rb.clone())],
    };

    // Hydrate the hub from both repos.
    ensure_hub(&BdCli::new(), &paths, &roster).expect("ensure_hub");
    let refreshed = refresh::run(&BdCli::new(), &roster, &paths).expect("refresh runs");
    let hub = hub_dir(&paths);

    // The search-worker path: `bd search --json`, then the exact immutable map
    // produced by the refresh that hydrated this hub generation.
    let issues = BdCli::new()
        .search(&hub, "task")
        .expect("bd search --json parses");
    assert!(
        !issues.is_empty(),
        "the fixture titles all contain 'task', so search finds them"
    );
    assert!(
        issues
            .iter()
            .all(|i| i.id != closed && i.status != "closed"),
        "closed issues are excluded from search: {:?}",
        issues
            .iter()
            .map(|i| (&i.id, &i.status))
            .collect::<Vec<_>>()
    );
    let snap = snapshot::attribute(issues, &refreshed.prefix_map, SystemTime::now());

    assert!(
        snap.rows.iter().any(|r| r.repo_name == "ra"),
        "a result is attributed to the ra repo: {:?}",
        snap.rows.iter().map(|r| &r.repo_name).collect::<Vec<_>>()
    );
    assert!(
        snap.rows.iter().any(|r| r.repo_name == "rb"),
        "a result is attributed to the rb repo (cross-repo search)"
    );
    assert!(
        snap.rows
            .iter()
            .all(|r| r.repo_name != snapshot::UNKNOWN_REPO),
        "every hydrated result attributes to a known repo, not the unknown bucket"
    );
    assert!(
        snap.rows.iter().any(|r| r.issue.title.contains("task")),
        "a result carries the searched-for title text"
    );
}

/// Wait for the first message matching `want`, failing after `timeout`.
fn expect_msg(rx: &mpsc::Receiver<Msg>, timeout: Duration, want: impl Fn(&Msg) -> bool) -> Msg {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        match rx.recv_timeout(left) {
            Ok(msg) if want(&msg) => return msg,
            Ok(_) => continue,
            Err(error) => panic!("no matching watcher message: {error}"),
        }
    }
}

fn start_watcher(
    repos: impl Into<WatchList>,
    file: std::path::PathBuf,
) -> (Watcher, mpsc::Receiver<Msg>) {
    let (tx, rx) = mpsc::channel();
    // The roster each test hands in is the whole truth; there is no file.
    let watcher = Watcher::start(
        Arc::new(BdJournal::new()),
        repos,
        file,
        |_| true,
        move |msg| {
            let _ = tx.send(msg);
        },
    );
    (watcher, rx)
}

#[test]
fn watcher_follows_the_events_journal_end_to_end() {
    if !bd_available() || !bd_has_events() {
        eprintln!("SKIP: bd with `events` (>= 1.3.0) not installed");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let live = tmp.path().join("live");
    let off = tmp.path().join("off");
    std::fs::create_dir_all(&live).unwrap();
    std::fs::create_dir_all(&off).unwrap();
    build_ready_fixture_repo_with_prefix(&live, "wl");
    build_ready_fixture_repo_with_prefix(&off, "wo");
    bd_in(&live, &["config", "set", "events-journal", "true"]);
    let checkpoints = tmp.path().join("events_checkpoints.json");

    // `off` is opted out (`hank repos unwatch`): its journal stays off.
    let list = WatchList {
        repos: vec![live.clone(), off.clone()],
        opted_out: [off.clone()].into(),
    };
    let (watcher, rx) = start_watcher(list, checkpoints.clone());

    bd_in(&live, &["create", "Arrives live", "-p", "1"]);
    let changed = expect_msg(&rx, Duration::from_secs(20), |m| {
        matches!(m, Msg::WatchChanged(_))
    });
    assert_eq!(
        changed,
        Msg::WatchChanged(RefreshScope::Repos([live.clone()].into())),
        "only the repo that changed is refreshed"
    );

    // Hank's own refresh (export + hub sync) must not feed the journal back.
    let paths = Paths::with_base(tmp.path());
    let roster = Config {
        repos: vec![RepoEntry::new(live.clone())],
        watch: true,
    };
    ensure_hub(&BdCli::new(), &paths, &roster).expect("hub");
    refresh::run(&BdCli::new(), &roster, &paths).expect("refresh");
    assert!(
        rx.recv_timeout(Duration::from_secs(3)).is_err(),
        "a refresh is not a change"
    );
    let started = std::time::Instant::now();
    watcher.stop();
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "stopping kills the followers promptly"
    );

    let saved = Checkpoints::load(&checkpoints);
    assert!(saved.repos.contains_key(&live), "{saved:?}");
    assert!(!saved.repos.contains_key(&off));
    assert!(
        !BdCli::new().events_journal_enabled(&off).expect("read off"),
        "an opted-out repo's journal is left off"
    );

    // Resuming from the verified checkpoint replays nothing already seen.
    let (watcher, rx) = start_watcher(vec![live.clone()], checkpoints.clone());
    assert!(
        rx.recv_timeout(Duration::from_secs(4)).is_err(),
        "a resumed watcher does not re-report old records"
    );
    bd_in(&live, &["create", "After resume", "-p", "2"]);
    expect_msg(&rx, Duration::from_secs(20), |m| {
        matches!(m, Msg::WatchChanged(_))
    });
    watcher.stop();
}

#[test]
fn watcher_rebaselines_a_pruned_checkpoint() {
    if !bd_available() || !bd_has_events() {
        eprintln!("SKIP: bd with `events` (>= 1.3.0) not installed");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("pruned");
    std::fs::create_dir_all(&repo).unwrap();
    build_ready_fixture_repo_with_prefix(&repo, "wp");
    for (key, value) in [
        ("events-journal", "true"),
        ("events-journal-retain-days", "0"),
        ("events-journal-retain-rows", "1"),
    ] {
        bd_in(&repo, &["config", "set", key, value]);
    }
    let checkpoints = tmp.path().join("events_checkpoints.json");
    let (watcher, rx) = start_watcher(vec![repo.clone()], checkpoints.clone());
    bd_in(&repo, &["create", "first", "-p", "1"]);
    expect_msg(&rx, Duration::from_secs(20), |m| {
        matches!(m, Msg::WatchChanged(_))
    });
    watcher.stop();

    // Two more writes while Hank is away, then prune past the checkpoint.
    bd_in(&repo, &["create", "second", "-p", "1"]);
    bd_in(&repo, &["create", "third", "-p", "1"]);
    bd_in(&repo, &["events", "prune", "--before", "3"]);

    let (watcher, rx) = start_watcher(vec![repo.clone()], checkpoints.clone());
    let msg = expect_msg(&rx, Duration::from_secs(20), |m| {
        matches!(m, Msg::WatchChanged(_))
    });
    assert_eq!(msg, Msg::WatchChanged(RefreshScope::Full));
    watcher.stop();
    assert!(
        !Checkpoints::load(&checkpoints).repos.contains_key(&repo),
        "the pruned checkpoint is forgotten"
    );
}

#[test]
fn events_journal_setting_reads_back_after_toggle() {
    if !bd_available() || !bd_has_events() {
        eprintln!("SKIP: bd with `events` (>= 1.3.0) not installed");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("rj");
    std::fs::create_dir_all(&repo).unwrap();
    build_ready_fixture_repo_with_prefix(&repo, "rj");
    let bd = BdCli::new();

    assert!(
        !bd.events_journal_enabled(&repo).expect("read default"),
        "a fresh repo has the journal off"
    );
    bd_in(&repo, &["config", "set", "events-journal", "true"]);
    assert!(bd.events_journal_enabled(&repo).expect("read on"));
    bd_in(&repo, &["config", "set", "events-journal", "false"]);
    assert!(!bd.events_journal_enabled(&repo).expect("read off"));
}

#[test]
fn watcher_turns_on_an_off_journal_and_follows_it() {
    if !bd_available() || !bd_has_events() {
        eprintln!("SKIP: bd with `events` (>= 1.3.0) not installed");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("auto");
    std::fs::create_dir_all(&repo).unwrap();
    build_ready_fixture_repo_with_prefix(&repo, "wa");
    let bd = BdCli::new();
    assert!(!bd.events_journal_enabled(&repo).expect("read default"));

    let (watcher, rx) = start_watcher(vec![repo.clone()], tmp.path().join("cp.json"));
    let notice = expect_msg(&rx, Duration::from_secs(20), |m| {
        matches!(m, Msg::WatchWarning(_))
    });
    assert!(
        matches!(&notice, Msg::WatchWarning(w) if w.contains("turned on the events journal")),
        "the write is announced: {notice:?}"
    );
    assert!(bd.events_journal_enabled(&repo).expect("read on"));
    let config = std::fs::read_to_string(repo.join(".beads").join("config.yaml")).unwrap();
    assert!(config.contains("events-journal"), "{config}");

    bd_in(&repo, &["create", "Arrives after enable", "-p", "1"]);
    let changed = expect_msg(&rx, Duration::from_secs(20), |m| {
        matches!(m, Msg::WatchChanged(_))
    });
    assert_eq!(
        changed,
        Msg::WatchChanged(RefreshScope::Repos([repo.clone()].into()))
    );
    watcher.stop();
}

#[test]
fn repos_watch_and_unwatch_round_trip_end_to_end() {
    if !bd_available() || !bd_has_events() {
        eprintln!("SKIP: bd with `events` (>= 1.3.0) not installed");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::with_base(tmp.path());
    let repo = tmp.path().join("toggle");
    std::fs::create_dir_all(&repo).unwrap();
    build_ready_fixture_repo_with_prefix(&repo, "rt");
    let bd = BdCli::new();
    run_repos_add(&bd, &paths, &repo, &mut Vec::new()).expect("add");

    run_repos_watch(&bd, &paths, Some(repo.as_path()), true, &mut Vec::new()).expect("watch");
    assert!(bd.events_journal_enabled(&repo).expect("read on"));

    run_repos_watch(&bd, &paths, Some(repo.as_path()), false, &mut Vec::new()).expect("unwatch");
    assert!(!bd.events_journal_enabled(&repo).expect("read off"));
    assert!(load_roster(&paths).unwrap().repos[0].unwatched);

    let stranger = tmp.path().join("stranger");
    std::fs::create_dir_all(&stranger).unwrap();
    build_ready_fixture_repo_with_prefix(&stranger, "rs");
    assert!(run_repos_watch(&bd, &paths, Some(stranger.as_path()), true, &mut Vec::new()).is_err());
    assert!(!bd.events_journal_enabled(&stranger).expect("read stranger"));
}
