# Hank

A terminal UI that answers **"what's ready to work on across all my
[beads](https://github.com/gastownhall/beads) repos?"** It federates N beads
repositories into a persistent hub database that `bd` itself maintains
(multi-repo hydration) and presents a cross-repo ready-work list with a detail
pane, cross-repo search, and a copy-context action.

Refresh runs `bd export` to refresh each source repo's `.beads/issues.jsonl`.
Live refresh manages the `events-journal` setting in `.beads/config.yaml`
(see [Live refresh](#live-refresh)). The copy-context key hands you a
ready-to-run command for your terminal.

## Requirements

- `bd` (beads) **>= 1.1.0** with `schema_version == 1` on `PATH` at runtime.
  Hank checks this at startup and refuses a version it cannot vouch for.
- Optional live refresh (`hank --watch`) needs `bd` **>= 1.3.0** (see
  [Live refresh](#live-refresh)).

Prebuilt binaries are available for Apple Silicon and Intel macOS, plus ARM64
and x86_64 GNU/Linux. Building from source requires Rust 1.88 or newer.

## Install

Install the latest release:

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/brian-bell/hank/releases/latest/download/hank-installer.sh | sh
```

The installer places `hank` in `$CARGO_HOME/bin`, falling back to
`~/.cargo/bin`, and tells you if that directory needs to be added to `PATH`.
`bd` remains a separate runtime requirement.

Rust users can install from crates.io. The crate is `hank-tui` (the name `hank`
belongs to an unrelated crate); the binary it installs is still `hank`:

```bash
cargo binstall hank-tui   # prebuilt binary from the GitHub release, no compile
cargo install hank-tui    # build from source
```

To build from a local source checkout instead:

```bash
cargo install --path .
```

This builds and installs `hank` into Cargo's binary directory.

## Quickstart

```bash
hank repos discover ~/dev --add   # find every ~/dev/*/.beads repo and add it
hank                              # launch the TUI
```

`discover` without `--add` previews what it found without changing anything.
The hub database is created automatically on first run under your XDG data dir
and is disposable derived data (see `hank reset`).

## Live refresh

Off by default. Launch with `hank --watch`, or set `watch = true` at the top of
`config.toml`, and Hank follows each repo's `bd` events journal
(`bd events tail --follow`) in the background. When a repo changes, Hank
re-exports just that repo and re-syncs the hub, so the list updates without
pressing `r`. The status bar shows `live` while the watcher runs. A repo added
with `hank repos add` while the TUI is open is followed from its next refresh.

The journal is per workspace and off by default. With live refresh on, Hank
turns it on for you in every roster repo where it is off, by running
`bd -C <repo> config set events-journal true`, and says so in the status bar.
`hank repos add` and `hank repos discover --add` do the same for new repos when
`watch = true`. bd keeps this setting in the repo's `.beads/config.yaml`, which
is git-tracked, so you will see a one-line change there to commit (Hank never
commits). bd reads only that file for this setting, so there is no
per-machine alternative.

To stop Hank from managing a repo's journal, opt it out:

```bash
hank repos unwatch ~/dev/megaclock   # journal off; live refresh leaves it alone
hank repos watch ~/dev/megaclock     # journal on; clears the opt-out
hank repos watch --all               # every roster repo
```

`unwatch` records `unwatched = true` on the repo's roster entry. A repo whose
journal is off is re-checked while the TUI runs, so turning it on later needs no
restart. `hank doctor` prints `journal: on|off` for every roster repo, marking
opted-out ones `(unwatched)`. Hank saves the last journal position per repo in
`hank/events_checkpoints.json` under the data dir and resumes from it; if the
journal has pruned past that position, Hank falls back to a full refresh.
Syncs such as `bd dolt pull` are not journaled, so press `r` after pulling;
every launch is also a full refresh.

## Keybindings (TUI)

| Key            | Action                                                    |
| -------------- | --------------------------------------------------------- |
| `j` / `↓`      | Move selection down (next issue while in detail)          |
| `k` / `↑`      | Move selection up (previous issue while in detail)        |
| `J` / `K`      | Scroll the detail pane one row down / up                  |
| `PgDn` / `PgUp`| Scroll the detail pane one page down / up                 |
| `f`            | Open the repository picker (`All repos` is first)          |
| `p`            | Toggle the priority filter: All ↔ P0/P1 only              |
| `/`            | Open cross-repo search                                    |
| `Enter`        | Open the detail pane for the selected issue               |
| `y`            | Copy `cd <repo> && bd show <id>` for the selected issue   |
| `Y`            | Copy a markdown block (title / id / repo / description)   |
| `r`            | Refresh (re-export every repo, re-sync the hub)           |
| `h`            | Open / close the sync-health panel                        |
| `Esc`          | Leave the current sub-mode (detail / search) → list       |
| `q`            | Quit                                                      |

The repository picker is available from the ready list and settled search
results. Move its pending choice with `j`/`k` or the arrow keys, confirm with
`Enter`, or cancel with `Esc`. A confirmed repository view applies globally to
both ready and search results and is restored on the next launch. `All repos`
is used on first run and whenever saved UI state is missing or invalid.

The sync-health panel (`h`) lists each roster repo with how fresh the hub's
copy is (when it last exported cleanly: "synced 3m ago") and, when a repo's latest refresh failed, flags it
**STALE** with the reason; the hub keeps that repo's last good export until a
refresh succeeds. Below that it shows `hank doctor`'s findings (bd version gate,
paths, per-repo prefix and journal state), run fresh each time the panel opens.
Stale repos are also flagged on their list headers and counted in the status
bar. Scroll the panel with `j`/`k` or `PgUp`/`PgDn`; close it with `Esc` or `h`.

`y`/`Y` place the text on your system clipboard via an **OSC 52** terminal
escape — no native clipboard dependency, and it works over ssh. For an
unattributed issue (an id matching no configured repo prefix) the copied command
falls back to `bd -C <hub> show <id>`, which is always runnable.

**Clipboard/tmux caveat:** OSC 52 requires a terminal that honors it (most
modern terminals do). Under tmux, enable it with `set -g set-clipboard on` (and,
depending on version, `set -g allow-passthrough on`). Hank emits the standard
sequence and does not wrap it for tmux passthrough in v1.

## Commands (headless)

```bash
hank snapshot [--json]   # print the merged, attributed ready list (no TUI)
hank doctor              # bd version + gate, config/hub paths, per-repo health + journal
hank reset               # delete the hub DB; rebuilt on the next snapshot/launch
hank repos add <path>    # add a beads repo to the roster
hank repos remove <path> # drop a repo from the roster and the hub
hank repos list          # print the roster
hank repos discover <dir> [--add]   # scan <dir>/*/.beads one level deep
hank repos watch <path>|--all       # turn a repo's events journal on (live refresh)
hank repos unwatch <path>|--all     # turn it off and opt the repo out
```

The roster's source of truth is `hank/config.toml` under your
platform config dir (`~/.config` on Linux, `~/Library/Application Support` on
macOS); the `repos` subcommands edit it, and `hank doctor` prints the exact
paths in use. Missing paths warn, never fail.

The last confirmed repository view is stored independently at
`hank/ui_state.json` under the platform data directory. `hank reset`
does not remove this user preference; it only discards derived hub/cache data.

## How it works

Hank reads everything through a `bd` hub workspace that `bd` itself hydrates
from your repos (`bd repo add` + `bd repo sync`). Refresh exports each source
repo and syncs the hub once; every query then goes through the hub via
`bd … --json`, so `bd` owns ready/blocked semantics. Issues are attributed to
repos by longest id prefix. See [docs/architecture.md](docs/architecture.md)
for the refresh pipeline, state core, and module map.

## Verification commands

The project's quality gate:

```bash
cargo fmt --check                              # formatting
cargo clippy --all-targets -- -D warnings      # lints (warnings are errors)
cargo test                                     # unit + render tests (green without bd)
cargo test --test bd_integration               # gated e2e (skips cleanly without bd)
```

The integration suite builds real fixture repos with `bd` in tempdirs; each test
skips with an explicit `SKIP` line when `bd` is not installed. CI runs it in a
dedicated job with a pinned `bd` and fails if any test skips.

The ignored, machine-dependent refresh matrix is available separately:

```bash
cargo test refresh_performance_matrix -- --ignored --nocapture
```

Recorded phase timings live in `docs/performance/refresh.md`.

## Not in v1 (planned)

- A blocked-issues view (v1 shows only ready work).
- A background daemon. Live refresh (`--watch`) runs only while the TUI is open.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.
