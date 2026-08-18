//! Shared helpers for gated integration tests: arranging real `bd` repo state
//! in temp dirs, independent of hank's own `BdCli` so a client bug can't mask a
//! helper bug.

use std::path::Path;
use std::process::Command;

/// True if a `bd` binary is present and reports a version.
pub fn bd_available() -> bool {
    Command::new("bd")
        .args(["version", "--json"])
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// Run `bd` with the given args targeting `dir` via `-C` (or globally if `dir`
/// is `None`), panicking on failure. Returns captured stdout.
fn bd(dir: Option<&Path>, args: &[&str]) -> String {
    let mut cmd = Command::new("bd");
    if let Some(d) = dir {
        cmd.arg("-C").arg(d);
    }
    cmd.args(args);
    run(cmd, args)
}

/// Run `bd init` (and only init) in `dir` as the working directory. bd rejects
/// `-C` for `init` because it pre-checks for an existing project, so the target
/// directory must be the process cwd.
fn bd_init(dir: &Path, prefix: &str) {
    let mut cmd = Command::new("bd");
    cmd.current_dir(dir).args(["init", "--prefix", prefix]);
    run(cmd, &["init", "--prefix", prefix]);
}

fn run(mut cmd: Command, args: &[&str]) -> String {
    let out = cmd.output().expect("spawn bd");
    assert!(
        out.status.success(),
        "bd {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Parse the `id` of a just-created issue from `bd create --json` stdout, which
/// is either a bare object or an array-of-one.
fn parse_created_id(stdout: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).expect("create --json parses");
    let obj = match &v {
        serde_json::Value::Array(a) => a.first().expect("array-of-one create result"),
        other => other,
    };
    obj.get("id")
        .and_then(|x| x.as_str())
        .expect("create result has string id")
        .to_string()
}

/// Build a fixture repo at `dir` with prefix `ra`. See
/// [`build_ready_fixture_repo_with_prefix`].
pub fn build_ready_fixture_repo(dir: &Path) {
    build_ready_fixture_repo_with_prefix(dir, "ra");
}

/// Build a fixture repo at `dir` with the given `prefix`: three issues where the
/// third is blocked by the second, then export to `.beads/issues.jsonl`. After
/// this, `bd -C dir ready` returns 2 of 3 issues (the blocked one excluded), and
/// every issue id carries `prefix-`.
pub fn build_ready_fixture_repo_with_prefix(dir: &Path, prefix: &str) {
    bd_init(dir, prefix);

    let ready = parse_created_id(&bd(
        Some(dir),
        &[
            "create",
            "Ready task one",
            "-p",
            "1",
            "-d",
            "has a description",
            "--json",
        ],
    ));
    let blocker = parse_created_id(&bd(
        Some(dir),
        &["create", "Blocker task", "-p", "0", "--json"],
    ));
    let blocked = parse_created_id(&bd(
        Some(dir),
        &[
            "create",
            "Blocked task",
            "-p",
            "2",
            "-d",
            "blocked by the blocker",
            "--json",
        ],
    ));

    // `blocked` depends on `blocker` via a blocks link.
    bd(Some(dir), &["link", &blocked, &blocker, "--type", "blocks"]);

    let out = dir.join(".beads/issues.jsonl");
    bd(
        Some(dir),
        &["export", "-o", out.to_str().expect("utf8 path")],
    );

    // `ready` is the third distinct id; silence unused warning by asserting shape.
    assert_ne!(ready, blocker);
    assert_ne!(ready, blocked);
}
