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

## Slow dashboard pages (branch fix-slow-pages, from main fc318ed)

### Method

`bench/storage/slow_pages.sh` (setup / run / report): one tenant in two data dirs, the single
file and a `railwatch split` copy, with one control DB and API token; every props-diff page case
under every window preset (1h, 6h, 24h, 7d, 30d) and every `/api/v1` environment endpoint under
every window it takes plus filters, sorts and second pages: 276 page + 107 API cases. props-dump
and api-dump on CPUs 4-7; `report` diffs the outputs byte for byte (single vs per-day, and a base
label vs this one in each layout) and lists the slowest cases. Tenant: 1.03M rows over 10 days
(562k queries, 139k logs, 139k cache events, 71k executions, 32k rollups), 600 MB single, 21
files per-day, on /var/tmp (disk). Table: 3 rounds alternating main and branch binaries, 5 warm
runs each (first run dropped), median of the per-round medians, ms. Busy shared workstation.

| case | main single | main per-day | branch single | branch per-day |
|---|---:|---:|---:|---:|
| queries.q@30d | 477.1 | 275.0 | 222.0 | 315.2 |
| queries.q@7d | 385.3 | 216.4 | 178.0 | 246.8 |
| logs.fts@30d | 159.6 | 265.6 | 47.7 | 48.1 |
| logs.fts@7d | 158.5 | 261.4 | 45.5 | 44.5 |
| queries.parallel_cursor@30d | 48.0 | 254.8 | 30.8 | 32.6 |
| api.queries.adapter@30d | 84.9 | 241.5 | 69.7 | 85.0 |
| logs.fts@24h | 150.6 | 238.8 | 38.7 | 32.0 |
| queries.parallel_cursor@7d | 43.6 | 199.8 | 26.1 | 32.7 |
| api.queries.adapter@7d | 62.6 | 183.6 | 52.7 | 67.6 |
| queries@30d | 95.3 | 134.5 | 34.0 | 42.8 |
| queries@7d | 77.6 | 109.8 | 31.5 | 38.0 |
| logs@7d | 35.5 | 47.8 | 34.3 | 33.0 |
| profiles@7d | 7.9 | 31.4 | 7.7 | 7.7 |
| sum of all 383 medians | 4186 | 6273 | 2517 | 2999 |

Pages at 7d over 50 ms: main 5 single / 7 per-day, branch 1 / 1 (queries.q). Cases where
per-day is over 1.2x single and at least 2 ms slower: main 86, branch 42. Every output identical:
main == branch in both layouts, single == per-day, 383/383.

### What each change did (EXPLAIN QUERY PLAN and RW_SQL_PROFILE=1 per statement)

1. **Log snippets** (`filter_query::fts_snippets`): `rowid IN (50 ids)` with MATCH is one FTS5
   pass per id (plan `INDEX 0:=M1`, 120 ms). `rowid BETWEEN min AND max AND +rowid IN (...)` is
   one pass bounded by the range (`INDEX 0:M1><`, 3 ms), same rows. Both layouts.
2. **Rollup decode** (`rollup::load`): straight from the statement instead of through `db::Rec`
   (owned copies of every column first). The 7-day queries page reads ~31k rollup rows 3 times;
   perf had `load`, `from_utf8_lossy` and `parse_db_time` above the SQL.
3. **One rollup read per page** (`grouped_and_summary`, overview): the grouped table and
   summary_with_delta's current window ran the same statement twice.
