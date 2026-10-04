//! Persistent, versioned TUI preferences.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::app::{RepoFilter, UiState};
use crate::snapshot::StatusFilter;

/// The file version. `status` was added to version 2 as an optional key rather
/// than a new version: an older hank ignores it and keeps the repository view.
const VERSION: u64 = 2;

#[derive(Deserialize, Serialize)]
struct UiStateFile {
    version: u64,
    #[serde(default)]
    repository: Option<StoredRepository>,
    /// The ready list's status by name. Read as a string, so a value this
    /// build doesn't know falls back to ready without losing the repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    status: Option<String>,
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

/// Atomically persist the confirmed repository view and status.
pub fn save(path: &Path, state: &UiState) -> Result<()> {
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
    if let Some(parent) = parent {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating UI state directory {}", parent.display()))?;
    }
    let state = UiStateFile {
        version: VERSION,
        repository: match &state.repository {
            RepoFilter::All => None,
            RepoFilter::Only(prefix) => Some(StoredRepository::Prefix(prefix.clone())),
            RepoFilter::Unknown => Some(StoredRepository::Unknown),
        },
        status: match state.status {
            StatusFilter::Ready => None,
            status => Some(status.as_str().to_string()),
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

/// Load the last confirmed repository view and status.
///
/// UI state is a preference, never required launch data: any read, parse, or
/// schema error safely falls back to all repos, and a missing or unknown
/// status to ready.
pub fn load(path: &Path) -> UiState {
    let Ok(bytes) = fs::read(path) else {
        return UiState::default();
    };
    let Ok(header) = serde_json::from_slice::<UiStateVersion>(&bytes) else {
        return UiState::default();
    };
    match header.version {
        1 => {
            let Ok(state) = serde_json::from_slice::<LegacyUiStateFile>(&bytes) else {
                return UiState::default();
            };
            let repository = match state.repository {
                Some(prefix) if prefix != crate::snapshot::UNKNOWN_REPO => RepoFilter::Only(prefix),
                _ => RepoFilter::All,
            };
            UiState {
                repository,
                ..UiState::default()
            }
        }
        VERSION => {
            let Ok(state) = serde_json::from_slice::<UiStateFile>(&bytes) else {
                return UiState::default();
            };
            UiState {
                repository: match state.repository {
                    Some(StoredRepository::Prefix(prefix)) => RepoFilter::Only(prefix),
                    Some(StoredRepository::Unknown) => RepoFilter::Unknown,
                    None => RepoFilter::All,
                },
                status: state
                    .status
                    .and_then(|name| name.parse().ok())
                    .unwrap_or_default(),
            }
        }
        _ => UiState::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn repo(repository: RepoFilter) -> UiState {
        UiState {
            repository,
            ..UiState::default()
        }
    }

    #[test]
    fn missing_corrupt_and_unsupported_state_load_all_repos() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("ui_state.json");

        assert_eq!(load(&path), UiState::default());

        fs::write(&path, "{not json").unwrap();
        assert_eq!(load(&path), UiState::default());

        fs::write(&path, r#"{"version":999,"repository":"repo-a"}"#).unwrap();
        assert_eq!(load(&path), UiState::default());
    }

    #[test]
    fn all_and_one_repository_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nested").join("ui_state.json");

        save(&path, &repo(RepoFilter::All)).unwrap();
        assert_eq!(load(&path), repo(RepoFilter::All));

        save(&path, &repo(RepoFilter::Only("repo-a".into()))).unwrap();
        assert_eq!(load(&path), repo(RepoFilter::Only("repo-a".into())));

        save(&path, &repo(RepoFilter::Unknown)).unwrap();
        assert_eq!(load(&path), repo(RepoFilter::Unknown));
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
        assert_eq!(load(&path), repo(RepoFilter::Only("repo-a".into())));

        fs::write(&path, r#"{"version":1,"repository":"unknown"}"#).unwrap();
        assert_eq!(
            load(&path),
            repo(RepoFilter::All),
            "legacy unknown could mean a real prefix or the unattributed bucket"
        );
    }

    #[test]
    fn status_round_trips_beside_the_repository() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("ui_state.json");

        for status in StatusFilter::ALL {
            let state = UiState {
                repository: RepoFilter::Only("repo-a".into()),
                status,
            };
            save(&path, &state).unwrap();
            assert_eq!(load(&path), state);
        }
    }

    #[test]
    fn missing_or_unknown_status_loads_as_ready_and_keeps_the_repository() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("ui_state.json");

        // A file saved before the status filter existed.
        fs::write(&path, r#"{"version":2,"repository":{"Prefix":"repo-a"}}"#).unwrap();
        assert_eq!(load(&path), repo(RepoFilter::Only("repo-a".into())));

        // A status from a newer hank.
        fs::write(
            &path,
            r#"{"version":2,"repository":{"Prefix":"repo-a"},"status":"deferred"}"#,
        )
        .unwrap();
        assert_eq!(load(&path), repo(RepoFilter::Only("repo-a".into())));
    }
}
