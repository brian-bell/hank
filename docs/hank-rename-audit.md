# Hank old-name audit

This is the durable local audit evidence originally recorded for
`federated-beads-9mq.4` (renamed `hank-795a95f0`). It was refreshed for Hank
`0.1.0-rc.2` after the explicitly authorized GitHub and Beads-prefix migration.

Run the token-aware audit from the repository root:

```bash
rg -n -i --hidden \
  --glob '!target/**' --glob '!.git' --glob '!.git/**' \
  '(^|[^[:alnum:]])fbd([^[:alnum:]]|$)|federated-beads' .
```

Every remaining semantic match belongs to one of these required categories:

- **Legacy migration literals:** `src/config.rs`, its unit tests,
  `tests/hank_binary.rs`, the README upgrade section, and old `.fbd.lock` /
  `.issues.jsonl.fbd.*.tmp` fixtures. Hank must recognize or prove it does not
  copy these exact legacy names.
- **Historical tracker/storage evidence:** historical `federated-beads-*`
  Beads ids in source comments, this audit, and
  `docs/performance/federated-beads-9dt.md`. The Beads database was explicitly
  migrated to the `hank-*` prefix; these old textual references remain as
  provenance rather than active issue ids. The Dolt database and project UUID
  remain durable identities.
- **Immutable rc.1 history:** the README identifies the old `fbd` RC only to
  explain migration. The binary test's negative assertion proves that this
  historical executable name is absent from current help/install surfaces.
- **Stale active references:** none remain. Active package, library, binary,
  command examples, Cargo repository URL, installer URL, state paths, runtime
  markers, Makefile targets, cargo-dist plan, formula identity, Beads prefix,
  GitHub repository URL, Git remote, and Beads sync remote use Hank.

The `.git` worktree indirection is excluded because it is host-managed checkout
infrastructure, not repository content. Cargo.lock checksum substrings are also
excluded by the token-aware expression because they are not semantic names.
