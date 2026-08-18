//! `BdCli`: the real [`BdClient`] backed by spawning the `bd` binary.

use std::ffi::OsString;
use std::path::Path;
use std::process::Command;

use serde::de::DeserializeOwned;

use super::{BdClient, BdError, BdErrorKind, BdVersion, Issue, IssueDetail, RepoSyncReport};

/// The real [`BdClient`]: every method shells out to the `bd` binary on PATH.
#[derive(Debug, Clone)]
pub struct BdCli {
    /// The program to spawn; `bd` by default. A field (not a const) so a future
    /// caller could point at an absolute path without touching call sites.
    program: String,
}

impl Default for BdCli {
    fn default() -> Self {
        // Delegate to `new()` so `default()` yields a usable `bd` client rather
        // than an empty program string that would fail every spawn.
        Self::new()
    }
}

impl BdCli {
    /// A client that spawns `bd` from PATH.
    pub fn new() -> Self {
        BdCli {
            program: "bd".to_string(),
        }
    }

    /// Render the command line for display/errors: `bd <args...>`. Lossy on
    /// non-UTF-8 argument bytes — this string is for humans, never for spawning.
    fn command_line(&self, args: &[OsString]) -> String {
        let mut s = self.program.clone();
        for a in args {
            s.push(' ');
            s.push_str(&a.to_string_lossy());
        }
        s
    }

    /// Spawn `bd <args>`, require exit success, and deserialize stdout as JSON.
    fn run_json<T: DeserializeOwned>(&self, args: Vec<OsString>) -> Result<T, BdError> {
        let stdout = self.run_capture(&args, None)?;
        serde_json::from_slice::<T>(&stdout).map_err(|_| BdError {
            command: self.command_line(&args),
            stderr: String::from_utf8_lossy(&stdout).into_owned(),
            kind: BdErrorKind::Parse,
        })
    }

    /// Spawn `bd <args>` and require exit success, discarding stdout. For calls
    /// (repo add/export/repo sync) that print status text, not JSON.
    fn run_ok(&self, args: Vec<OsString>) -> Result<(), BdError> {
        self.run_capture(&args, None).map(|_| ())
    }

    /// Spawn `bd <args>`, require exit success, and return human-readable stdout.
    fn run_text(&self, args: Vec<OsString>) -> Result<String, BdError> {
        self.run_capture(&args, None)
            .map(|stdout| String::from_utf8_lossy(&stdout).into_owned())
    }

    /// Spawn `bd <args>` (optionally with `cwd` as the working directory),
    /// mapping spawn failure and non-zero exit into [`BdError`]; returns raw
    /// stdout on success.
    ///
    /// `cwd` exists because `bd init` targets the current directory and rejects
    /// the global `-C` flag (which pre-checks for an existing beads project);
    /// every other call uses `-C` and leaves `cwd` `None`.
    fn run_capture(&self, args: &[OsString], cwd: Option<&Path>) -> Result<Vec<u8>, BdError> {
        let mut cmd = Command::new(&self.program);
        cmd.args(args);
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        let output = cmd.output().map_err(|e| BdError {
            command: self.command_line(args),
            stderr: e.to_string(),
            kind: BdErrorKind::Spawn,
        })?;
        if !output.status.success() {
            return Err(BdError {
                command: self.command_line(args),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                kind: BdErrorKind::NonZeroExit {
                    code: output.status.code(),
                },
            });
        }
        Ok(output.stdout)
    }
}

/// A path as an argv element, preserving the exact bytes (no lossy UTF-8
/// conversion) so non-UTF-8 paths — valid on Unix — reach `bd` intact.
fn arg(p: &Path) -> OsString {
    p.as_os_str().to_os_string()
}

fn argv_version() -> Vec<OsString> {
    vec!["version".into(), "--json".into()]
}

/// `init`'s argv carries no `-C`: bd rejects `-C` here (it pre-checks for an
/// existing project), so [`BdClient::init`] runs `bd` with `dir` as its cwd.
fn argv_init(prefix: &str) -> Vec<OsString> {
    vec!["init".into(), "--prefix".into(), prefix.into()]
}

fn argv_repo_add(hub: &Path, repo_path: &Path) -> Vec<OsString> {
    vec![
        "-C".into(),
        arg(hub),
        "repo".into(),
        "add".into(),
        arg(repo_path),
    ]
}

fn argv_repo_list(hub: &Path) -> Vec<OsString> {
    vec![
        "-C".into(),
        arg(hub),
        "repo".into(),
        "list".into(),
        "--json".into(),
    ]
}

