use anyhow::{Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// A single beads source repository in the roster.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoEntry {
    pub path: PathBuf,
}

/// The roster of beads repositories hank federates. Source of truth is
/// `config.toml`; this is its in-memory form.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub repos: Vec<RepoEntry>,
}

#[derive(Deserialize)]
struct SpannedConfig {
    #[serde(default)]
    repos: Vec<SpannedRepoEntry>,
}

#[derive(Deserialize)]
struct SpannedRepoEntry {
    path: toml::Spanned<String>,
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
const LEGACY_APP_DIR: &str = "federated-beads";
const CONFIG_FILE_NAME: &str = "config.toml";
const CACHE_FILE_NAME: &str = "snapshot_cache.json";
const UI_STATE_FILE_NAME: &str = "ui_state.json";
const MIGRATION_LOCK_FILE_NAME: &str = ".hank-migration.lock";

static MIGRATION_TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

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
    legacy_config_file: PathBuf,
    legacy_ui_state_file: PathBuf,
    migration_lock_file: PathBuf,
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
            legacy_config_file: config_root.join(LEGACY_APP_DIR).join(CONFIG_FILE_NAME),
            legacy_ui_state_file: data_root.join(LEGACY_APP_DIR).join(UI_STATE_FILE_NAME),
            migration_lock_file: data_root.join(APP_DIR).join(MIGRATION_LOCK_FILE_NAME),
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

/// A user-state migration problem. Config and infrastructure problems are
/// launch-blocking; invalid legacy UI state is only a warning because the TUI
/// can safely use its documented All-repositories default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationProblem {
    artifact: MigrationArtifact,
    message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MigrationArtifact {
    Config,
    UiState,
    Infrastructure,
}

/// Observable result of checking and, when safe, migrating legacy user state.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MigrationReport {
    migrated: Vec<PathBuf>,
    problems: Vec<MigrationProblem>,
}

impl MigrationReport {
    /// First problem that must stop normal Hank commands.
    pub fn fatal_problem(&self) -> Option<&str> {
        self.problems
            .iter()
            .find(|problem| problem.artifact != MigrationArtifact::UiState)
            .map(|problem| problem.message.as_str())
    }

    /// Non-fatal migration diagnostics suitable for stderr or `hank doctor`.
    pub fn warnings(&self) -> impl Iterator<Item = &str> {
        self.problems
            .iter()
            .filter(|problem| problem.artifact == MigrationArtifact::UiState)
            .map(|problem| problem.message.as_str())
    }

    pub fn migrated(&self) -> &[PathBuf] {
        &self.migrated
    }

    pub fn problems(&self) -> impl Iterator<Item = &str> {
        self.problems.iter().map(|problem| problem.message.as_str())
    }
}

/// Validate and migrate the two user-owned legacy artifacts. A blocking fs2
/// lock serializes concurrent Hank launches; each destination is still
/// published with a no-clobber hard link so a canonical file created by a
/// non-cooperating process always wins.
pub fn migrate_legacy_state(paths: &Paths) -> MigrationReport {
    let mut report = MigrationReport::default();
    if !legacy_artifact_may_need_migration(&paths.legacy_config_file, &paths.config_file)
        && !legacy_artifact_may_need_migration(&paths.legacy_ui_state_file, &paths.ui_state_file)
    {
        return report;
    }
    let lock_parent = paths
        .migration_lock_file
        .parent()
        .expect("migration lock always has a parent");
    if let Err(error) = fs::create_dir_all(lock_parent) {
        report.problems.push(MigrationProblem {
            artifact: MigrationArtifact::Infrastructure,
            message: format!(
                "legacy migration cannot create data root {}: {error}",
                lock_parent.display()
            ),
        });
        return report;
    }
    let lock = match OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&paths.migration_lock_file)
    {
        Ok(lock) => lock,
        Err(error) => {
            report.problems.push(MigrationProblem {
                artifact: MigrationArtifact::Infrastructure,
                message: format!(
                    "legacy migration cannot open lock {}: {error}",
                    paths.migration_lock_file.display()
                ),
            });
            return report;
        }
    };
    if let Err(error) = lock.lock_exclusive() {
        report.problems.push(MigrationProblem {
            artifact: MigrationArtifact::Infrastructure,
            message: format!(
                "legacy migration cannot lock {}: {error}",
                paths.migration_lock_file.display()
            ),
        });
        return report;
    }

    migrate_file(
        &paths.legacy_config_file,
        &paths.config_file,
        MigrationArtifact::Config,
        |bytes| migrated_config_bytes(&paths.legacy_config_file, bytes),
        &mut report,
    );
    migrate_file(
        &paths.legacy_ui_state_file,
        &paths.ui_state_file,
        MigrationArtifact::UiState,
        |bytes| {
            crate::ui_state::validate_bytes(bytes)?;
            Ok(bytes.to_vec())
        },
        &mut report,
    );
    report
}

