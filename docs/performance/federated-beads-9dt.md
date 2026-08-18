# `federated-beads-9dt` refresh performance evidence

> Historical evidence recorded before the Hank rename. The durable Beads issue
> id and measurements are preserved unchanged.

Recorded 2026-07-28 on:

- Apple arm64, Darwin 25.5.0
- Rust/Cargo 1.97.1
- `bd` 1.1.0, schema version 1, build `8e4e59d39`
- debug test profile

Command:

```bash
cargo test refresh_performance_matrix -- --ignored --nocapture
```

The ignored harness uses two warmups and ten recorded iterations for each
stage/roster combination. Timings are observations, not pass/fail thresholds.
All values below are microseconds.

Stages isolate the three child beads:

1. `baseline_direct_serial`: direct canonical export, serial source work, and
   stateless version/prefix reads.
2. `stable_export_serial`: content-stable publication with one source worker.
3. `bounded_parallel`: content-stable publication with the default maximum of
   four source workers.
4. `retained_tui_state`: bounded workers plus warm session compatibility and
   verified-prefix reuse.

## One repository

| Stage | Total median | Total p95 | Version | Reconcile | Source | Sync | Ready | version/prefix calls | Sync path |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- | --- |
| baseline | 1,117,136 | 1,133,779 | 69,275 | 120 | 558,116 | 288,604 | 199,436 | 1 / 1 | imported 1 |
| stable export | 1,020,405 | 1,035,292 | 68,559 | 113 | 557,602 | 191,055 | 203,470 | 1 / 1 | up to date |
| bounded parallel | 1,022,950 | 1,035,062 | 68,905 | 113 | 560,624 | 191,602 | 201,819 | 1 / 1 | up to date |
| retained TUI | 734,359 | 740,788 | 0 | 97 | 336,043 | 193,903 | 201,920 | 0 / 0 | up to date |

## Five repositories

| Stage | Total median | Total p95 | Version | Reconcile | Source | Sync | Ready | version/prefix calls | Sync path |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- | --- |
| baseline | 3,745,812 | 3,902,420 | 68,916 | 194 | 2,756,791 | 721,061 | 208,181 | 1 / 5 | imported 5 |
| stable export | 3,312,108 | 3,381,971 | 69,410 | 193 | 2,737,417 | 304,021 | 205,785 | 1 / 5 | up to date |
| bounded parallel | 2,422,302 | 2,476,639 | 69,718 | 193 | 1,837,468 | 307,394 | 207,601 | 1 / 5 | up to date |
| retained TUI | 1,681,438 | 1,759,856 | 0 | 186 | 1,162,038 | 308,039 | 207,296 | 0 / 0 | up to date |

## Ten repositories

| Stage | Total median | Total p95 | Version | Reconcile | Source | Sync | Ready | version/prefix calls | Sync path |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- | --- |
| baseline | 7,001,637 | 7,029,380 | 68,871 | 288 | 5,523,768 | 1,204,406 | 204,348 | 1 / 10 | imported 10 |
| stable export | 6,279,037 | 6,372,764 | 69,068 | 291 | 5,560,425 | 445,482 | 211,343 | 1 / 10 | up to date |
| bounded parallel | 4,057,484 | 4,127,606 | 69,394 | 294 | 3,314,966 | 452,161 | 208,183 | 1 / 10 | up to date |
| retained TUI | 2,833,727 | 3,051,793 | 0 | 297 | 2,165,749 | 458,158 | 211,498 | 0 / 0 | up to date |

## Twenty repositories

| Stage | Total median | Total p95 | Version | Reconcile | Source | Sync | Ready | version/prefix calls | Sync path |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- | --- |
| baseline | 23,137,259 | 24,030,146 | 196,492 | 1,012 | 20,053,692 | 2,503,734 | 325,589 | 1 / 20 | imported 20 |
| stable export | 22,281,262 | 23,394,931 | 274,096 | 1,021 | 20,690,245 | 1,021,880 | 360,653 | 1 / 20 | up to date |
| bounded parallel | 19,275,309 | 19,948,919 | 223,868 | 1,133 | 16,708,854 | 1,967,647 | 363,029 | 1 / 20 | up to date |
| retained TUI | 10,484,639 | 10,986,905 | 0 | 729 | 8,236,226 | 1,669,910 | 526,306 | 0 / 0 | up to date |

Every recorded stage performed one export per roster repository, exactly one
hub sync, and exactly one ready read. Stable publication consistently changed
the unchanged sync classification from importing every repository to
`UpToDate`. Parallelism benefits begin above one repository. Warm retained TUI
state eliminated the version call and every prefix subprocess while continuing
to export each repository as the current-state validation witness.
