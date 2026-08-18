//! `FakeBdClient`: a programmable [`BdClient`] test double.
//!
//! Exposure decision: this is ordinary `pub` (but `#[doc(hidden)]`) library
//! code rather than `#[cfg(test)]`-gated, so later slices' unit tests for the
//! `hub`, `refresh`, and `snapshot` modules — which take a `&impl BdClient` —
//! can drive it. It is a test double, not part of hank's supported API.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::{BdClient, BdError, BdVersion, Issue, RepoSyncReport};

type CallHook = dyn Fn(&Call) + Send + Sync;

/// One recorded invocation, so tests can assert call ordering/count (e.g.
/// "export A, export B, then sync once").
#[doc(hidden)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Call {
    Version,
    Init(PathBuf, String),
    RepoAdd(PathBuf, PathBuf),
    RepoList(PathBuf),
    Export(PathBuf, PathBuf),
    IssuePrefix(PathBuf),
    RepoSync(PathBuf),
    Ready(PathBuf),
    Show(PathBuf, String),
    ShowIssue(PathBuf, String),
    Search(PathBuf, String),
}

/// A programmable [`BdClient`] test double.
///
/// **Every** method of the trait is programmable with a success value or a
/// [`BdError`], so downstream modules can drive their error paths (hub init
/// failure, roster-list failure, sync failure, per-repo export failure, …)
/// without writing another fake. Each `with_*` response is **reused** across
/// calls (not consumed). Unset slots default to something benign: the
/// value-returning calls yield an empty list / bd 1.1.0 version / an empty show
/// error, and the unit-returning calls (`init`/`repo_add`/`repo_sync`) yield
/// `Ok(())`. `export` is keyed **per repo path** so one repo can fail while the
/// rest succeed. Every call is recorded and retrievable via
/// [`FakeBdClient::calls`].
#[doc(hidden)]
#[derive(Default)]
pub struct FakeBdClient {
    calls: Mutex<Vec<Call>>,
    call_hook: Option<Arc<CallHook>>,
    version: Option<Result<BdVersion, BdError>>,
    init: Option<Result<(), BdError>>,
    repo_add: Option<Result<(), BdError>>,
    repo_list: Option<Result<serde_json::Value, BdError>>,
    repo_sync: Option<Result<RepoSyncReport, BdError>>,
    ready: Option<Result<Vec<Issue>, BdError>>,
    show: Option<Result<String, BdError>>,
    show_issue: Option<Result<Issue, BdError>>,
    search: Option<Result<Vec<Issue>, BdError>>,
    export_errs: HashMap<PathBuf, BdError>,
    export_contents: HashMap<PathBuf, Vec<u8>>,
    issue_prefixes: HashMap<PathBuf, String>,
}

impl std::fmt::Debug for FakeBdClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FakeBdClient")
            .field("calls", &self.calls())
            .finish_non_exhaustive()
    }
}

impl FakeBdClient {
    pub fn new() -> Self {
        FakeBdClient::default()
    }

    pub fn with_version(mut self, v: BdVersion) -> Self {
        self.version = Some(Ok(v));
        self
    }

    pub fn with_version_err(mut self, err: BdError) -> Self {
        self.version = Some(Err(err));
        self
    }

    pub fn with_init_err(mut self, err: BdError) -> Self {
        self.init = Some(Err(err));
        self
    }

    pub fn with_repo_add_err(mut self, err: BdError) -> Self {
        self.repo_add = Some(Err(err));
        self
    }

    pub fn with_repo_list(mut self, value: serde_json::Value) -> Self {
        self.repo_list = Some(Ok(value));
        self
    }

    pub fn with_repo_list_err(mut self, err: BdError) -> Self {
        self.repo_list = Some(Err(err));
        self
    }

    pub fn with_repo_sync_err(mut self, err: BdError) -> Self {
        self.repo_sync = Some(Err(err));
        self
    }

