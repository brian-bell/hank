use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// A single beads source repository in the roster.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoEntry {
    pub path: PathBuf,
    /// Set by `hank repos unwatch`: live refresh must never turn this repo's
    /// events journal back on. Omitted from `config.toml` while off.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unwatched: bool,
}

impl RepoEntry {
    /// A roster entry live refresh may manage (not opted out).
    pub fn new(path: PathBuf) -> Self {
        RepoEntry {
            path,
            unwatched: false,
        }
    }
}

/// The roster of beads repositories hank federates. Source of truth is
/// `config.toml`; this is its in-memory form.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub repos: Vec<RepoEntry>,
    /// Opt in to live refresh from each repo's events journal (`hank --watch`
    /// for one session). Omitted from `config.toml` while off.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub watch: bool,
}

impl Config {
    /// Load a roster from a TOML file. Errors if the file is missing or invalid
    /// (never silently returns a default).
    pub fn load(path: &Path) -> Result<Config> {
        let text = fs::read_to_string(path)
            .with_context(|| format!("reading config file {}", path.display()))?;
        let config: Config = toml::from_str(&text)
            .with_context(|| format!("parsing config file {}", path.display()))?;
        Ok(config)
    }

    /// Save the roster to a TOML file, creating parent directories as needed.
    ///
    /// Because this file is the roster's source of truth, the write is atomic:
    /// the serialized config is written to a temporary file in the same
    /// directory and then renamed over the destination, so an interrupted or
    /// failed write can never leave `config.toml` truncated or partial.
    pub fn save(&self, path: &Path) -> Result<()> {
        let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
        if let Some(parent) = parent {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating config directory {}", parent.display()))?;
        }
        let text = toml::to_string_pretty(self).context("serializing config to TOML")?;

        // Same-directory temp file so the final rename is an atomic replace on
        // the same filesystem. The pid keeps concurrent writers from colliding.
        let file_name = path
            .file_name()
            .context("config path has no file name")?
            .to_string_lossy();
        let tmp_name = format!(".{}.tmp.{}", file_name, std::process::id());
        let tmp_path = match parent {
            Some(parent) => parent.join(tmp_name),
            None => PathBuf::from(tmp_name),
        };

        fs::write(&tmp_path, text)
            .with_context(|| format!("writing temp config file {}", tmp_path.display()))?;
        fs::rename(&tmp_path, path).with_context(|| {
            format!(
                "replacing config file {} with {}",
                path.display(),
                tmp_path.display()
            )
        })?;
        Ok(())
    }
}

/// The application's subdirectory / file name under the XDG roots.
const APP_DIR: &str = "hank";
const CONFIG_FILE_NAME: &str = "config.toml";
const CACHE_FILE_NAME: &str = "snapshot_cache.json";
const UI_STATE_FILE_NAME: &str = "ui_state.json";
const EVENTS_CHECKPOINTS_FILE_NAME: &str = "events_checkpoints.json";

/// Resolved filesystem locations hank uses. Constructed either from real XDG
/// roots (`resolve`, only at the process edge) or from an injected base
/// (`with_base`, for env-independent tests). The join logic lives in one place
/// (`from_roots`) so both paths share tested behavior.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    config_file: PathBuf,
    data_dir: PathBuf,
    cache_file: PathBuf,
    ui_state_file: PathBuf,
    events_checkpoints_file: PathBuf,
}

impl Paths {
    /// Path to the roster config file (`<config_root>/hank/config.toml`).
    pub fn config_file(&self) -> &Path {
        &self.config_file
    }

    /// Path to Hank's data directory (`<data_root>/hank`).
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// Path to the cached [`crate::snapshot::Snapshot`] JSON file
    /// (`<data_root>/hank/snapshot_cache.json`), read at launch by
    /// [`crate::cache::load`] and written after every successful refresh by
    /// [`crate::cache::save`].
    pub fn cache_file(&self) -> &Path {
        &self.cache_file
    }

    /// Path to the persisted TUI preferences
    /// (`<data_root>/hank/ui_state.json`).
    pub fn ui_state_file(&self) -> &Path {
        &self.ui_state_file
    }

    /// Path to the live-refresh journal checkpoints
    /// (`<data_root>/hank/events_checkpoints.json`), see [`crate::watch`].
    pub fn events_checkpoints_file(&self) -> &Path {
        &self.events_checkpoints_file
    }