4. **Per-day cursor pages** (`cursor_page::per_file`): the union view's `ORDER BY duration DESC
   LIMIT 51` sorted every row in range of every file (`MERGE (UNION ALL)` over full ranges). Each
   file now returns its own top N, merged; (col, id) is a total order, so the same rows. Log
   text filters name each file's own `logs_fts` (through the view, the 17-file FTS union ran
   inside each of 17 branches). `idx_queries_slowest` only in files whose whole day is in the
   window.
5. **Per-day aggregates** (`reader::aggregate`, logs' level counts): SQLite does not push
   COUNT / GROUP BY into a view's branches. COUNT, queue stats (AVG as SUM/COUNT of the integer
   column, exact), active people and logs' level counts run per file and are combined; GROUP BY
   keys come back sorted as the single file's temp b-tree returns them. COUNT over 7 days 26 -> 4
   ms per-day.
6. **Filtered slowest queries, single file** (`cursor_page::slowest_probe`): the forced
   `idx_queries_slowest` visits every row of the window in duration order when a filter matches
   nothing (queries.q: 0 of 440k). The page first filters the index's top 2,000 rows; a full page
   from them is the answer, otherwise the statement runs on the occurred_at plan.

`per_day_pages_equal_the_single_file` (dashboard/pages/telemetry.rs) covers what the bench tenant
cannot: four log levels, two queues with real latencies, three users, five day files, the probe's
full-page and fallback paths. It fails if the per-file GROUP BY order or the bind renumbering is
wrong (both were checked by breaking them).

### Targets

- **Every page under 50 ms at 7d: not met for one page.** queries.q@7d (`widgets role:reading`)
  is 178 ms single / 247 ms per-day: the filter matches none of the window's 440k queries, so any
  plan reads all of them (`role` has no index; text is `LIKE '%..%'`). An index on `role` would be
  a schema change to the gem's files and only helps that one field. Every other page at 7d is
  under 50 ms in both layouts (next: cache_events.store 49 ms per-day).
- **No per-day page over 1.2x single: not met.** 42 cases are over 1.2x and at least 2 ms
  slower. Profiled: queries.q (1.39x; single got cheaper), the cache store filter (`SELECT
  DISTINCT group_hash` over the union, 32 -> 49 ms). Not profiled, causes read from the code:
  `/api/v1/logs` without a cursor (3 -> 15 ms; its offset page's `COUNT(*)` and OFFSET query
  still go through the views), queries.parallel_cursor@24h (5 -> 32 ms; the window covers no
  whole day, so no file gets the hint), a ~3 ms fixed cost on `executions#show` (lookups by
  execution_id in every file). In ratio the worst are sub-5 ms pages.

### Not done / did not help

- A MATERIALIZED CTE for the FTS subquery: the view's branches still re-ran it (133 ms).
- A join view pairing `logs` with `logs_fts` per file: FTS5 rejects MATCH through it.
- `idx_queries_slowest` in every file under any filter: queries.q per-day 206 -> 271 ms.
- No index or schema change: files stay byte-compatible with the gem's.
- Not rerun: bench/diff/props, api and mcp against Rails (the `rw-baseline` container was
  stopped); this branch's gate is Rust-only (main vs branch, single vs per-day).

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

## Bounded memory and disk (branch bounded-disk-memory, from main 3a09c49)

For a server on the monitored app's own box (the railwatch-server gem). A stress test of that
setup found anonymous RSS of 1.3-1.8 GB under dashboard load, 0.95 GB still held when idle, a
rollup accumulator that could reach ~3 GB, and disk bounded only by retention (time).

### Method

Tenant: `rw-load --spread-days 30` against main, 2.07M rows (1.18M queries, 291k logs, 292k
cache events, 146k view renders, 149k executions) in 62 day files, 1.7 GB; retention 90 days
and `DETAIL_RETENTION_DAYS=31` so nothing is pruned during a run. Load: 30 clients without
keep-alive cycling 18 URLs for 60 s (10 slow `/api/v1` endpoints at 7d/30d, and the overview,
requests, queries, `queries?q=widgets role:reading`, logs, logs FTS, cache and tenants pages
through the trusted-proxy header), server on CPUs 8-15, clients on 0-5. RSS from
/proc/PID/status, split by /proc/PID/smaps; per-connection SQLite memory from
`sqlite3_db_status`. **The release binary is static musl** (bin/release); a glibc build of the
same code holds much less (below), so memory was measured on musl builds made the way
bin/release makes them.

### What held the memory

