//! Ignored real-`bd` refresh performance matrix.
//!
//! Run with:
//! `cargo test refresh_performance_matrix -- --ignored --nocapture`

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant, SystemTime};

use crate::bd::{BdCli, BdClient};
use crate::config::{Config, Paths, RepoEntry};
use crate::hub::{ensure_hub, hub_dir};
use crate::refresh;
use crate::runtime::{PipelineMetrics, RuntimeRefreshState, gather_snapshot_measured};
use crate::snapshot;

const WARMUPS: usize = 2;
const SAMPLES: usize = 10;

#[test]
#[ignore = "requires real bd and records machine-dependent performance evidence"]
fn refresh_performance_matrix() {
    if !bd_available() {
        eprintln!("SKIP: bd not installed");
        return;
    }

    for size in [1usize, 5, 10, 20] {
        let fixture = Fixture::new(size);
        record_stage("baseline_direct_serial", size, || {
            baseline_iteration(&fixture)
        });
        record_stage("stable_export_serial", size, || {
            optimized_iteration(&fixture, 1, None)
        });
        record_stage("bounded_parallel", size, || {
            optimized_iteration(&fixture, 4, None)
        });

        let state = RuntimeRefreshState::default();
        record_stage("retained_tui_state", size, || {
            gather_snapshot_measured(&BdCli::new(), &fixture.roster, &fixture.paths, &state).1
        });
    }
}

#[test]
fn bd_rename_prefix_invalidates_retained_prefix() {
    if !bd_available() {
        eprintln!("SKIP: bd not installed");
        return;
    }

    let fixture = Fixture::new(1);
    let repo = fixture.roster.repos[0].path.clone();
    let config_path = repo.join(".beads").join("config.yaml");
    let metadata_path = repo.join(".beads").join("metadata.json");
    let config_before = std::fs::read(&config_path).expect("config before rename");
    let metadata_before = std::fs::read(&metadata_path).expect("metadata before rename");
    let first = refresh::run_with_state(&BdCli::new(), &fixture.roster, &fixture.paths, None, 1)
        .expect("initial stateful refresh");
    let previous = first.candidate().clone();
    let _ = first.into_outcome();

    bd(&repo, &["rename-prefix", "new-"]);
    let second = refresh::run_with_state(
        &BdCli::new(),
        &fixture.roster,
        &fixture.paths,
        Some(&previous),
        1,
    )
    .expect("refresh after prefix rename");

    assert_eq!(
        std::fs::read(&config_path).expect("config after rename"),
        config_before,
        "config.yaml cannot be used as the prefix invalidation witness"
    );
    assert_eq!(
        std::fs::read(&metadata_path).expect("metadata after rename"),
        metadata_before,
        "metadata.json cannot be used as the prefix invalidation witness"
    );
    assert_eq!(
        second
            .outcome()
            .prefix_map
            .repo_for("new-probe")
            .map(|entry| &entry.path),
        Some(&repo)
    );
    assert!(
        second.outcome().prefix_map.repo_for("r0-probe").is_none(),
        "the old retained prefix is not used after exported ids change"
    );
}

fn record_stage(stage: &str, roster_size: usize, mut iteration: impl FnMut() -> PipelineMetrics) {
    for _ in 0..WARMUPS {
        let _ = iteration();
    }
    let samples = (0..SAMPLES).map(|_| iteration()).collect::<Vec<_>>();
    let field = |select: fn(&PipelineMetrics) -> Duration| {
        let values = samples
            .iter()
            .map(|sample| select(sample).as_micros())
            .collect::<Vec<_>>();
        (median(&values), percentile_95(&values))
    };
    let (total_median, total_p95) = field(|sample| sample.total);
    let (version_median, _) = field(|sample| sample.version);
    let (reconcile_median, _) = field(|sample| sample.reconcile);
    let (source_median, _) = field(|sample| sample.source_wall);
    let (sync_median, _) = field(|sample| sample.sync);
    let (ready_median, _) = field(|sample| sample.ready);
    let calls = samples.last().expect("ten samples").calls;
    println!(
        "{}",
        serde_json::json!({
            "case": "unchanged",
            "stage": stage,
            "roster_size": roster_size,
            "samples": SAMPLES,
            "total_median_us": total_median,
            "total_p95_us": total_p95,
            "version_median_us": version_median,
            "reconcile_median_us": reconcile_median,
            "source_median_us": source_median,
            "sync_median_us": sync_median,
            "ready_median_us": ready_median,
            "calls": {
                "version": calls.version,
                "export": calls.export,
                "issue_prefix": calls.issue_prefix,
                "repo_sync": calls.repo_sync,
                "ready": calls.ready,
                "search": calls.search,
            },
            "sync_report": samples
                .last()
                .and_then(|sample| sample.sync_report.as_ref())
                .map(|report| format!("{report:?}")),
        })
    );
}

