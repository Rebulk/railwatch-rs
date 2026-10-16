# Efficiency pass driven by flamegraphs (branch perf, from main 7b18e5c)

## Method

- Profiling build: `RUSTFLAGS="-C force-frame-pointers=yes" CFLAGS="-fno-omit-frame-pointer -g"
  CARGO_PROFILE_RELEASE_DEBUG=line-tables-only CARGO_TARGET_DIR=target-prof cargo build --release --workspace`
  (frame pointers through bundled SQLite too, so syscalls have their callers).
- `bench/flame/ingest.sh LABEL ENVS CONC SECONDS [RATE]`: fresh server on :8801 pinned to CPUs 8-11,
  rw-load on 12-15, data on /dev/shm (RAM: measures the server, not this disk), 3 s warm-up,
  `perf record -F 999 --call-graph fp` on the server pid, CPU-µs/event = server utime+stime over the
  measured window / events accepted. SVGs in bench/results/flame/; `bench/flame/analyze.py FOLDED`
  splits by thread, self/inclusive frames, `--callers FRAME`.
- The workstation is shared (load 2-6 during runs); throughput varies ±10% run to run and an
  occasional run is far off. A/B runs alternate binaries; numbers below are medians of the stated runs.

## Changes

| # | change | scenario | before | after |
|---|---|---|---|---|
| 1 | SQLite built without `SQLITE_ENABLE_MEMORY_MANAGEMENT`, with `SQLITE_DEFAULT_MEMSTATUS=0` (.cargo/config.toml) | a: closed loop, 10 tenants, conc 32, 20 s (4 runs each) | 70.8k ev/s (65-71k), 57 CPU-µs/ev, p99 0.72-0.84 s, futex 15.8% of CPU | **87.9k ev/s** (3 of 4 runs 86-88k), **45.5 CPU-µs/ev**, p99 0.50-0.65 s, futex 0.2% |
| 2 | Incremental rollup's id lookup filters `+kind` (rollup_job.rs `select_by_ids_sql`) | b: 1 tenant, 40 batches/s (20k ev/s), 150 s, single file (1 run each, then folded into 3-5) | rollups thread 91 s CPU | 44 s |
| 3 | Tenant writer `PRAGMA cache_spill = OFF` (tenants.rs) | b, single file, 3 runs each, on top of 2 | late-run writer 26.7 ms/batch, p99 966-1001 ms | 23.6 ms/batch, p99 247-693 ms |
| 4 | Grouping keeps each group's affected users (`grouping::AffectedUsers`) and reads only newer exceptions by rowid, including user-less ones | b, per-day, 2 runs | rollups thread 45 s, 64.7 CPU-µs/ev, p99 50-57 ms | **26 s, 57.5 CPU-µs/ev, p99 40-43 ms** |
| 2-4 | all of the above, merged with storage (88a939e) | b, **single file**, 2 runs, main vs perf | 19.3k ev/s (fell behind), p50 26 ms, **p99 1034-1079 ms**, 86 CPU-µs/ev, rollups 91 s | 20.0k ev/s, p50 26 ms, **p99 65-79 ms**, 59.6 CPU-µs/ev, rollups 27 s |
| 2-4 | same | b, **per-day** (new default), 2 runs | p99 89-104 ms, 80.4 CPU-µs/ev, rollups 92 s | p99 40-43 ms, 57.5 CPU-µs/ev, rollups 26 s |
| 2-4 | same | a: closed loop, 10 tenants, conc 32, 20 s, per-day, 3 runs | 93.3k ev/s, p99 478-542 ms, 42.8 CPU-µs/ev | **97.0k ev/s**, p99 331-344 ms, 41.0 CPU-µs/ev |
| 5 | Per-day reader: hinted union views instead of stripping `INDEXED BY` / `NOT INDEXED` (storage/reader.rs, dashboard/db.rs) | dashboard, 5-day per-day tenant (325k rows), 122 cases x 5 warm, 2 runs | sum of medians 817 ms; tenants@7d 20.1, jobs.q_text@7d 15.2, jobs.q_class@7d 8.4 ms | 773-782 ms; 11.7, 4.9, 3.0 ms |

### 1. The futex share was SQLite's two process-wide mutexes