1. **musl's malloc kept SQLite's freed memory.** Bundled SQLite allocates through the C
   library, not the Rust global allocator (mimalloc). On musl, 60 s after the load main held
   1.0-1.3 GB anonymous, 0.9-1.1 GB of it in small unnamed mappings (musl's heap), 0.1 GB in
   mimalloc's arena. The same code built for glibc held 0.3-0.37 GB. Now SQLite allocates
   through mimalloc too (`SQLITE_CONFIG_MALLOC`, memory.rs).
2. **mimalloc's purge delay.** This build's default is 1 s; anything freed and not reused
   within it stays resident. Measured alternating 0 / 10 / 100 / 1000 ms twice on 45 s runs:
   peak anonymous 323-357 / 408-421 / 483-495 / 690-713 MB, pages served 1371-1946 / 2514-2736
   / 1505-2593 / 2459-2507. 0 cost ~25% throughput; 10 ms (mimalloc v3's documented default)
   did not. Set to 10 ms at start unless `MIMALLOC_PURGE_DELAY` is set.
3. **Each pooled union reader held ~98 MB** after the 261 page cases (`sqlite3_db_status`):
   page cache 62 MB (4 MB per attached file x 62 files), prepared statements 31 MB (256 cached
   statements, each compiled over every file's branch of its views), schema 4.6 MB. Pooled
   readers per tenant: 4. Now the cache is one 4 MB budget split over the files (at least 64 KiB
   each) and a union reader keeps 64 statements: ~15 MB per reader. Reads go through mmap, so
   the cache barely matters: the 261 cases took 3.2-3.4 s at 64 KiB per file and at 4 MB.
4. **The rollup accumulator** keeps ~650 bytes of RSS per row (measured: 300 MB after 450k
   records at 7 batches/s on main) and capped at 3M rows (~2 GB). Now capped at 128 MB of
   counted row bytes (`incremental::MAX_BYTES`, ~200k rows); past it, the hours it holds leave
   it and are recomputed in full at each flush (RollupJob, same output) until they age out.

### Results (musl release builds, 2 runs each, 30 clients, 60 s)

| | main | branch |
|---|---:|---:|
| anonymous RSS at start | 13 MB | 44-46 MB |
| anonymous RSS peak under load | 1,311-1,509 MB | **379-384 MB** |
| anonymous RSS 60 s after | 1,005-1,267 MB | **260-273 MB** |
| file-backed (mmap of day files) peak | 4.65 GB | 4.66-4.68 GB |
| requests served in 60 s | 3,905-4,214 (46-100 were 429) | 3,400-3,426 (0 429) |

Requests served swung 2x between identical runs on this shared box, so throughput was rerun
alternating main / branch / branch with `MIMALLOC_PURGE_DELAY=1000`, 45 s each, twice:
main 1,271 and 2,433 (peak anonymous 864, 1,102 MB), branch 2,469 and 2,383 (403, 314 MB),
branch at 1 s purge 2,471 and 2,241 (657, 640 MB). No throughput cost seen; p50 355-650 /
366-376 / 368-384 ms.

File-backed RSS is the day files mapped read-only (mmap_size 16 GiB per file, as before): page
cache the kernel can drop at any time, not process memory. The "0.95 GB anon idle" in the
stress test was musl's heap, now gone. Targets: idle under 300 MB, met (260-273 MB); under 800
MB with 30 clients on the slowest pages, met (379-384 MB).

**Accumulator** (musl builds, fresh data dir, `rw-load` 2,000 batches = 1M records, all in
the current hour, 4 clients, ~13 min): main peaked at 1,081 MB anonymous and held 1,110 MB
idle 60 s later; the branch hit the cap once (316k rows, 134 MB counted), peaked at 244 MB and
held 69 MB idle. The cost: once an hour is past the cap, each 5 s flush recomputes that hour in
full (as main's 3M-row fallback did): 1,168 vs 1,284 ev/s in this saturated run (-9%, one run
each), p50 1.37 vs 1.23 s. A busy hour of a real app (CRM: ~1,000 rows per minute) reaches the
cap only well past ~200k rollup rows in two hours.

### Identity and page timings

`bench/storage/slow_pages.sh` on its 1.03M-row tenant (glibc builds, CPUs 4-7, 5 warm runs):
main vs branch byte-identical, 383/383 in both layouts, every round. Sum of per-case medians,
median of rounds: single 2,779 -> 2,762 ms (3 rounds), per-day 3,343 -> 3,243 ms (6 rounds,
alternating which binary ran first). No case over 10% and 2 ms slower after 6 per-day rounds
(after the first 3, five were, all in two rounds where the branch ran right after main; round
sums swing 3.1-4.6 s on this box).

### Disk

`max_data_bytes` and `min_free_bytes` (storage/budget.rs, docs/SELF-HOST.md "Disk budget"),
off by default. Prune also runs once at start. Tests: storage::budget (prune order across
environments, never today or meta, the floor's refusals counted and reported),
tests/ingest_http.rs (503 + Retry-After, nothing written, ping and 401 unaffected),
tests/disk_budget.rs (the real binary: retention catch-up at start; over the budget at start,
oldest detail days then core days, `raw_pruned_through` moved, logged, and a clean exit on
SIGTERM).

Under a burst: ingest adds what each commit wrote to a running estimate and wakes the pass as
soon as it crosses the budget (it used to wait up to 10 s). Repro from the resilience report
(N1): 200 MB tmpfs volume, `MAX_DATA_BYTES=60M MIN_FREE_BYTES=40M`, `rw-load --concurrency 2
--rate 9 --duration 120 --spread-days 10`, telemetry sampled every 0.5 s (budget 62.9 MB): peak 149
MB on main fcce07b, 61.5-63.3 MB with the estimate (3 runs). Followed by 150 s more into today's
files: main grew to 164 MB until the floor refused ingest at 41 MB free; with the estimate ingest
answered 503 over the budget (logged once) and the volume kept 141 MB free. Tests: storage::budget (the counter against the file
growth it stands for, a wake that trims within TRIM_WAIT, 503 over the budget with only today
left and back to 200 when within), tests/ingest_http.rs (that 503 over HTTP),
tests/disk_budget.rs (the real binary: four senders in a burst stay within the budget plus one
batch each, then 503, logged once), storage::reader (a day file deleted between listing and ATTACH
is skipped: trims now run during bursts, and the worker's re-attach failed on them).

## Rollup reads (branch perf/rollup-reads, from main 36cd877)

Production on 2026-10-25 (rebulk-system, v0.2.5): `/api/v1/.../queries?window=30d` 1.7-2.0 s,
`series?type=query&window=30d` 0.85 s, MCP `search_telemetry` queries `tenant:` at 7d 1.9 s
cold. A 30-day query window there is 696k rollup rows of 12k groups.

### Method

Tenant: `bench/diff/props/gen_tenant.py --spread-days 30 --executions 120000 --seed 5
--records-per-batch 2000 --shapes 12000 --tenants 300 --uniform --by-time --few-logs`, posted
to a fresh Rust server (per-day), rollups written by the server: 1.0M records, 660k queries,
441k query rollup rows of 12,000 groups at 30d (494k rollup rows in all), meta 165 MB, 1.2 GB
over 62 day files; the same rows exported to one 834 MB file. Cases: 296 page cases
(make_cases.py + slow_pages.py page-cases, every window preset), 496 `/api/v1` cases (every
endpoint at 1h/24h/7d/30d; queries under every sort, both directions, pages 1-3; requests, jobs
and spans under every sort; tenants sorted and paged; `tenant:` filters, one matching nothing,
a page past the end) and 32 MCP cases (list_tenants, search_telemetry queries plain, `tenant:`,
matching nothing, list_slow_routes, release_health, web_vitals at every window). props-dump,
api-dump and mcp-dump on CPUs 8-11, main and branch built from the same tree. Per statement:
SQLITE_TRACE_PROFILE and EXPLAIN QUERY PLAN.

### What was slow, and the changes

1. **Grouped tables built a `Rollup` per row** (`aggregations::Groups`). queries@30d read
   441k rows through `rollup::between` (`SELECT rollups.*`, 108 ms of SQLite) and then built
   each one: its name and group hash as Strings, the digest copied, the bucket parsed, the
   extra checked, then `group_by_hash` and `summarize` per group. Groups now folds the rows of
   the same WHERE as they come (sums, the digests as bytes, the latest bucket's name, the
   sparkline), from a column list without `extra` unless the page's summary needs it. The
   statement keeps `between`'s WHERE and plan, so the groups' order of first appearance, and
   with it `sort_by`'s ties, is unchanged. Not done: ranking in SQL (`GROUP BY group_hash`)
   was slower (367 ms: a temp B-tree over 441k rows), and reading the shown groups' digests in
   a second statement cost 80-100 ms (their rows are spread over the whole file).
   `summary_with_delta` and the other `summarize(between(...))` callers on these pages fold
   the same way (`rollup::summary_between`).
2. **Rollup series grouped every row into steps** (`aggregations::rollup_series`):
   `GROUP BY (strftime(bucket) / step)` sorted 441k rows in a temp B-tree (217 ms). It now
   groups by `bucket` first, which the index already returns in order (no sort), then the
   ~720 hours into steps. SUMs and MAXes of integer columns: the same integers.
3. **Slowest queries filtered by tenant, per-day** (`cursor_page::per_file`): every file
   walked `idx_queries_slowest` until it found 51 of the tenant's rows, most of the file for
   a small tenant. With `tenant:` each file now reads `idx_queries_tenant_duration_cursor`,
   the tenant's own (duration, id) range: 51 rows a file. Present in every day file (the
   exported DDL, partial `WHERE app_tenant IS NOT NULL` in files this server creates, which
   the `app_tenant = ?` term implies). Not an index added: every file already has it. The
   single file keeps `slowest_probe`.

| case (ms) | main cold | branch cold | main warm | branch warm |
|---|---:|---:|---:|---:|
| API queries@24h | 726 | 734 | 17.0 | 13.5 |
| API queries@7d | 774 | 764 | 98.3 | 71.3 |
| API queries@30d | 878 | 853 | 407.5 | 251.5 |
| API series.query@24h | 135 | 133 | 7.8 | 3.1 |
| API series.query@7d | 207 | 170 | 56.8 | 22.6 |
| API series.query@30d | 468 | 337 | 212.6 | 84.9 |
| MCP search queries tenant:transloadx@7d | 876 | 829 | 40.7 | 2.0 |
| MCP search queries tenant:transloadx@30d | 1043 | 840 | 208.1 | 3.7 |
| API summary@30d | 312 | 309 | 17.7 | 12.0 |
| API requests@30d | 311 | 308 | 12.2 | 7.4 |
| API tenants@30d | 1230 | 1234 | 184.3 | 186.7 |
| MCP list_tenants@30d | 1240 | 1226 | 185.5 | 185.4 |
| API visits@7d | 346 | 348 | 2.5 | 2.4 |

Per-day, main 36cd877, 3 rounds alternating binaries. Warm: median of 5 calls in one process
after one dropped. Cold: the tenant's files evicted from the page cache
(`posix_fadvise(DONTNEED)`) and the first call of a fresh process, so it includes opening and
attaching 63 files and reading what the page needs from disk (130 ms for the smallest pages).
Sums of per-case warm medians, main / branch: pages 6316 / 5272 ms per-day and 5007 / 3872
single; API 30047 / 22105 per-day and 28592 / 19934 single; MCP 1071 / 487 per-day. No case
over 15% and 3 ms slower except profiles@7d per-day (15.2 -> 18.9 ms; nothing here touches it).

Identical output: main == branch byte for byte for all 824 cases, per-day and single file,
and single == per-day. Unit tests compare the new paths with the old ones on random rollups
(`folded_groups_equal_the_row_by_row_tables`: 6 tenants x 3 record types x 5 windows x every
sort and direction x 3 limits, ties, renamed groups, extras, missing digests;
`rollup_series_by_hour_first_equals_grouping_every_row`;
`folded_totals_equal_summarize_in_any_order`). Each was checked by breaking the code it
covers: group order, latest name, avg rounding, dropped extras, sparkline index, an unsorted
percentile, the summary's digests, max, client errors, the series' MAX and client errors.

### Not changed (measured)

- **tenants@30d / list_tenants@30d, 193 ms warm.** Three per-file GROUP BYs over the window's
  95k tenant-tagged requests on `idx_executions_tenant_summary` (covering): 96, 39 and 36 ms,
  about 1 us a row, each file's part 1.5 ms. The rows are what the page counts. Fewer passes
  (one statement for the summary, last-seen and sparkline) would save perhaps a third and
  changes how three ported Ruby queries are shaped; not done here.
- **Cold reads.** A first call after the files leave the page cache pays for reading them:
  130 ms for the smallest pages here, 300-1200 ms for the 30-day ones. The branch reads the
  same pages of the same files (the rollups' rows are the same statement's), so cold time
  moves only where CPU was a large part of it (series@30d, the tenant-filtered search).
- **No index or schema change.** A covering index on rollups (record_type, group_hash, bucket,
  sums) made a SQL ranking 85 ms; it would be 38 MB on this 165 MB meta (built in 0.7 s) and
  need a migration path; the fold got most of that without it.
- Not measured against production data; the queries rollups in production have 1.6x the rows
  of this tenant at 30d.

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