    pub fn with_ready(mut self, issues: Vec<Issue>) -> Self {
        self.ready = Some(Ok(issues));
        self
    }

    pub fn with_ready_err(mut self, err: BdError) -> Self {
        self.ready = Some(Err(err));
        self
    }

    pub fn with_show(mut self, output: impl Into<String>) -> Self {
        self.show = Some(Ok(output.into()));
        self
    }

    pub fn with_show_err(mut self, err: BdError) -> Self {
        self.show = Some(Err(err));
        self
    }

    pub fn with_show_issue(mut self, issue: Issue) -> Self {
        self.show_issue = Some(Ok(issue));
        self
    }

    pub fn with_show_issue_err(mut self, err: BdError) -> Self {
        self.show_issue = Some(Err(err));
        self
    }

    pub fn with_search(mut self, issues: Vec<Issue>) -> Self {
        self.search = Some(Ok(issues));
        self
    }

    pub fn with_search_err(mut self, err: BdError) -> Self {
        self.search = Some(Err(err));
        self
    }

    /// Program `export(repo)` to fail for exactly this path; other paths still
    /// export `Ok`. Lets a refresh test fail one repo while the rest proceed.
    pub fn with_export_err(mut self, repo: impl Into<PathBuf>, err: BdError) -> Self {
        self.export_errs.insert(repo.into(), err);
        self
    }

    /// Program the exact bytes a successful export writes to its explicit
    /// target. This keeps stable-publication tests deterministic.
    pub fn with_export_content(
        mut self,
        repo: impl Into<PathBuf>,
        bytes: impl Into<Vec<u8>>,
    ) -> Self {
        self.export_contents.insert(repo.into(), bytes.into());
        self
    }

    /// Program `issue_prefix(repo)` to return this exact (possibly hyphenated)
    /// prefix, overriding the default (which reads the repo's seeded
    /// `metadata.json` `dolt_database`). Lets attribution tests declare a real
    /// prefix that differs from the underscore-sanitized DB name.
    pub fn with_issue_prefix(
        mut self,
        repo: impl Into<PathBuf>,
        prefix: impl Into<String>,
    ) -> Self {
        self.issue_prefixes.insert(repo.into(), prefix.into());
        self
    }

    /// Run a thread-safe observer after each call is recorded. Tests use this
    /// for barriers, bounds, and panic injection without wall-clock sleeps.
    pub fn with_call_hook(mut self, hook: impl Fn(&Call) + Send + Sync + 'static) -> Self {
        self.call_hook = Some(Arc::new(hook));
        self
    }

    /// The invocations recorded so far, in order.
    pub fn calls(&self) -> Vec<Call> {
        self.calls.lock().expect("calls mutex poisoned").clone()
    }

    fn record(&self, call: Call) {
        self.calls
            .lock()
            .expect("calls mutex poisoned")
            .push(call.clone());
        if let Some(hook) = &self.call_hook {
            hook(&call);
        }
    }
}

/// Return a programmed response clone, or `Ok(default())` when unset.
fn resolve<T: Clone>(
    slot: &Option<Result<T, BdError>>,
    default: impl FnOnce() -> T,
) -> Result<T, BdError> {
    match slot {
        Some(r) => r.clone(),
        None => Ok(default()),
    }
}

impl BdClient for FakeBdClient {
    fn version(&self) -> Result<BdVersion, BdError> {
        self.record(Call::Version);
        resolve(&self.version, || BdVersion {
            version: "1.1.0".into(),
            schema_version: 1,
            build: None,
            commit: None,
            branch: None,
        })
    }

    fn init(&self, dir: &Path, prefix: &str) -> Result<(), BdError> {
        self.record(Call::Init(dir.to_path_buf(), prefix.to_string()));
        resolve(&self.init, || ())
    }

    fn repo_add(&self, hub: &Path, repo_path: &Path) -> Result<(), BdError> {
        self.record(Call::RepoAdd(hub.to_path_buf(), repo_path.to_path_buf()));
        resolve(&self.repo_add, || ())
    }