The baseline flamegraph's ~38% futex (21% futex_wake, spinning on the run-queue lock in
try_to_wake_up) is not Rust's channels. With callers attached, every futex syscall comes from
SQLite: `pcache1Fetch`/`pcache1Unpin` (4.5% + 2.3% + 0.5%), `sqlite3Malloc` via `mallocWithAlarm`
and `memjrnlWrite` (2.6% + 2.2% + 0.7%), `sqlite3_free` (1.0%), the statement journal's
`memjrnlFreeChunks` (0.8%). The oneshot replies, the std mpsc channel, the follow-up channel and
the semaphore are all below 0.1%.

Why: libsqlite3-sys's bundled build defines `SQLITE_ENABLE_MEMORY_MANAGEMENT`, which forces
pcache1 into a single global page-cache group (`separateCache = 0`) guarded by
`SQLITE_MUTEX_STATIC_LRU`, so every page fetch/unpin on every tenant's connection takes one
process-wide lock; and the default `MEMSTATUS=1` takes the `mem0` mutex on every sqlite3_malloc
and free (the per-savepoint in-memory statement journal allocates per page). Ten writer threads
plus the rollups thread and the dashboard readers contend on both.

Nothing in the server uses what those options buy (`sqlite3_release_memory`,
`soft_heap_limit64`, `sqlite3_status` memory counters: grep is empty). Each connection now gets its
own page-cache group with no mutex taken, and malloc goes straight to the allocator. No change
to any file, query or result: cargo test 100/100; bench/diff/run.sh every table and ack identical.

### p99 in scenario b: the rollups thread fell behind, then the writer did

Not a checkpoint stall, not lock contention. Evidence, scenario b single file:

- `lock_ms` (BEGIN IMMEDIATE wait) p99 0 ms, max 8-27 ms across every run: the rollups writer
  never held the write lock long enough to matter.
- With rollups and grouping switched off (a temporary hook, not committed) the same run is flat:
  p99 29 ms, writer 19-20 ms/batch for all 150 s, WAL never above 12 MB, rollups thread 0.1 s.
- With them on, the rollups thread's busy share climbs every 10 s (2% -> 115%) until it cannot
  keep up; from then on its read transaction is almost always open, so the WAL cannot reset
  (64 MB -> 0.5-1.7 GB), the writer's autocheckpoint re-reads a growing WAL and the writer goes
  from 20 to 26-28 ms/batch. At 40 batches/s that is >100% busy: batches coalesce 16 per
  transaction and every request waits ~0.5 s. That is the 1 s p99.
- Major faults per run: 0-4. /dev/shm was not swapping.

Why the rollups thread grew with the file (flamegraph b-late, 41% of all samples):

- **#2.** `select_by_ids_sql` (the incremental rollup reading a batch's new rows by id) said
  `executions.kind = ? AND id IN (...)`. SQLite chose `idx_executions_kind_outcome_cursor
  (kind=?)` and walked every row of the kind for each lookup: 18 ms at 400k rows vs 0.04 ms with
  `+kind` (INTEGER PRIMARY KEY plan). 53% of the rollups thread.
- **#4.** update_affected_users is `COUNT(DISTINCT user_ref) FROM exceptions WHERE group_hash = ?`
  per group per batch, over every retained exception of the group (Rails does the same; the gem
  0.9.1 throttles it to once per 5 min per issue, the cloud does not). 22% of the thread, growing
  with the issue. Now kept per tenant: each group's user set and the last exception id read;
  each call reads only `id > last` (`+group_hash` so it walks rowids, not the group's index
  range). Ids only grow (AUTOINCREMENT; on per-day, meta's sqlite_sequence is the mark across
  files) and the cache is cleared on prune, the only path that deletes exceptions. The first
  version filtered `user_ref IS NOT NULL`, so groups without users (rw-load's exceptions; any app
  without user context) never moved their mark; fixed in 4e373f5. Stored value unchanged:
  tests/background.rs compares it with the full COUNT after every batch across a prune (and fails
  if the prune does not clear it); bench/diff/grouping_diff.sh 40/40 vs rw-baseline at chunk 50,
  chunk 7 and resolve+repeat, rw-group now using one cache across chunks.

**#3**, the writer itself: `pagerStress` + `walRewriteChecksums` were ~30% of the writer once the
file outgrew the 2000-page cache. A batch dirties more pages than that (a leaf per random index
key), SQLite spilled them into the WAL mid-transaction and re-read them at COMMIT to rewrite
checksums. With `cache_spill = OFF` they stay in memory until COMMIT. Durability is unchanged
(only committed frames are ever valid either way); cost is one transaction's dirty pages in RAM
(a few MB). A larger `cache_size` (64 MB) helped p99 in 2 of 3 runs but not CPU; not adopted.