fn argv_export(repo: &Path, output: &Path) -> Vec<OsString> {
    // bd resolves a relative `-o` against hank's *process* working directory, not
    // the `-C` dir (verified against bd 1.1.0: `bd -C <repo> export -o
    // .beads/issues.jsonl` writes under the caller's cwd, not <repo>). So the
    // output path must locate the repo explicitly. Joining onto `repo` writes to
    // `<repo>/.beads/issues.jsonl` correctly whether `repo` is absolute (an
    // absolute `-o`) or relative (the same cwd base as `-C`), keeping each
    // export inside its own source repo instead of clobbering the caller's cwd.
    vec![
        "-C".into(),
        arg(repo),
        "export".into(),
        "-o".into(),
        arg(output),
    ]
}

fn argv_repo_sync(hub: &Path) -> Vec<OsString> {
    vec!["-C".into(), arg(hub), "repo".into(), "sync".into()]
}

fn argv_issue_prefix(repo: &Path) -> Vec<OsString> {
    vec![
        "-C".into(),
        arg(repo),
        "config".into(),
        "get".into(),
        "issue_prefix".into(),
        "--json".into(),
    ]
}

fn argv_ready(hub: &Path) -> Vec<OsString> {
    // `--limit 0` = unlimited; bd's `ready` otherwise caps output at 100, which
    // would silently truncate a large cross-repo hub.
    vec![
        "-C".into(),
        arg(hub),
        "ready".into(),
        "--limit".into(),
        "0".into(),
        "--json".into(),
    ]
}

fn argv_show(hub: &Path, id: &str) -> Vec<OsString> {
    vec!["-C".into(), arg(hub), "show".into(), id.into()]
}

fn argv_show_json(hub: &Path, id: &str) -> Vec<OsString> {
    vec![
        "-C".into(),
        arg(hub),
        "show".into(),
        id.into(),
        "--json".into(),
    ]
}

fn argv_search(hub: &Path, query: &str) -> Vec<OsString> {
    // `--query=<value>` keeps a flag-like query (e.g. `--help`) literal instead
    // of letting bd parse it as an option; `--limit 0` avoids the default 50 cap.
    vec![
        "-C".into(),
        arg(hub),
        "search".into(),
        format!("--query={query}").into(),
        "--limit".into(),
        "0".into(),
        "--json".into(),
    ]
}

/// `bd config get <key> --json` payload; hank reads only `value`.
#[derive(Debug, serde::Deserialize)]
struct ConfigValue {
    value: String,
}

impl BdClient for BdCli {
    fn version(&self) -> Result<BdVersion, BdError> {
        self.run_json(argv_version())
    }

    fn init(&self, dir: &Path, prefix: &str) -> Result<(), BdError> {
        self.run_capture(&argv_init(prefix), Some(dir)).map(|_| ())
    }

    fn repo_add(&self, hub: &Path, repo_path: &Path) -> Result<(), BdError> {
        self.run_ok(argv_repo_add(hub, repo_path))
    }

    fn repo_list(&self, hub: &Path) -> Result<serde_json::Value, BdError> {
        self.run_json(argv_repo_list(hub))
    }

    fn export_to(&self, repo: &Path, output: &Path) -> Result<(), BdError> {
        self.run_ok(argv_export(repo, output))
    }

    fn issue_prefix(&self, repo: &Path) -> Result<String, BdError> {
        let config: ConfigValue = self.run_json(argv_issue_prefix(repo))?;
        Ok(config.value)
    }

    fn repo_sync(&self, hub: &Path) -> Result<RepoSyncReport, BdError> {
        let output = self.run_text(argv_repo_sync(hub))?;
        let lower = output.to_ascii_lowercase();
        if lower.contains("up to date") {
            Ok(RepoSyncReport::UpToDate)
        } else if let Some(count) = lower
            .split_whitespace()
            .find_map(|word| word.parse::<usize>().ok())
        {
            Ok(RepoSyncReport::Imported { count })
        } else {
            Ok(RepoSyncReport::Other(output))
        }
    }

    fn ready(&self, hub: &Path) -> Result<Vec<Issue>, BdError> {
        self.run_json(argv_ready(hub))
    }

    fn show(&self, hub: &Path, id: &str) -> Result<String, BdError> {
        self.run_text(argv_show(hub, id))
    }

    fn show_issue(&self, hub: &Path, id: &str) -> Result<Issue, BdError> {
        let details: Vec<IssueDetail> = self.run_json(argv_show_json(hub, id))?;
        IssueDetail::into_single(details)
            .map(|detail| detail.issue)
            .map_err(|e| BdError {
                command: self.command_line(&argv_show_json(hub, id)),
                stderr: e.to_string(),
                kind: BdErrorKind::Shape,
            })
    }