    fn repo_list(&self, hub: &Path) -> Result<serde_json::Value, BdError> {
        self.record(Call::RepoList(hub.to_path_buf()));
        resolve(&self.repo_list, || serde_json::Value::Array(Vec::new()))
    }

    fn export_to(&self, repo: &Path, output: &Path) -> Result<(), BdError> {
        self.record(Call::Export(repo.to_path_buf(), output.to_path_buf()));
        match self.export_errs.get(repo) {
            Some(err) => Err(err.clone()),
            None => {
                let bytes = self
                    .export_contents
                    .get(repo)
                    .cloned()
                    .or_else(|| fs::read(repo.join(".beads/issues.jsonl")).ok())
                    .unwrap_or_else(|| b"[]\n".to_vec());
                fs::write(output, bytes).map_err(|error| BdError {
                    command: format!("fake export {}", repo.display()),
                    stderr: error.to_string(),
                    kind: super::BdErrorKind::NonZeroExit { code: Some(1) },
                })
            }
        }
    }

    fn issue_prefix(&self, repo: &Path) -> Result<String, BdError> {
        self.record(Call::IssuePrefix(repo.to_path_buf()));
        if let Some(prefix) = self.issue_prefixes.get(repo) {
            return Ok(prefix.clone());
        }
        // Default: mirror a real repo whose prefix has no hyphens (prefix ==
        // dolt_database) by reading the seeded metadata.json. A missing/unreadable
        // metadata surfaces as a BdError, so `run` records it as a Metadata error
        // just as a real `bd config get` failure on a non-project dir would.
        crate::refresh::read_prefix(repo).map_err(|detail| BdError {
            command: format!("bd -C {} config get issue_prefix --json", repo.display()),
            stderr: detail,
            kind: super::BdErrorKind::NonZeroExit { code: Some(1) },
        })
    }

    fn repo_sync(&self, hub: &Path) -> Result<RepoSyncReport, BdError> {
        self.record(Call::RepoSync(hub.to_path_buf()));
        resolve(&self.repo_sync, || RepoSyncReport::Other(String::new()))
    }

    fn ready(&self, hub: &Path) -> Result<Vec<Issue>, BdError> {
        self.record(Call::Ready(hub.to_path_buf()));
        resolve(&self.ready, Vec::new)
    }

    fn show(&self, hub: &Path, id: &str) -> Result<String, BdError> {
        self.record(Call::Show(hub.to_path_buf(), id.to_string()));
        match &self.show {
            Some(r) => r.clone(),
            None => Err(BdError {
                command: format!("bd -C {} show {id}", hub.display()),
                stderr: "FakeBdClient: no show response programmed".into(),
                kind: super::BdErrorKind::NonZeroExit { code: Some(1) },
            }),
        }
    }

    fn show_issue(&self, hub: &Path, id: &str) -> Result<Issue, BdError> {
        self.record(Call::ShowIssue(hub.to_path_buf(), id.to_string()));
        match &self.show_issue {
            Some(r) => r.clone(),
            None => Err(BdError {
                command: format!("bd -C {} show {id} --json", hub.display()),
                stderr: "FakeBdClient: no structured show response programmed".into(),
                kind: super::BdErrorKind::NonZeroExit { code: Some(1) },
            }),
        }
    }

    fn search(&self, hub: &Path, query: &str) -> Result<Vec<Issue>, BdError> {
        self.record(Call::Search(hub.to_path_buf(), query.to_string()));
        resolve(&self.search, Vec::new)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bd::types::Issue;
    use crate::bd::{BdClient, BdErrorKind};
    use std::path::Path;

    fn sample_issue(id: &str) -> Issue {
        Issue {
            id: id.to_string(),
            title: "t".into(),
            status: "open".into(),
            priority: 1,
            description: None,
            issue_type: None,
            owner: None,
            labels: Vec::new(),
            created_at: None,
            created_by: None,
            updated_at: None,
            dependency_count: None,
            dependent_count: None,
            comment_count: None,
        }
    }

    #[test]
    fn returns_programmed_ready() {
        let fake = FakeBdClient::new().with_ready(vec![sample_issue("ra-1")]);
        let got = fake.ready(Path::new("/tmp/hub")).expect("programmed ok");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, "ra-1");
        // Stored response is reused on a second call.
        assert_eq!(fake.ready(Path::new("/tmp/hub")).unwrap().len(), 1);
    }

