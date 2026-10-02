# Upgrading from the `fbd` RC

On the first Hank command that uses user state, Hank checks the legacy
`federated-beads/config.toml` and `federated-beads/ui_state.json` locations. A
valid legacy file is copied to its canonical Hank path with no-clobber,
atomic publication; the legacy file is retained for rollback. A canonical Hank
file always wins. Relative roster paths keep their legacy meaning.

Legacy hub, snapshot-cache, lock, and temporary files are derived state and are
not copied; Hank rebuilds them under `hank/`. A malformed legacy config stops
normal commands with an actionable path instead of silently loading an empty
roster; `hank doctor` reports the problem. Invalid legacy UI state is skipped
and the repository view safely defaults to `All repos`. `hank reset` does not
perform migration and touches only canonical Hank hub/cache state.