Ruled out with evidence: rollup-writer lock contention (lock_ms above), allocation (no
malloc/free above 1% self), coalescing unfairness (one tenant; batches/txn is 1.0 until the
writer saturates, then 16), B-tree page splits (`balance` 7-10% of the writer, flat over the
run). `wal_autocheckpoint=0` without a checkpointer grew the WAL to 4 GB and the writer died
("database is locked" on the rollups flush, then disk I/O); a dedicated checkpointer was not
needed once the reader stopped pinning the WAL.

### 5. Per-day pages: the hint, not the attach

On a 5-day per-day tenant (325k rows) the dashboard was 1.18x median / 2.21x p90 of the same
rows in one file. Small pages are not paying a fixed attach/view cost: their median is 0.33 ms
single vs 0.36 ms per-day. The regression is per statement (RW_SQL_PROFILE=1 in props-dump
prints each statement's time): the reader stripped `INDEXED BY` / `NOT INDEXED` because a view
cannot take them, so in every day file SQLite picked the kind+occurred_at range instead of the
covering index the hint names (tenants summary 0.0 -> 5.5 ms per statement; jobs' name/preview
filter 0.25 -> 6 ms). attach_all now also creates, per hint the dashboard uses, the same union
with the hint on each branch (`executions__idx_executions_tenant_summary`, ...), and db::prepare
points the hinted table at it. `idx_queries_slowest` is the exception: per day file it was
slower (queries@1h 1.9 -> 9.9 ms), so that hint stays stripped. Props 122/122 identical to the
single file; bench/diff/props/run.sh 130/130 single and SPLIT=1; issues 40/42 on both layouts,
the same 40/42 as main (two detector-made issues, docs/STORAGE.md).

What is still slower per-day: logs.fts@7d 56 -> 80 ms (FTS5 MATCH + snippet run per file: 13
files) and the queries@7d family 68 -> 82 ms (ORDER BY duration LIMIT 51 needs every file's top
51 merged). Both are per-file work that grows with the number of files, not waste; a merged FTS
index or a per-day top-N cache would change what is stored, so not done.

### Not waste (measured, left alone)

- Idle: 3 environments, 60 s: 0.01 s CPU, ~5 wakeups/s, 18 MB RSS, 6 threads.
- Rollups flush (`fresh_attributes` recomputing each dirty group's t-digest every 5 s) is now the
  biggest remaining part of the rollups thread, which is 26-27 s of the server's 179 s CPU in a
  150 s scenario-b run (the split inside the thread after #4 was not re-profiled).
  Making it incremental means merging t-digests, which changes digest bytes vs a full recompute
  (DIFFERENCES.md promises byte-identical); left open.

### Harness notes

- `SHM=/dev/shm/<own>`: another session reusing /dev/shm/rwperf deleted a run's tenant mid-run
  (disk I/O error, writer stopped) and poisoned an A/B; runs here used /dev/shm/rwperf-perf.
- `LAYOUT=single` (ingest.sh, bench/diff/run.sh) swaps in an empty single file; default is per-day.
- bench/diff/run.sh exports a per-day tenant to compare it. logs_fts_data/_idx then differ from
  Rails (export rebuilds FTS5 segments; rows identical; same on main). LAYOUT=single is byte-level:
  acks 26/26, every table identical.
- The workstation swapped 27 GB of zram throughout; major faults were checked per run (0-4).

## Proposals

None of these is implemented.

1. **The writer still checkpoints inline.** `sqlite3WalCheckpoint` is 13-19% of the writer under
   load. A checkpointer thread per tenant (`wal_autocheckpoint=0` on the writer, PASSIVE every
   ~1 s elsewhere) would take that off the ack path. Durability is the same, but it needs a
   bound on WAL growth when a reader pins it (the experiment without one reached 4 GB and failed).
   Worth it only if the ack-path p99 matters more than one more thread per tenant.
2. **`synchronous = OFF` was not tried** (not allowed). The bench runs on tmpfs, where fsync is
   free, so these numbers say nothing about it either way; on disk, measure before arguing.
3. **affected_users in Rails is O(occurrences) per batch.** The gem already throttles it to once
   per 5 min per issue; railwatch-cloud does not. Rust now computes the same number cheaply, so
   nothing to change here, but the cloud has the same cost.
4. **Incremental t-digests** (see "Not waste"): at most the rollups thread's 15% of server CPU at
   20k ev/s; changes digest bytes vs a full recompute.
