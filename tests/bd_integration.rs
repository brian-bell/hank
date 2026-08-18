//! Gated end-to-end tests against a real `bd` binary.
//!
//! Every test here first checks whether `bd` is installed; if not, it prints
//! `SKIP: bd not installed` and returns early so `cargo test --test
//! bd_integration` is always green regardless of environment. These tests are
//! the schema-drift tripwire: they drive the real `BdCli` against real repos.

mod helpers;

use std::time::SystemTime;

use hank::bd::{BdCli, BdClient, RepoSyncReport};
use hank::cli::run_snapshot;
use hank::config::{Config, Paths, RepoEntry};
use hank::hub::{ensure_hub, hub_dir, read_hub_roster};
use hank::{refresh, snapshot};
use helpers::{bd_available, build_ready_fixture_repo, build_ready_fixture_repo_with_prefix};

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
        repos: vec![
            RepoEntry { path: ra.clone() },
            RepoEntry { path: rb.clone() },
        ],
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
        repos: vec![
            RepoEntry { path: ra.clone() },
            RepoEntry { path: rb.clone() },
        ],
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
        repos: vec![RepoEntry { path: repo.clone() }],
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
        repos: vec![RepoEntry { path: repo.clone() }],
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
        repos: vec![
            RepoEntry { path: ra.clone() },
            RepoEntry { path: rb.clone() },
        ],
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

    let paths = Paths::with_base(tmp.path());
    let roster = Config {
        repos: vec![
            RepoEntry { path: ra.clone() },
            RepoEntry { path: rb.clone() },
        ],
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
