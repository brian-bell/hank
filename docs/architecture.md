# Architecture

How Hank is put together. For installation and usage see the [README](../README.md);
agent-oriented notes live in [AGENTS.md](../AGENTS.md).

```
Source repos            Hank                              Hub (bd workspace)
──────────────   ──────────────────────────────   ─────────────────────────
~/dev/megaclock  refresh:  bd export per repo  →   <XDG data dir>/
~/dev/reading-…            bd repo sync (once)  →     hank/hub/
     …           read:     bd ready/show/search --json (all through the hub)
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
- **State core**: the whole TUI is a pure `reduce(&mut App, Msg) -> Vec<Effect>`
  state machine (no I/O, no clock, no threads inside), so it is exhaustively
  unit-tested; the runtime performs the effects. The last confirmed repository
  view is stored separately in versioned `ui_state.json`; snapshot-cache
  freshness and roster configuration remain independent.

Module map: `config` (roster + XDG paths) · `ui_state` (persisted TUI
preferences) · `bd` (the `BdClient` trait, real subprocess + fake impls, serde
types) · `hub` (lifecycle) · `refresh` (export + sync + prefix map) · `snapshot`
(the read model) · `app` (`reduce` core, `view` renderer, `keys` mapping,
`context` copy builders) · `runtime` (the event loop and workers) · `watch`
(the opt-in events-journal follower) · `cli` (headless subcommands).
