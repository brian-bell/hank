# Agent Instructions

> **Note:** This repository intentionally maintains **separate `CLAUDE.md` and
> `AGENTS.md` files** (not symlinked) to support the beads (`bd`) integration:
> bd's setup recipes write and verify their own managed blocks in each file
> (`bd setup claude` targets `CLAUDE.md`, `bd setup codex` targets `AGENTS.md`),
> and they must remain independently editable by bd. `CLAUDE.md` is a thin
> pointer back to this file for everything project-specific.

## Project Overview

**Hank (`hank`)** is a read-only Rust terminal UI (ratatui + crossterm)
that federates N beads repositories into one persistent `bd` hub workspace and
answers "what's ready to work on across all my repos?" It shows a cross-repo
ready list with a detail pane, cross-repo search (`/`), repo/priority filters,
and a copy-context action (`y`/`Y` via OSC 52). Hank never writes issue data;
its only source-repo write is `bd export` refreshing `.beads/issues.jsonl`.

Requires `bd` >= 1.1.0 with `schema_version == 1` on `PATH` at runtime (gated
at startup; constants in `src/cli.rs`). Rust edition 2024.

## Build, Test, Run

```bash
cargo build                                    # build
cargo run                                      # launch the TUI (bare hank)
cargo run -- snapshot                          # headless ready list
cargo fmt --check                              # quality gate: formatting
cargo clippy --all-targets -- -D warnings      # quality gate: lints
cargo test                                     # quality gate: unit + render tests (green without bd)
cargo test --test bd_integration               # quality gate: gated e2e (skips per-test without bd)
```

A `Makefile` wraps these (`make check`, `make test-all`, `make install`, ...);
run `make help` for the full target list. Keep it in sync with this section.

The four quality-gate commands are the project's constant verification suite.
Unit tests never touch real XDG paths or a real `bd` — they use
`Paths::with_base` and `FakeBdClient`. The integration suite builds real
fixture repos with `bd` in tempdirs and prints an explicit `SKIP` line per test
when `bd` is missing.

## Architecture

- **Hub, not custom store**: aggregation is a `bd` hub workspace under
  `<data_dir>/hank/hub` using bd's multi-repo hydration
  (`bd repo add` + `bd repo sync`). Every read goes through `bd … --json`
  subprocess calls against the hub; Hank never reimplements ready/blocked
  semantics.
