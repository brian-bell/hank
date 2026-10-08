use std::process::Command;

fn hank() -> Command {
    let executable = std::env::var_os("CARGO_BIN_EXE_hank")
        .expect("Cargo must expose the sole hank binary to integration tests");
    Command::new(executable)
}

#[test]
fn version_uses_hank_identity() {
    let output = hank().arg("--version").output().unwrap();

    assert!(output.status.success());
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "hank 0.1.0\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn help_uses_hank_commands() {
    let output = hank().arg("--help").output().unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("Usage: hank [OPTIONS] [COMMAND]"),
        "{stdout}"
    );
    assert!(stdout.contains("--watch"), "{stdout}");
    assert!(
        stdout.contains("First run: `hank repos discover ~/dev --add` then `hank`."),
        "{stdout}"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn cargo_metadata_exposes_exactly_one_hank_binary() {
    let output = Command::new(env!("CARGO"))
        .args(["metadata", "--no-deps", "--format-version=1"])
        .output()
        .unwrap();
    assert!(output.status.success());

    let metadata: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let package = &metadata["packages"][0];
    assert_eq!(package["name"], "hank-tui");
    assert_eq!(package["version"], "0.1.0");
    assert_eq!(package["default_run"], "hank");

    let binary_names = package["targets"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|target| {
            target["kind"]
                .as_array()
                .is_some_and(|kinds| kinds.iter().any(|kind| kind == "bin"))
        })
        .map(|target| target["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(binary_names, ["hank"]);
}

#[test]
fn repos_watch_needs_a_path_or_all() {
    for action in ["watch", "unwatch"] {
        let output = hank().args(["repos", action]).output().unwrap();
        assert!(!output.status.success(), "bare `repos {action}` is refused");
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains("<PATH>"), "{stderr}");

        let output = hank()
            .args(["repos", action, "/some/repo", "--all"])
            .output()
            .unwrap();
        assert!(!output.status.success(), "a path and --all conflict");
    }
}

#[test]
fn snapshot_status_lists_and_checks_its_values() {
    let output = hank().args(["snapshot", "--help"]).output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("[possible values: ready, open, in_progress, blocked]"),
        "{stdout}"
    );

    let output = hank()
        .args(["snapshot", "--status", "closed"])
        .output()
        .unwrap();
    assert!(!output.status.success(), "an unknown status is refused");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("'closed'"), "{stderr}");
}