    #[test]
    fn returns_programmed_error() {
        let fake = FakeBdClient::new().with_ready_err(BdError {
            command: "bd -C /tmp/hub ready --json".into(),
            stderr: "boom".into(),
            kind: BdErrorKind::NonZeroExit { code: Some(2) },
        });
        let err = fake
            .ready(Path::new("/tmp/hub"))
            .expect_err("programmed err");
        assert!(matches!(
            err.kind,
            BdErrorKind::NonZeroExit { code: Some(2) }
        ));
    }

    #[test]
    fn export_fails_only_for_programmed_path() {
        let boom = BdError {
            command: "bd -C /tmp/b export ...".into(),
            stderr: "disk full".into(),
            kind: BdErrorKind::NonZeroExit { code: Some(1) },
        };
        let fake = FakeBdClient::new().with_export_err("/tmp/b", boom);

        // Programmed path fails; a different path still exports Ok.
        let tmp = tempfile::tempdir().unwrap();
        assert!(
            fake.export_to(Path::new("/tmp/a"), &tmp.path().join("a"))
                .is_ok()
        );
        assert!(
            fake.export_to(Path::new("/tmp/b"), &tmp.path().join("b"))
                .is_err()
        );
    }

    #[test]
    fn every_method_is_error_programmable() {
        let err = || BdError {
            command: "bd ...".into(),
            stderr: "nope".into(),
            kind: BdErrorKind::NonZeroExit { code: Some(1) },
        };
        let hub = Path::new("/tmp/hub");

        assert!(
            FakeBdClient::new()
                .with_version_err(err())
                .version()
                .is_err()
        );
        assert!(
            FakeBdClient::new()
                .with_init_err(err())
                .init(hub, "hub")
                .is_err()
        );
        assert!(
            FakeBdClient::new()
                .with_repo_add_err(err())
                .repo_add(hub, Path::new("/tmp/ra"))
                .is_err()
        );
        assert!(
            FakeBdClient::new()
                .with_repo_list_err(err())
                .repo_list(hub)
                .is_err()
        );
        assert!(
            FakeBdClient::new()
                .with_repo_sync_err(err())
                .repo_sync(hub)
                .is_err()
        );
        assert!(
            FakeBdClient::new()
                .with_show_err(err())
                .show(hub, "ra-1")
                .is_err()
        );
        assert!(
            FakeBdClient::new()
                .with_show_issue_err(err())
                .show_issue(hub, "ra-1")
                .is_err()
        );
        assert!(
            FakeBdClient::new()
                .with_search_err(err())
                .search(hub, "q")
                .is_err()
        );
    }

    #[test]
    fn records_calls() {
        let fake = FakeBdClient::new();
        let tmp = tempfile::tempdir().unwrap();
        let _ = fake.export_to(Path::new("/tmp/a"), &tmp.path().join("a"));
        let _ = fake.export_to(Path::new("/tmp/b"), &tmp.path().join("b"));
        let _ = fake.repo_sync(Path::new("/tmp/hub"));

        let calls = fake.calls();
        assert_eq!(calls.len(), 3);
        assert!(matches!(&calls[0], Call::Export(p, _) if p == Path::new("/tmp/a")));
        assert!(matches!(&calls[1], Call::Export(p, _) if p == Path::new("/tmp/b")));
        assert!(matches!(&calls[2], Call::RepoSync(p) if p == Path::new("/tmp/hub")));
    }
}