    fn search(&self, hub: &Path, query: &str) -> Result<Vec<Issue>, BdError> {
        self.run_json(argv_search(hub, query))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// Lift a `&str` argv literal to the `OsString` vectors the builders return.
    fn os(parts: &[&str]) -> Vec<OsString> {
        parts.iter().map(OsString::from).collect()
    }

    #[test]
    fn builds_correct_argv() {
        assert_eq!(argv_version(), os(&["version", "--json"]));

        // `init` carries no `-C`; it runs with the target dir as cwd because bd
        // rejects `-C` before an existing project is created.
        assert_eq!(argv_init("ra"), os(&["init", "--prefix", "ra"]));

        assert_eq!(
            argv_repo_add(Path::new("/tmp/hub"), Path::new("/tmp/ra")),
            os(&["-C", "/tmp/hub", "repo", "add", "/tmp/ra"])
        );

        assert_eq!(
            argv_repo_list(Path::new("/tmp/hub")),
            os(&["-C", "/tmp/hub", "repo", "list", "--json"])
        );

        // The caller controls the output path; its bytes are passed unchanged.
        assert_eq!(
            argv_export(Path::new("/tmp/ra"), Path::new("/tmp/export.tmp")),
            os(&["-C", "/tmp/ra", "export", "-o", "/tmp/export.tmp"])
        );

        assert_eq!(
            argv_repo_sync(Path::new("/tmp/hub")),
            os(&["-C", "/tmp/hub", "repo", "sync"])
        );

        assert_eq!(
            argv_issue_prefix(Path::new("/tmp/ra")),
            os(&["-C", "/tmp/ra", "config", "get", "issue_prefix", "--json"])
        );

        // `--limit 0` (unlimited) defeats bd's default 100-result cap.
        assert_eq!(
            argv_ready(Path::new("/tmp/hub")),
            os(&["-C", "/tmp/hub", "ready", "--limit", "0", "--json"])
        );

        assert_eq!(
            argv_show(Path::new("/tmp/hub"), "ra-2hc"),
            os(&["-C", "/tmp/hub", "show", "ra-2hc"])
        );
        assert_eq!(
            argv_show_json(Path::new("/tmp/hub"), "ra-2hc"),
            os(&["-C", "/tmp/hub", "show", "ra-2hc", "--json"])
        );
        // Flag-like queries stay literal via `--query=`; `--limit 0` = unlimited.
        assert_eq!(
            argv_search(Path::new("/tmp/hub"), "needle"),
            os(&[
                "-C",
                "/tmp/hub",
                "search",
                "--query=needle",
                "--limit",
                "0",
                "--json"
            ])
        );

        // A query that begins with `-` must not become a bd flag.
        assert_eq!(
            argv_search(Path::new("/tmp/hub"), "--help")[3],
            OsString::from("--query=--help")
        );
    }

    #[test]
    fn argv_preserves_paths_with_spaces() {
        // No shell is involved, so a spaced/unicode path is a single argv element.
        let dir = Path::new("/tmp/my repos/rä");
        assert_eq!(
            argv_ready(dir),
            os(&["-C", "/tmp/my repos/rä", "ready", "--limit", "0", "--json"])
        );
    }

    #[cfg(unix)]
    #[test]
    fn show_spawns_bd_once() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let program = tmp.path().join("fake-bd");
        fs::write(
            &program,
            r#"#!/bin/sh
printf 'call\n' >> "${0}.calls"
case " $* " in
  *" --json "*) printf '[{"id":"ra-1","title":"Task","status":"open","priority":2}]' ;;
  *) printf '○ ra-1 [TASK] · Task\n' ;;
esac
"#,
        )
        .unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();

        let bd = BdCli {
            program: program.to_string_lossy().into_owned(),
        };
        bd.show(Path::new("/tmp/hub"), "ra-1")
            .expect("native detail loads");

        let calls = fs::read_to_string(format!("{}.calls", program.display())).unwrap();
        assert_eq!(calls.lines().count(), 1, "one detail open runs one bd show");
    }

    /// Non-UTF-8 paths are valid on Unix; the argv must carry their exact bytes
    /// rather than the `to_string_lossy` replacement character.
    #[cfg(unix)]
    #[test]
    fn argv_preserves_non_utf8_path() {
        use std::os::unix::ffi::OsStrExt;
        // 0x80 is a lone continuation byte: valid in a path, invalid UTF-8.
        let raw = std::ffi::OsStr::from_bytes(b"/tmp/ra\x80");
        let argv = argv_ready(Path::new(raw));
        assert_eq!(argv[1].as_bytes(), b"/tmp/ra\x80");
    }

    #[test]
    fn bderror_display_truncates_stderr() {
        let huge = "x".repeat(5000);
        let err = BdError {
            command: "bd -C /tmp/hub ready --json".to_string(),
            stderr: huge,
            kind: BdErrorKind::NonZeroExit { code: Some(1) },
        };
        let shown = err.to_string();
        assert!(shown.contains("bd -C /tmp/hub ready --json"));
        assert!(
            shown.len() < 5000,
            "stderr must be truncated, got {} chars",
            shown.len()
        );
        assert!(
            shown.contains("(truncated)"),
            "expected truncation marker in: {shown}"
        );
    }
}