fn baseline_iteration(fixture: &Fixture) -> PipelineMetrics {
    let bd = BdCli::new();
    let total_started = Instant::now();
    let version_started = Instant::now();
    bd.version().expect("baseline version");
    let version = version_started.elapsed();

    let reconcile_started = Instant::now();
    ensure_hub(&bd, &fixture.paths, &fixture.roster).expect("baseline reconcile");
    let reconcile = reconcile_started.elapsed();

    let source_started = Instant::now();
    let mut pairs = Vec::new();
    for entry in &fixture.roster.repos {
        bd.export_to(&entry.path, &entry.path.join(".beads").join("issues.jsonl"))
            .expect("baseline direct export");
        pairs.push((
            bd.issue_prefix(&entry.path).expect("baseline prefix"),
            entry.clone(),
        ));
    }
    let source_wall = source_started.elapsed();

    let sync_started = Instant::now();
    let sync_report = bd
        .repo_sync(&hub_dir(&fixture.paths))
        .expect("baseline sync");
    let sync = sync_started.elapsed();

    let ready_started = Instant::now();
    snapshot::fetch(
        &bd,
        &hub_dir(&fixture.paths),
        &refresh::PrefixMap::from_pairs(pairs),
        SystemTime::now(),
    )
    .expect("baseline ready");
    let ready = ready_started.elapsed();
    PipelineMetrics {
        total: total_started.elapsed(),
        version,
        reconcile,
        source_wall,
        sync,
        ready,
        calls: crate::runtime::OperationCounts {
            version: 1,
            export: fixture.roster.repos.len(),
            issue_prefix: fixture.roster.repos.len(),
            repo_sync: 1,
            ready: 1,
            search: 0,
        },
        sync_report: Some(sync_report),
    }
}

fn optimized_iteration(
    fixture: &Fixture,
    worker_limit: usize,
    previous: Option<&refresh::AttributionCandidate>,
) -> PipelineMetrics {
    let bd = BdCli::new();
    let total_started = Instant::now();
    let version_started = Instant::now();
    bd.version().expect("optimized version");
    let version = version_started.elapsed();

    let reconcile_started = Instant::now();
    ensure_hub(&bd, &fixture.paths, &fixture.roster).expect("optimized reconcile");
    let reconcile = reconcile_started.elapsed();

    let synced =
        refresh::run_with_state(&bd, &fixture.roster, &fixture.paths, previous, worker_limit)
            .expect("optimized refresh");
    let refresh_metrics = synced.outcome().metrics;
    let sync_report = synced.outcome().sync_report.clone();
    let ready_started = Instant::now();
    snapshot::fetch(
        &bd,
        &hub_dir(&fixture.paths),
        &synced.outcome().prefix_map,
        synced.outcome().synced_at,
    )
    .expect("optimized ready");
    let ready = ready_started.elapsed();
    PipelineMetrics {
        total: total_started.elapsed(),
        version,
        reconcile,
        source_wall: refresh_metrics.source_wall,
        sync: refresh_metrics.sync,
        ready,
        calls: crate::runtime::OperationCounts {
            version: 1,
            export: refresh_metrics.export_calls,
            issue_prefix: refresh_metrics.issue_prefix_calls,
            repo_sync: 1,
            ready: 1,
            search: 0,
        },
        sync_report: Some(sync_report),
    }
}

fn median(values: &[u128]) -> u128 {
    let mut values = values.to_vec();
    values.sort_unstable();
    values[values.len() / 2]
}

fn percentile_95(values: &[u128]) -> u128 {
    let mut values = values.to_vec();
    values.sort_unstable();
    values[((values.len() * 95).div_ceil(100)).saturating_sub(1)]
}

struct Fixture {
    _temp: tempfile::TempDir,
    paths: Paths,
    roster: Config,
}

impl Fixture {
    fn new(size: usize) -> Self {
        let temp = tempfile::tempdir().expect("performance tempdir");
        let mut repos = Vec::with_capacity(size);
        for index in 0..size {
            let repo = temp.path().join(format!("repo-{index:02}"));
            std::fs::create_dir_all(&repo).expect("fixture repo dir");
            bd_init(&repo, &format!("r{index}"));
            bd(&repo, &["create", "Benchmark issue", "-p", "2", "--json"]);
            let output = repo.join(".beads").join("issues.jsonl");
            bd(
                &repo,
                &["export", "-o", output.to_str().expect("utf8 temp path")],
            );
            repos.push(RepoEntry { path: repo });
        }
        let paths = Paths::with_base(temp.path());
        let roster = Config {
            watch: false,
            repos,
        };
        ensure_hub(&BdCli::new(), &paths, &roster).expect("fixture hub");
        Self {
            _temp: temp,
            paths,
            roster,
        }
    }
}

fn bd_available() -> bool {
    Command::new("bd")
        .args(["version", "--json"])
        .output()
        .is_ok_and(|output| output.status.success())
}

fn bd_init(dir: &Path, prefix: &str) {
    let output = Command::new("bd")
        .current_dir(dir)
        .args(["init", "--prefix", prefix])
        .output()
        .expect("spawn bd init");
    assert!(
        output.status.success(),
        "bd init failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn bd(dir: &Path, args: &[&str]) {
    let output = Command::new("bd")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("spawn bd");
    assert!(
        output.status.success(),
        "bd {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