/// Cheap no-write preflight used to avoid requiring a writable data root when
/// there is no legacy user state to migrate. Any inspection error is treated as
/// a candidate so the locked migration path can surface an actionable problem.
fn legacy_artifact_may_need_migration(legacy: &Path, canonical: &Path) -> bool {
    match fs::symlink_metadata(canonical) {
        Ok(_) => false,
        Err(error) if error.kind() == io::ErrorKind::NotFound => !matches!(
            fs::symlink_metadata(legacy),
            Err(error) if error.kind() == io::ErrorKind::NotFound
        ),
        Err(_) => true,
    }
}

fn migrate_file(
    legacy: &Path,
    canonical: &Path,
    artifact: MigrationArtifact,
    validate_and_transform: impl FnOnce(&[u8]) -> Result<Vec<u8>>,
    report: &mut MigrationReport,
) {
    match fs::symlink_metadata(canonical) {
        Ok(_) => return,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            report.problems.push(MigrationProblem {
                artifact,
                message: format!(
                    "legacy migration cannot inspect canonical state {}: {error}",
                    canonical.display()
                ),
            });
            return;
        }
    }
    match fs::symlink_metadata(legacy) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return,
        Err(error) => {
            report.problems.push(MigrationProblem {
                artifact,
                message: format!(
                    "legacy migration cannot inspect legacy state {}: {error}",
                    legacy.display()
                ),
            });
            return;
        }
    }
    let mut source = match File::open(legacy) {
        Ok(source) => source,
        Err(error) => {
            report.problems.push(MigrationProblem {
                artifact,
                message: format!("reading legacy state {}: {error}", legacy.display()),
            });
            return;
        }
    };
    let permissions = match source.metadata() {
        Ok(metadata) => metadata.permissions(),
        Err(error) => {
            report.problems.push(MigrationProblem {
                artifact,
                message: format!("reading legacy metadata {}: {error}", legacy.display()),
            });
            return;
        }
    };
    let mut bytes = Vec::new();
    if let Err(error) = source.read_to_end(&mut bytes) {
        report.problems.push(MigrationProblem {
            artifact,
            message: format!("reading legacy state {}: {error}", legacy.display()),
        });
        return;
    }
    let bytes = match validate_and_transform(&bytes) {
        Ok(bytes) => bytes,
        Err(error) => {
            report.problems.push(MigrationProblem {
                artifact,
                message: format!(
                    "legacy state {} is invalid and was not migrated: {error}",
                    legacy.display()
                ),
            });
            return;
        }
    };
    match publish_no_clobber(canonical, &bytes, permissions) {
        Ok(PublishOutcome::Published) => report.migrated.push(canonical.to_path_buf()),
        Ok(PublishOutcome::CanonicalWon) => {}
        Err(error) => report.problems.push(MigrationProblem {
            artifact,
            message: format!(
                "migrating legacy state {} to {}: {error}",
                legacy.display(),
                canonical.display()
            ),
        }),
    }
}

