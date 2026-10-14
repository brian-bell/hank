# Architecture

How Hank is put together. For installation and usage see the [README](../README.md);
agent-oriented notes live in [AGENTS.md](../AGENTS.md).

```
Source repos            Hank                              Hub (bd workspace)
──────────────   ──────────────────────────────   ─────────────────────────
~/dev/megaclock  refresh:  bd export per repo  →   <XDG data dir>/
~/dev/reading-…            bd repo sync (once)  →     hank/hub/
     …           read:     bd ready/list/blocked/show/search --json (all through the hub)
```

- **Central DB**: a `bd` "hub" workspace using built-in multi-repo hydration
  (`bd repo add` + `bd repo sync`), not a custom aggregation store.
- **Read path**: every query goes through the hub via `bd … --json` subprocess
  calls. `bd` owns ready/blocked semantics; Hank never reimplements them.
- **Repo attribution**: `bd`'s JSON does not expose a source repo, so Hank maps
  each issue id to its repo by **longest id prefix** (read from each repo's
  effective `bd` prefix), detecting and flagging prefix collisions.
- **Refresh**: TUI-owned and async. Launch and the `r` key run content-stable
  exports with at most four source repositories in parallel, then one hub sync.
  Unchanged exports retain their canonical inode and mtime; changed exports are
  atomically replaced while preserving the platform's standard permission
  object. Ownership, ACLs, xattrs, labels, file flags, and inode identity are
  not part of the changed-export metadata contract. The stale list stays
  browsable, and `<hub>/.hank.lock` serializes concurrent Hank instances.
- **Live refresh** (opt-in): one `bd events tail --follow` per repo feeds a
  debounced batcher; a batch refreshes only the repos that changed, queued
  behind any refresh already running. See [Live refresh](../README.md#live-refresh).
- **Startup**: valid cached rows paint with their original age, even beyond
  the ordinary 12h cache freshness limit, while a full refresh runs. Cache
  membership ignores order and watch preferences; per-row source paths in new
  files allow removed repos to be filtered. A satisfied on-disk reconciliation
  witness bypasses redundant hub setup even on the first refresh.
  `<hub>/.hank-prefixes.json` stores prefix hints under the hub refresh lock;
  every reused hint must match a fresh non-empty export, and hints never allow
  an unverified repo to skip export. Hub reset removes these derived hints.
  During startup the watcher reports changes before debounce. Each source
  export covers only reports observed before it begins. Reports during/after
  export remain pending; any export/sync/read failure retains covered reports
  for retry. Truncation and batches delivered after startup conservatively
  retain their normal refresh behavior. `--startup-timings <path>` records
  cache diagnostics, frame timings, phase durations, and operation counts on exit.
- **State core**: the whole TUI is a pure `reduce(&mut App, Msg) -> Vec<Effect>`
  state machine (no I/O, no clock, no threads inside), so it is exhaustively
  unit-tested; the runtime performs the effects. The last confirmed repository
  view and status are stored separately in versioned `ui_state.json`;
  snapshot-cache freshness and roster configuration remain independent.
- **Status filter**: the ready list holds one status at a time (`ready`,
  `open`, `in_progress`, `blocked`; `snapshot::read_issues`). Refreshes read
  the current status and stamp it on the snapshot, and the app drops a
  snapshot of any other status. Stepping the status (`s`) reloads from the
  already-synced hub (`Effect::LoadStatus`) instead of re-exporting; one load
  runs at a time (each holds the hub lock), and steps taken meanwhile coalesce
  into one load of the final status. Before any list has loaded (a launch
  whose read of the saved status failed), `s` runs a full refresh instead. `blocked`
  is the union of `bd blocked` and `bd list --status blocked`; search asks
  `bd blocked` too, so its in-place status filter agrees.

Module map: `config` (roster + XDG paths) · `ui_state` (persisted TUI
preferences) · `bd` (the `BdClient` trait, real subprocess + fake impls, serde
types) · `hub` (lifecycle) · `refresh` (export + sync + prefix map) · `snapshot`
(the read model) · `app` (`reduce` core, `view` renderer, `keys` mapping,
`context` copy builders) · `runtime` (the event loop and workers) · `watch`
(the opt-in events-journal follower) · `cli` (headless subcommands).