- **Repo attribution**: bd's JSON has no source-repo field, so Hank maps issue
  ids to repos by longest id prefix (from each repo's effective `bd` prefix),
  flagging collisions.
- **Refresh**: async and TUI-owned — export each repo + one hub sync on a
  worker thread; an advisory lock on `<hub>/.hank.lock` serializes concurrent
  Hank instances.
- **Pure state core**: the TUI is `reduce(&mut App, Msg) -> Vec<Effect>` with
  no I/O, clock, or threads inside; the runtime performs effects. `view::draw`
  is pure over `(App, now)` and tested with ratatui's `TestBackend`.

Module map (`src/`):

| Module | Responsibility |
| --- | --- |
| `config` | Roster (`config.toml`) load/save (atomic), XDG `Paths` |
| `bd/` | `BdClient` trait, real `BdCli` subprocess impl, `FakeBdClient` test double, serde types |
| `hub` | Hub lifecycle: create on first run, reconcile roster, guarded `reset` |
| `refresh` | Export-all + sync + prefix→repo attribution map, advisory lock |
| `snapshot` | Read model: `bd ready` → attributed, sorted rows (also `hank snapshot --json`) |
| `app/` | `mod` (pure `reduce` core), `view` (renderer), `keys` (crossterm→`Msg`, only file importing crossterm), `context` (copy builders + OSC 52) |
| `runtime` | Event loop: event + refresh worker threads feed one mpsc channel |
| `cli` | Headless runners (`snapshot`, `doctor`, `reset`, `repos`), version gate |

## Conventions & Gotchas

- Development favors TDD in vertical slices, especially for behavior changes,
  parsers, migration logic, and public interfaces.
- serde forward-compatibility: any key bd omits when empty is
  `Option`/`#[serde(default)]`; never add `#[serde(deny_unknown_fields)]`.
- `bd repo list --json` is broken in bd 1.1.0 (ignores `--json`), so the hub's
  roster is read from `<hub>/.beads/config.yaml` `repos.additional` instead.
- Canonical user state lives at `<config_root>/hank/config.toml` and
  `<data_root>/hank/ui_state.json`. Startup safely migrates only those two
  user-owned legacy files from `federated-beads/`; it never copies the legacy
  hub, cache, locks, or temp state. `hank reset` skips migration.
- ratatui is pinned with the `unstable-rendered-line-info` feature for
  `Paragraph::line_count` (detail-pane scroll clamping); stay within 0.30.x.
- Only `main.rs` resolves real paths, spawns the real `bd`, and wires
  stdout/stderr; everything else takes injected `BdClient`/`Paths`/writers.
- Clippy warnings are errors in the gate; keep `cargo clippy --all-targets
  -- -D warnings` clean.

## Non-Interactive Shell Commands

**ALWAYS use non-interactive flags** with file operations to avoid hanging on confirmation prompts.

Shell commands like `cp`, `mv`, and `rm` may be aliased to include `-i` (interactive) mode on some systems, causing the agent to hang indefinitely waiting for y/n input.

**Use these forms instead:**
```bash
# Force overwrite without prompting
cp -f source dest           # NOT: cp source dest
mv -f source dest           # NOT: mv source dest
rm -f file                  # NOT: rm file

# For recursive operations
rm -rf directory            # NOT: rm -r directory
cp -rf source dest          # NOT: cp -r source dest
```

**Other commands that may prompt:**
- `scp` - use `-o BatchMode=yes` for non-interactive
- `ssh` - use `-o BatchMode=yes` to fail instead of prompting
- `apt-get` - use `-y` flag
- `brew` - use `HOMEBREW_NO_AUTO_UPDATE=1` env var

<!-- BEGIN BEADS INTEGRATION v:1 profile:minimal hash:6cd5cc61 -->
## Beads Issue Tracker

This project uses **bd (beads)** for issue tracking. Run `bd prime` to see full workflow context and commands.

### Quick Reference

```bash
bd ready              # Find available work
bd show <id>          # View issue details
bd update <id> --claim  # Claim work
bd close <id>         # Complete work
```

### Rules

- Use `bd` for ALL task tracking — do NOT use TodoWrite, TaskCreate, or markdown TODO lists
- Run `bd prime` for detailed command reference and session close protocol
- Use `bd remember` for persistent knowledge — do NOT use MEMORY.md files

**Architecture in one line:** issues live in a local Dolt DB; sync uses `refs/dolt/data` on your git remote; `.beads/issues.jsonl` is a passive export. See https://github.com/gastownhall/beads/blob/main/docs/SYNC_CONCEPTS.md for details and anti-patterns.

## Agent Context Profiles

The managed Beads block is task-tracking guidance, not permission to override repository, user, or orchestrator instructions.

- **Conservative (default)**: Use `bd` for task tracking. Do not run git commits, git pushes, or Dolt remote sync unless explicitly asked. At handoff, report changed files, validation, and suggested next commands.
- **Minimal**: Keep tool instruction files as pointers to `bd prime`; use the same conservative git policy unless active instructions say otherwise.
- **Team-maintainer**: Only when the repository explicitly opts in, agents may close beads, run quality gates, commit, and push as part of session close. A current "do not commit" or "do not push" instruction still wins.

## Session Completion

This protocol applies when ending a Beads implementation workflow. It is subordinate to explicit user, repository, and orchestrator instructions.

1. **File issues for remaining work** - Create beads for anything that needs follow-up
2. **Run quality gates** (if code changed) - Tests, linters, builds
3. **Update issue status** - Close finished work, update in-progress items
4. **Handle git/sync by active profile**:
   ```bash
   # Conservative/minimal/default: report status and proposed commands; wait for approval.
   git status

   # Team-maintainer opt-in only, unless current instructions forbid it:
   git pull --rebase
   git push
   git status
   ```
5. **Hand off** - Summarize changes, validation, issue status, and any blocked sync/commit/push step

**Critical rules:**
- Explicit user or orchestrator instructions override this Beads block.
- Do not commit or push without clear authority from the active profile or the current user request.
- If a required sync or push is blocked, stop and report the exact command and error.
<!-- END BEADS INTEGRATION -->

<!-- BEGIN BEADS CODEX SETUP: generated by bd setup codex -->
## Beads Issue Tracker

Use Beads (`bd`) for durable task tracking in repositories that include it. Use the `beads` skill at `.agents/skills/beads/SKILL.md` (project install) or `~/.agents/skills/beads/SKILL.md` (global install) for Beads workflow guidance, then use the `bd` CLI for issue operations.

### Quick Reference

```bash
bd ready                # Find available work
bd show <id>            # View issue details
bd update <id> --claim  # Claim work
bd close <id>           # Complete work
bd prime                # Refresh Beads context
```

### Rules

- Use `bd` for all task tracking; do not create markdown TODO lists.
- Run `bd prime` when Beads context is missing or stale. Codex 0.129.0+ can load Beads context automatically through native hooks; use `/hooks` to inspect or toggle them.
- Keep persistent project memory in Beads via `bd remember`; do not create ad hoc memory files.

**Architecture in one line:** issues live in a local Dolt DB; sync uses `refs/dolt/data` on your git remote; `.beads/issues.jsonl` is a passive export. See https://github.com/gastownhall/beads/blob/main/docs/SYNC_CONCEPTS.md for details and anti-patterns.
<!-- END BEADS CODEX SETUP -->