    /// Resolve a possibly-relative roster entry against the injected config
    /// directory. This keeps direct lower-level callers deterministic even when
    /// they construct a [`Config`] without going through the CLI load boundary.
    pub(crate) fn resolve_roster_path(&self, path: &Path) -> PathBuf {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.config_file
                .parent()
                .expect("the configured roster path always has a parent directory")
                .join(path)
        }
    }

    /// Derive paths from explicit config and data roots. Single source of the
    /// app-dir / file-name join convention.
    fn from_roots(config_root: &Path, data_root: &Path) -> Paths {
        Paths {
            config_file: config_root.join(APP_DIR).join(CONFIG_FILE_NAME),
            data_dir: data_root.join(APP_DIR),
            cache_file: data_root.join(APP_DIR).join(CACHE_FILE_NAME),
            ui_state_file: data_root.join(APP_DIR).join(UI_STATE_FILE_NAME),
            events_checkpoints_file: data_root.join(APP_DIR).join(EVENTS_CHECKPOINTS_FILE_NAME),
        }
    }

    /// Construct paths under a single injected base (tests). Both roots are the
    /// base, so all files land beneath it without touching real XDG dirs.
    pub fn with_base(base: &Path) -> Paths {
        Paths::from_roots(base, base)
    }

    /// Resolve real XDG locations. Only called from `main`; never in tests.
    pub fn resolve() -> Result<Paths> {
        let config_root = dirs::config_dir().context("resolving XDG config dir")?;
        let data_root = dirs::data_local_dir().context("resolving XDG data dir")?;
        Ok(Paths::from_roots(&config_root, &data_root))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn roundtrip_roster() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        let original = Config {
            watch: false,
            repos: vec![
                RepoEntry::new(PathBuf::from("/a")),
                RepoEntry::new(PathBuf::from("/b/c")),
            ],
        };

        original.save(&path).unwrap();
        let loaded = Config::load(&path).unwrap();

        assert_eq!(loaded, original);
    }

    #[test]
    fn load_missing_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.toml");

        assert!(Config::load(&path).is_err());
    }

    #[test]
    fn save_creates_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/does/not/exist/config.toml");

        let original = Config {
            watch: false,
            repos: vec![RepoEntry::new(PathBuf::from("/x"))],
        };

        original.save(&path).unwrap();
        assert!(path.exists());
        assert_eq!(Config::load(&path).unwrap(), original);
    }

    #[test]
    fn paths_uses_injected_base() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();

        let paths = Paths::with_base(base);

        assert_eq!(paths.config_file(), base.join("hank").join("config.toml"));
        assert_eq!(paths.data_dir(), base.join("hank"));
        assert_eq!(
            paths.ui_state_file(),
            base.join("hank").join("ui_state.json")
        );
    }

    #[test]
    fn save_overwrites_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        Config {
            watch: false,
            repos: vec![RepoEntry::new(PathBuf::from("/first"))],
        }
        .save(&path)
        .unwrap();

        let second = Config {
            watch: false,
            repos: vec![RepoEntry::new(PathBuf::from("/second"))],
        };
        second.save(&path).unwrap();

        assert_eq!(Config::load(&path).unwrap(), second);
    }

    #[test]
    fn watch_opt_in_roundtrips_and_stays_out_of_the_file_when_off() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        let on = Config {
            repos: Vec::new(),
            watch: true,
        };
        on.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), on);

        Config::default().save(&path).unwrap();
        assert!(!fs::read_to_string(&path).unwrap().contains("watch"));
        fs::write(&path, "repos = []\n").unwrap();
        assert!(!Config::load(&path).unwrap().watch, "absent means off");
    }

    #[test]
    fn unwatched_opt_out_roundtrips_and_stays_out_of_the_file_when_off() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        let mut opted_out = RepoEntry::new(PathBuf::from("/quiet"));
        opted_out.unwatched = true;
        let roster = Config {
            repos: vec![RepoEntry::new(PathBuf::from("/live")), opted_out],
            watch: true,
        };
        roster.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), roster);
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(text.matches("unwatched").count(), 1, "{text}");

        fs::write(&path, "[[repos]]\npath = \"/old\"\n").unwrap();
        assert_eq!(
            Config::load(&path).unwrap().repos,
            [RepoEntry::new(PathBuf::from("/old"))],
            "a config written before the opt-out existed loads unchanged"
        );
    }

    #[test]
    fn empty_roster_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        let original = Config::default();
        original.save(&path).unwrap();

        assert_eq!(Config::load(&path).unwrap(), original);
    }
}