fn migrated_config_bytes(legacy: &Path, bytes: &[u8]) -> Result<Vec<u8>> {
    let text = std::str::from_utf8(bytes).context("config file is not UTF-8")?;
    let _: Config = toml::from_str(text).context("parsing config TOML")?;
    let spanned: SpannedConfig =
        toml::from_str(text).context("locating repository paths in config TOML")?;
    let legacy_dir = legacy
        .parent()
        .context("legacy config path has no parent directory")?;
    let mut replacements = Vec::new();
    for repo in spanned.repos {
        let original = PathBuf::from(repo.path.get_ref());
        if original.is_relative() && original.strip_prefix("~").is_err() {
            let rebased = legacy_dir.join(original);
            let rebased = rebased
                .to_str()
                .context("migrated repository path is not valid UTF-8")?;
            replacements.push((
                repo.path.span(),
                toml::Value::String(rebased.to_string()).to_string(),
            ));
        }
    }
    if replacements.is_empty() {
        return Ok(bytes.to_vec());
    }
    replacements.sort_by_key(|(span, _)| span.start);
    let mut migrated = text.to_string();
    for (span, replacement) in replacements.into_iter().rev() {
        migrated.replace_range(span, &replacement);
    }
    let _: Config = toml::from_str(&migrated).context("validating migrated config TOML")?;
    Ok(migrated.into_bytes())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PublishOutcome {
    Published,
    CanonicalWon,
}

fn publish_no_clobber(
    destination: &Path,
    bytes: &[u8],
    permissions: fs::Permissions,
) -> io::Result<PublishOutcome> {
    publish_no_clobber_with_counter(destination, bytes, permissions, &MIGRATION_TEMP_COUNTER)
}

fn publish_no_clobber_with_counter(
    destination: &Path,
    bytes: &[u8],
    permissions: fs::Permissions,
    counter: &AtomicU64,
) -> io::Result<PublishOutcome> {
    let parent = destination
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "destination has no parent"))?;
    fs::create_dir_all(parent)?;
    let file_name = destination
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "destination has no name"))?
        .to_string_lossy();
    let (mut file, temp) = reserve_migration_temp(parent, &file_name, counter)?;
    let result = (|| {
        file.write_all(bytes)?;
        file.set_permissions(permissions)?;
        file.sync_all()?;
        match fs::hard_link(&temp, destination) {
            Ok(()) => {
                File::open(parent)?.sync_all()?;
                Ok(PublishOutcome::Published)
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                Ok(PublishOutcome::CanonicalWon)
            }
            Err(error) => Err(error),
        }
    })();
    let cleanup = fs::remove_file(&temp);
    match (result, cleanup) {
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) if error.kind() != io::ErrorKind::NotFound => Err(error),
        (Ok(outcome), _) => Ok(outcome),
    }
}

