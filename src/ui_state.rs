//! Persistent, versioned TUI preferences.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::app::RepoFilter;

const VERSION: u64 = 2;

#[derive(Deserialize, Serialize)]
struct UiStateFile {
    version: u64,
    #[serde(default)]
    repository: Option<StoredRepository>,
}

#[derive(Deserialize)]
struct UiStateVersion {
    version: u64,
}

#[derive(Deserialize)]
struct LegacyUiStateFile {
    #[serde(default)]
    repository: Option<String>,
}

#[derive(Deserialize, Serialize)]
enum StoredRepository {
    Prefix(String),
    Unknown,
}

/// Atomically persist a confirmed repository view.
pub fn save(path: &Path, repo: &RepoFilter) -> Result<()> {
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
    if let Some(parent) = parent {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating UI state directory {}", parent.display()))?;
    }
    let state = UiStateFile {
        version: VERSION,
        repository: match repo {
            RepoFilter::All => None,
            RepoFilter::Only(prefix) => Some(StoredRepository::Prefix(prefix.clone())),
            RepoFilter::Unknown => Some(StoredRepository::Unknown),
        },
    };
    let bytes = serde_json::to_vec_pretty(&state).context("serializing UI state")?;
    let file_name = path
        .file_name()
        .context("UI state path has no file name")?
        .to_string_lossy();
    let temp_name = format!(".{file_name}.tmp.{}", std::process::id());
    let temp_path = match parent {
        Some(parent) => parent.join(temp_name),
        None => PathBuf::from(temp_name),
    };
    fs::write(&temp_path, bytes)
        .with_context(|| format!("writing temporary UI state {}", temp_path.display()))?;
    fs::rename(&temp_path, path)
        .with_context(|| format!("replacing UI state {}", path.display()))?;
    Ok(())
}

/// Load the last confirmed repository view.
///
/// UI state is a preference, never required launch data: any read, parse, or
/// schema error safely falls back to `All`.
pub fn load(path: &Path) -> RepoFilter {
    let Ok(bytes) = fs::read(path) else {
        return RepoFilter::All;
    };
    let Ok(header) = serde_json::from_slice::<UiStateVersion>(&bytes) else {
        return RepoFilter::All;
    };
    match header.version {
        1 => {
            let Ok(state) = serde_json::from_slice::<LegacyUiStateFile>(&bytes) else {
                return RepoFilter::All;
            };
            match state.repository {
                Some(prefix) if prefix != crate::snapshot::UNKNOWN_REPO => RepoFilter::Only(prefix),
                _ => RepoFilter::All,
            }
        }
        VERSION => {
            let Ok(state) = serde_json::from_slice::<UiStateFile>(&bytes) else {
                return RepoFilter::All;
            };
            match state.repository {
                Some(StoredRepository::Prefix(prefix)) => RepoFilter::Only(prefix),
                Some(StoredRepository::Unknown) => RepoFilter::Unknown,
                None => RepoFilter::All,
            }
        }
        _ => RepoFilter::All,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn missing_corrupt_and_unsupported_state_load_all_repos() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("ui_state.json");

        assert_eq!(load(&path), RepoFilter::All);

        fs::write(&path, "{not json").unwrap();
        assert_eq!(load(&path), RepoFilter::All);

        fs::write(&path, r#"{"version":999,"repository":"repo-a"}"#).unwrap();
        assert_eq!(load(&path), RepoFilter::All);
    }

    #[test]
    fn all_and_one_repository_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nested").join("ui_state.json");

        save(&path, &RepoFilter::All).unwrap();
        assert_eq!(load(&path), RepoFilter::All);

        save(&path, &RepoFilter::Only("repo-a".into())).unwrap();
        assert_eq!(load(&path), RepoFilter::Only("repo-a".into()));

        save(&path, &RepoFilter::Unknown).unwrap();
        assert_eq!(load(&path), RepoFilter::Unknown);
        assert_eq!(
            fs::read_dir(path.parent().unwrap())
                .unwrap()
                .filter_map(Result::ok)
                .count(),
            1,
            "atomic replacement leaves no temporary file"
        );
    }

    #[test]
    fn migrates_unambiguous_v1_repository_preferences() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("ui_state.json");

        fs::write(&path, r#"{"version":1,"repository":"repo-a"}"#).unwrap();
        assert_eq!(load(&path), RepoFilter::Only("repo-a".into()));

        fs::write(&path, r#"{"version":1,"repository":"unknown"}"#).unwrap();
        assert_eq!(
            load(&path),
            RepoFilter::All,
            "legacy unknown could mean a real prefix or the unattributed bucket"
        );
    }
}
