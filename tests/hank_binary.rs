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
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "hank 0.1.0-rc.2\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn help_uses_hank_commands() {
    let output = hank().arg("--help").output().unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Usage: hank [COMMAND]"), "{stdout}");
    assert!(
        stdout.contains("First run: `hank repos discover ~/dev --add` then `hank`."),
        "{stdout}"
    );
    assert!(!stdout.contains("`fbd"), "{stdout}");
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
    assert_eq!(package["name"], "hank");
    assert_eq!(package["version"], "0.1.0-rc.2");
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
fn reset_ignores_legacy_state_and_only_clears_canonical_derived_state() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let xdg_config = temp.path().join("config");
    let xdg_data = temp.path().join("data");
    std::fs::create_dir_all(&home).unwrap();

    #[cfg(target_os = "macos")]
    let (config_root, data_root) = {
        let root = home.join("Library/Application Support");
        (root.clone(), root)
    };
    #[cfg(not(target_os = "macos"))]
    let (config_root, data_root) = (xdg_config.clone(), xdg_data.clone());

    let legacy_config = config_root.join("federated-beads/config.toml");
    std::fs::create_dir_all(legacy_config.parent().unwrap()).unwrap();
    std::fs::write(&legacy_config, "repos = [invalid TOML").unwrap();
    let legacy_ui = data_root.join("federated-beads/ui_state.json");
    std::fs::create_dir_all(legacy_ui.parent().unwrap()).unwrap();
    std::fs::write(&legacy_ui, r#"{"version":2}"#).unwrap();
    let canonical_hub = data_root.join("hank/hub");
    let canonical_cache = data_root.join("hank/snapshot_cache.json");
    std::fs::create_dir_all(&canonical_hub).unwrap();
    std::fs::write(&canonical_cache, "cache").unwrap();

    let output = hank()
        .arg("reset")
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &xdg_config)
        .env("XDG_DATA_HOME", &xdg_data)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!canonical_hub.exists());
    assert!(!canonical_cache.exists());
    assert!(legacy_config.exists());
    assert!(legacy_ui.exists());
    assert!(!config_root.join("hank/config.toml").exists());
    assert!(!data_root.join("hank/ui_state.json").exists());
}