fn reserve_migration_temp(
    parent: &Path,
    file_name: &str,
    counter: &AtomicU64,
) -> io::Result<(File, PathBuf)> {
    loop {
        let temp = parent.join(format!(
            ".{file_name}.hank-migrate.{}.{}.tmp",
            std::process::id(),
            counter.fetch_add(1, Ordering::Relaxed)
        ));
        match OpenOptions::new().write(true).create_new(true).open(&temp) {
            Ok(file) => return Ok((file, temp)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
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
            repos: vec![
                RepoEntry {
                    path: PathBuf::from("/a"),
                },
                RepoEntry {
                    path: PathBuf::from("/b/c"),
                },
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
            repos: vec![RepoEntry {
                path: PathBuf::from("/x"),
            }],
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
        assert_eq!(
            paths.migration_lock_file,
            base.join("hank").join(".hank-migration.lock")
        );
    }

    #[test]
    fn migrates_valid_legacy_config_without_removing_source() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(dir.path());
        let legacy = dir.path().join("federated-beads/config.toml");
        let expected = Config {
            repos: vec![RepoEntry {
                path: PathBuf::from("/legacy/repo"),
            }],
        };
        expected.save(&legacy).unwrap();

        let report = migrate_legacy_state(&paths);

        assert!(report.fatal_problem().is_none(), "{report:?}");
        assert_eq!(Config::load(paths.config_file()).unwrap(), expected);
        assert_eq!(Config::load(&legacy).unwrap(), expected);
    }

    #[test]
    fn migrated_relative_repo_paths_keep_their_legacy_config_meaning() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(dir.path());
        let legacy_dir = dir.path().join("federated-beads");
        let repo = legacy_dir.join("repo");
        fs::create_dir_all(&repo).unwrap();
        fs::write(
            legacy_dir.join("config.toml"),
            "# preserved comment\nextra = \"preserved\"\nrepos = [{ path = \"repo\", note = \"keep\" }]\n",
        )
        .unwrap();

        let report = migrate_legacy_state(&paths);
        let loaded = crate::cli::load_roster(&paths).unwrap();

        assert!(report.fatal_problem().is_none(), "{report:?}");
        assert_eq!(loaded.repos[0].path, fs::canonicalize(repo).unwrap());
        let migrated = fs::read_to_string(paths.config_file()).unwrap();
        assert!(migrated.contains("# preserved comment"), "{migrated}");
        assert!(migrated.contains("extra = \"preserved\""), "{migrated}");
        assert!(migrated.contains("note = \"keep\""), "{migrated}");
    }

    #[test]
    fn absolute_only_config_is_published_byte_for_byte() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(dir.path());
        let legacy = dir.path().join("federated-beads/config.toml");
        fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        let original = b"# keep formatting and unknown keys\nextra = 'preserved'\nrepos = [ { path = '/absolute/repo', note = 'keep' } ]\n";
        fs::write(&legacy, original).unwrap();

        let report = migrate_legacy_state(&paths);

        assert!(report.fatal_problem().is_none(), "{report:?}");
        assert_eq!(fs::read(paths.config_file()).unwrap(), original);
    }

    #[test]
    fn tilde_paths_keep_legacy_expansion_semantics() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(dir.path());
        let legacy = dir.path().join("federated-beads/config.toml");
        Config {
            repos: vec![
                RepoEntry { path: "~".into() },
                RepoEntry {
                    path: "~/repo".into(),
                },
                RepoEntry {
                    path: "~user/repo".into(),
                },
            ],
        }
        .save(&legacy)
        .unwrap();

        let report = migrate_legacy_state(&paths);
        let migrated = Config::load(paths.config_file()).unwrap();

        assert!(report.fatal_problem().is_none(), "{report:?}");
        assert_eq!(migrated.repos[0].path, PathBuf::from("~"));
        assert_eq!(migrated.repos[1].path, PathBuf::from("~/repo"));
        assert_eq!(
            migrated.repos[2].path,
            legacy.parent().unwrap().join("~user/repo")
        );
    }

    #[test]
    fn migrates_valid_legacy_ui_state_without_removing_source() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(dir.path());
        let legacy = dir.path().join("federated-beads/ui_state.json");
        crate::ui_state::save(&legacy, &crate::app::RepoFilter::Only("repo-a".into())).unwrap();

        let report = migrate_legacy_state(&paths);

        assert!(report.fatal_problem().is_none(), "{report:?}");
        assert_eq!(
            crate::ui_state::load(paths.ui_state_file()),
            crate::app::RepoFilter::Only("repo-a".into())
        );
        assert_eq!(
            crate::ui_state::load(&legacy),
            crate::app::RepoFilter::Only("repo-a".into())
        );
    }

    #[test]
    fn fresh_install_does_not_create_canonical_state_files() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(dir.path());

        let report = migrate_legacy_state(&paths);

        assert!(report.fatal_problem().is_none(), "{report:?}");
        assert!(report.migrated().is_empty());
        assert!(!paths.config_file().exists());
        assert!(!paths.ui_state_file().exists());
        assert!(
            !paths.data_dir().exists(),
            "checking an install with no legacy state must not require or create the data root"
        );
    }

    #[test]
    fn canonical_state_wins_even_when_legacy_state_is_invalid() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(dir.path());
        let canonical = Config {
            repos: vec![RepoEntry {
                path: PathBuf::from("/canonical"),
            }],
        };
        canonical.save(paths.config_file()).unwrap();
        crate::ui_state::save(
            paths.ui_state_file(),
            &crate::app::RepoFilter::Only("canonical".into()),
        )
        .unwrap();
        fs::create_dir_all(dir.path().join("federated-beads")).unwrap();
        fs::write(
            dir.path().join("federated-beads/config.toml"),
            "not = [toml",
        )
        .unwrap();
        fs::write(dir.path().join("federated-beads/ui_state.json"), "not json").unwrap();

        let report = migrate_legacy_state(&paths);

        assert!(report.problems().next().is_none(), "{report:?}");
        assert_eq!(Config::load(paths.config_file()).unwrap(), canonical);
        assert_eq!(
            crate::ui_state::load(paths.ui_state_file()),
            crate::app::RepoFilter::Only("canonical".into())
        );
    }

    #[test]
    fn malformed_legacy_config_is_fatal_and_never_published() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(dir.path());
        let legacy = dir.path().join("federated-beads/config.toml");
        fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        fs::write(&legacy, "repos = [this is not TOML").unwrap();

        let report = migrate_legacy_state(&paths);

        let problem = report.fatal_problem().expect("invalid config is fatal");
        assert!(problem.contains(&legacy.display().to_string()), "{problem}");
        assert!(problem.contains("invalid"), "{problem}");
        assert!(!paths.config_file().exists());
        assert_eq!(
            fs::read_to_string(legacy).unwrap(),
            "repos = [this is not TOML"
        );
    }

    #[test]
    fn invalid_legacy_ui_state_warns_and_falls_back_to_all_repositories() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(dir.path());
        let legacy = dir.path().join("federated-beads/ui_state.json");
        fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        fs::write(&legacy, r#"{"version":999,"repository":"repo-a"}"#).unwrap();

        let report = migrate_legacy_state(&paths);

        assert!(report.fatal_problem().is_none(), "{report:?}");
        let warning = report.warnings().next().expect("invalid UI warning");
        assert!(warning.contains(&legacy.display().to_string()), "{warning}");
        assert!(!paths.ui_state_file().exists());
        assert_eq!(
            crate::ui_state::load(paths.ui_state_file()),
            crate::app::RepoFilter::All
        );
    }

    #[test]
    fn unwritable_canonical_target_is_deterministic_and_leaves_no_partial_file() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(dir.path());
        let legacy = dir.path().join("federated-beads/config.toml");
        Config::default().save(&legacy).unwrap();
        fs::write(dir.path().join("hank"), "blocks canonical directory").unwrap();

        let report = migrate_legacy_state(&paths);

        let problem = report.fatal_problem().expect("blocked target is fatal");
        assert!(
            problem.contains(&paths.data_dir().display().to_string()),
            "{problem}"
        );
        assert!(!paths.config_file().exists());
        assert_eq!(Config::load(&legacy).unwrap(), Config::default());
    }

    #[cfg(unix)]
    #[test]
    fn migration_preserves_restrictive_legacy_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(dir.path());
        let legacy = dir.path().join("federated-beads/config.toml");
        Config::default().save(&legacy).unwrap();
        fs::set_permissions(&legacy, fs::Permissions::from_mode(0o600)).unwrap();

        let report = migrate_legacy_state(&paths);

        assert!(report.fatal_problem().is_none(), "{report:?}");
        assert_eq!(
            fs::metadata(paths.config_file())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn migration_temp_collision_is_preserved_and_retried() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("config.toml");
        let counter = AtomicU64::new(0);
        let collision = dir.path().join(format!(
            ".config.toml.hank-migrate.{}.0.tmp",
            std::process::id()
        ));
        fs::write(&collision, "not ours").unwrap();
        let permissions = fs::metadata(&collision).unwrap().permissions();

        let outcome =
            publish_no_clobber_with_counter(&destination, b"repos = []\n", permissions, &counter)
                .unwrap();

        assert_eq!(outcome, PublishOutcome::Published);
        assert_eq!(fs::read_to_string(destination).unwrap(), "repos = []\n");
        assert_eq!(fs::read_to_string(collision).unwrap(), "not ours");
        assert!(
            !dir.path()
                .join(format!(
                    ".config.toml.hank-migrate.{}.1.tmp",
                    std::process::id()
                ))
                .exists()
        );
    }

    #[test]
    fn concurrent_and_repeated_migrations_are_idempotent_and_clean_temps() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(dir.path());
        let legacy = dir.path().join("federated-beads/config.toml");
        let expected = Config {
            repos: vec![RepoEntry {
                path: PathBuf::from("/concurrent"),
            }],
        };
        expected.save(&legacy).unwrap();

        std::thread::scope(|scope| {
            let handles = (0..8)
                .map(|_| {
                    let paths = paths.clone();
                    scope.spawn(move || migrate_legacy_state(&paths))
                })
                .collect::<Vec<_>>();
            for handle in handles {
                let report = handle.join().unwrap();
                assert!(report.fatal_problem().is_none(), "{report:?}");
            }
        });
        let repeated = migrate_legacy_state(&paths);

        assert!(repeated.migrated().is_empty());
        assert_eq!(Config::load(paths.config_file()).unwrap(), expected);
        assert_eq!(Config::load(&legacy).unwrap(), expected);
        assert!(
            fs::read_dir(paths.config_file().parent().unwrap())
                .unwrap()
                .filter_map(Result::ok)
                .all(|entry| !entry.file_name().to_string_lossy().contains("hank-migrate")),
            "migration temporary files must be cleaned"
        );
    }

    #[test]
    fn derived_legacy_state_is_never_copied() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::with_base(dir.path());
        let legacy_data = dir.path().join("federated-beads");
        fs::create_dir_all(legacy_data.join("hub")).unwrap();
        fs::write(legacy_data.join("snapshot_cache.json"), "legacy cache").unwrap();
        fs::write(legacy_data.join("hub/.fbd.lock"), "legacy lock").unwrap();
        fs::write(legacy_data.join(".issues.jsonl.fbd.1.0.tmp"), "legacy temp").unwrap();

        let report = migrate_legacy_state(&paths);

        assert!(report.fatal_problem().is_none(), "{report:?}");
        assert!(!paths.data_dir().join("hub").exists());
        assert!(!paths.cache_file().exists());
        assert!(legacy_data.join("hub/.fbd.lock").exists());
        assert!(legacy_data.join("snapshot_cache.json").exists());
    }

    #[test]
    fn save_overwrites_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        Config {
            repos: vec![RepoEntry {
                path: PathBuf::from("/first"),
            }],
        }
        .save(&path)
        .unwrap();

        let second = Config {
            repos: vec![RepoEntry {
                path: PathBuf::from("/second"),
            }],
        };
        second.save(&path).unwrap();

        assert_eq!(Config::load(&path).unwrap(), second);
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
