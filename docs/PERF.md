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

## Covering index on rollups (measured, not added; from main 669ec3b)

Production on 2026-10-26 (rebulk-system, v0.2.8): `/api/v1/.../queries?window=30d` ~1.1 s warm,
~700k query rollup rows of ~12k groups at 30d, meta 557 MB (rollups 410 MB). The question:
does a covering index for the window read (`WHERE record_type = ? AND bucket BETWEEN ? AND ?`,
on `index_rollups_on_record_type_and_bucket` today, every row then read from the table) make
queries@30d about 2x faster at an acceptable write and backup cost. It does not.

### Method

Tenant: `gen_tenant.py --spread-days 30 --executions 210000 --seed 5 --records-per-batch 2000
--shapes 12000 --tenants 300 --uniform --by-time --few-logs`, posted to a fresh per-day server
and settled: 1.76M records, 792k query rollup rows of 12,000 groups at 30d (185k at 7d, 26k at
24h), 856k rollup rows in all, meta 273 MB after VACUUM (rollups 183 MB, the two existing
indexes 65 and 32 MB). That is production's rollup scale (above it in rows, at half its meta
size: production's rows carry longer names and digests). Each variant is the same meta with one
index added (`CREATE INDEX`, then VACUUM); the binary is main's.

| shape | columns after (record_type, bucket) | size | build |
|---|---|---:|---:|
| A | id, group_hash, name, count, error_count, client_error_count, duration_sum, duration_max, digest (`Groups::between` without extra) | 173 MB | 1.9 s |
| B | count, error_count, client_error_count, duration_sum, p50, p95, p99 (`rollup_series`) | 43 MB | 1.1 s |
| C | A plus p50, p95, p99 (both) | 182 MB | 1.9 s |

`id` follows bucket in A and C so rows within a bucket come in the same order as the existing
index's (rowid), which the groups' first-appearance order and `sort_by`'s ties depend on. SQLite
chose each index with no sqlite_stat1 (nothing was ANALYZEd). 30 `/api/v1` cases (queries under
two sorts, series of queries, requests and one query group, summary, requests, jobs, spans, one
route) and 9 MCP cases (search_telemetry queries, list_slow_routes, release_health) at
24h/7d/30d, api-dump/mcp-dump on CPUs 8-11, 3 rounds of the 4 variants in turn. Warm: median of
5 calls after one dropped, median of rounds. Cold: the tenant's files evicted
(`posix_fadvise(DONTNEED)`), a fresh process, one call.

### Results (ms)

| case | main warm | A | B | C | main cold | A | B | C |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| API queries@24h | 21.1 | 20.7 | 26.2 | 21.2 | 2667* | 1037 | 1033 | 1182 |
| API queries@7d | 122.4 | 110.5 | 152.5 | 114.5 | 1151 | 1088 | 1138 | 1123 |
| API queries@30d | 456.7 | 433.3 | 603.7 | 462.2 | 1471 | 1487 | 1530 | 1541 |
| API queries sort=p99@24h | 21.2 | 20.7 | 27.5 | 21.9 | 1344 | 1017 | 1009 | 1016 |
| API queries sort=p99@30d | 484.5 | 465.6 | 595.0 | 468.5 | 1450 | 1436 | 1544 | 1485 |
| API series.query@7d | 36.0 | 35.2 | 27.3 | 32.5 | 403 | 412 | 393 | 409 |
| API series.query@30d | 148.4 | 151.5 | 115.1 | 137.1 | 552 | 560 | 493 | 564 |
| API series.query (one group)@30d | 1.3 | 1.3 | 1.5 | 61.5 | 465 | 466 | 476 | 472 |
| API series.request@30d | 5.6 | 5.9 | 3.8 | 4.0 | 466 | 467 | 371 | 377 |
| API summary@30d | 18.0 | 17.6 | 21.5 | 18.1 | 473 | 474 | 483 | 477 |
| API requests@30d | 12.2 | 10.0 | 14.1 | 10.8 | 467 | 378 | 473 | 393 |
| API jobs@30d | 15.7 | 14.0 | 15.0 | 14.3 | 961 | 860 | 964 | 878 |
| API route@30d | 50.0 | 49.6 | 49.6 | 49.5 | 1020 | 1014 | 1016 | 1021 |
| MCP search_telemetry queries@30d | 6.1 | 4.9 | 4.8 | 5.6 | 1491 | 1495 | 1480 | 1515 |
| MCP list_slow_routes@30d | 9.3 | 8.1 | 10.1 | 7.0 | 466 | 472 | 474 | 372 |

\* main's three cold runs were 2675, 2667 and 1269 ms, the others' 1000-1250: the first cold case
after the meta copy, on main only; not explained, and gone at 7d and 30d. Cases not shown were
under 2 ms warm on all four, or within a few ms.

Output: A and C byte-identical to main in every case and round. B is not: it returns the window's
rows in (bucket, count, ...) order, not rowid order, so the groups' first-appearance order and
with it `sort_by`'s ties change (queries, requests and jobs at 24h and 7d reorder equal rows). B
would need the read to sort, which is what #51 removed.

### Why it does not help

- **The table lookup is not the cost.** The window statement takes ~490 ms of queries@30d
  (`RW_SQL_PROFILE`; everything else on the page is 12 ms). The same rows, aggregated inside
  SQLite with no rows returned, take 190 ms on main and 163 ms with A: the index saves the
  27 ms of rowid lookups (rows of an hour sit together in the table, so the lookups are nearly
  sequential). The other ~300 ms is stepping 792k rows out to Rust (`sqlite3VdbeExec` 33% of
  the profile, column reads and type checks, the fold in `Groups::between` 12%), which a
  covering index does not change. Net: 457 -> 433 ms, 5%.
- **B slows the grouped tables down** (457 -> 604 ms at 30d, 122 -> 153 at 7d): SQLite picks B
  for the grouped read too (it covers the WHERE, not the columns), then looks every row up in
  the table in B's order, which is no longer the table's.
- **C regresses one-group series**, 1.3 -> 61.5 ms at 30d: with no statistics the planner takes
  the covering index's (record_type, bucket) range over the unique index's (record_type,
  group_hash, bucket) for `... AND group_hash = ?4`. Fixing that takes an `INDEXED BY` or stats,
  and the code must not depend on sqlite_stat1.
- **Cold reads are reading the file.** 300-500 ms of every cold case is opening and attaching
  the per-day files; the 30-day query cases are ~1.5 s with or without the index (A reads a
  smaller index instead of the table, but its index is as large as the columns it covers).

### Costs

- **Size.** A is 94% of the rollups table (name and digest are most of a row), so it would be
  ~385 MB on production's 557 MB meta (410 MB of rollups): the meta goes to ~940 MB. B is 23%
  (~95 MB there).
- **Build.** 1.9 s for A here (1.1 s for B, this machine's NVMe disk); an estimated 4-5 s for
  A at production's size (not measured there), holding the meta's write lock: rollup flushes
  and the ingest receipts in meta wait for it.
- **Writes.** `write_type`'s statements replayed for the 24 latest query hours (1,092 rows an
  hour: DELETE of the hour's groups, 500-row INSERTs, IMMEDIATE, server pragmas; CPUs 4-7,
  3 x 6 rounds): the medians are bimodal by run (55-65 ms or 140-150 ms per flush, the same
  for main and every shape, whichever checkpoint the run hit), so no difference in flush time
  could be measured. WAL written per flush: main 4.97 MB, A 5.27 MB (+6%), B 5.05 MB (+2%),
  C 5.27 MB.
- **Backups.** The meta snapshot (zstd 3, uploaded at most hourly): main 45.0 MB, A 75.1 MB
  (+67%), B 52.2 MB (+16%), C 77.6 MB. Production's meta compresses to ~116 MB
  (storage/backup.rs); the same ratio would put A's uploads near 190 MB an hour.

### What would

The cost is the number of rows the window returns. 30 days of query rollups are 792k
group-hours here but 311k group-days: reading days for the days a window covers fully (a daily
rollup, or folding hours into days when they age) would return 2.5x fewer rows. It changes the
stored digests (a merge of hourly t-digests is not byte-identical to one over the raw rows) and
needs its own plan; not done here.

## Security filters (branch security-agent, from main 32ee2be)

What the gem records of a request beyond the executions columns (path, ip,
user agent, headers, rate_limited, halted_callback, unpermitted_parameters)
is in `executions.detail` JSON, with no index. `ip:`, `ua:`, `path:`,
`probe:`, `rate_limited:`, `halted:`, `unpermitted:`, every
`query_requests` call and `security_summary` read the window's requests
row by row (`json_extract`), so they cost the window's request count, not
the matches.

### Method

Tenant: `gen_tenant.py --spread-days 30 --executions 400000 --seed 5
--records-per-batch 2000 --shapes 2000 --tenants 50 --uniform --by-time
--few-logs --security` (the new `--security` flag gives requests ip, path,
user agent, headers and flags, with ~2% planted scanner and brute-force
traffic), posted to a fresh per-day server: 3.35M records, 320k requests
(10.7k a day; rebulk-system production does ~20k), 3.2 GB over 93 files.
mcp-dump and api-dump on CPUs 8-11, release build, a token with the
security scope. Warm: the median of 5 calls after one dropped, median of
2 rounds. Cold: the tenant's files evicted (`posix_fadvise(DONTNEED)`), a
fresh process, one call.

### Results (ms, warm)

| case | 1h | 24h | 7d | 30d |
|---|---:|---:|---:|---:|
| security_summary | 3.7 | 70.8 | 509.3 | 347.4 (capped at 100k) |
| query_requests ip by 401 | 0.5 | 3.5 | 25.5 | 112.0 |
| query_requests ip x path, `path:../\|.env` | 1.6 | 12.3 | 79.5 | 343.9 |
| query_requests user_agent, `path:/admin*` | 1.4 | 11.2 | 66.6 | 286.1 |
| query_requests user, `status:403` | 0.5 | 3.4 | 23.2 | 97.7 |
| query_requests minute, `status:401 route:session` | 48.9 | 62.5 | 32.1 | 128.1 |
| query_requests ip by distinct_users, `status:4xx` | 0.7 | 8.4 | 56.8 | 249.6 |
| query_requests ip, every metric, no filter | 2.2 | 54.5 | 417.9 | 1155.6 (capped at 200k) |
| search_requests `ip:` | 0.7 | 9.8 | 65.8 | 189.7 |
| search_requests `probe:true status:2xx` (no match) | 3.5 | 29.8 | 192.9 | 816.5 |
| search_requests `ua:curl` | 1.5 | 12.2 | 18.7 | 28.7 |
| search_requests (no filter, as before) | 0.9 | 0.9 | 0.9 | 1.1 |
| search_telemetry exceptions `ip:` | 1.2 | 10.7 | 65.6 | 275.2 |
| API requests `ip:` | | 9.8 | 65.9 | 283.8 |
| API requests `probe:true` | | 37.3 | 247.0 | 1055.6 |
| API exceptions `ip:` | | 11.7 | 68.3 | 283.2 |

Cold at 7d: security_summary 1518, query_requests every metric 1221,
exceptions `ip:` 1155, probe search 938, ip x path 836; summary@24h 845.

- A filter is as fast as the requests it has to read: `ua:curl` at 30d is
  29 ms because the scanner's rows are near the top (newest first, LIMIT 50),
  and a filter that matches nothing reads the whole window. `status:`,
  `method:` and `route:` narrow on columns first.
- security_summary@7d reads two weeks (the window and the one before):
  ~150k requests, 509 ms. At 30d the window alone passes its 100k cap, so it
  reads the newest 100k, compares nothing and says so (`scan`), which is why
  30d is faster than 7d. Superseded: the summary has no such cap now, see
  "Complete security_summary" below.
- `query_requests` stops at 200k requests (the 30d no-filter case, 1.16 s;
  `scan.capped`). The 30d API `probe:true` table (1.06 s) is the other case
  over 1 s: it ORs 20 LIKEs over each of 320k paths.
- **Records of matching requests** (exceptions, logs, queries, jobs by
  `ip:`, ...): the request ids are read first and bound as one JSON array
  (`IN (SELECT value FROM json_each(?))`), at most 20,000 (`capped`, and a
  note in the result). As a correlated `IN (SELECT ... FROM executions)` the
  per-day views ran the subquery once per day file: exceptions by `ip:` at 30d
  took 8306 ms, now 275.
- **No index added.** Superseded: see "Security reads" below.
- Not measured against production data. Production requests carry more
  headers (bigger `detail`), so `json_extract` per row costs a little more.

## Security reads (branch security-speed, from main 0d69f42)

Production (rebulk-system, v0.2.12) was slower than the bench above:
security_summary 1.9 s at 24h and 3.4 s at 7d, `query_requests` probe paths
at 7d 13.8 s on the first call. It sends ~2x the bench's requests a day (60%
one IoT ingest route), with bigger `detail` (browser headers).

### What changed

One partial covering index on `executions` (`idx_executions_requests`,
`WHERE kind = 'request'`, docs/DIFFERENCES.md #14): `occurred_at` first, the
columns a request read returns, and the request fields as index expressions
(`json_extract(detail, '$.ip')`, the user agent, path, Cf-Ipcountry,
`json_type` of rate_limited and its name, the halted callback, the
unpermitted parameters and their count, the redirect, the response size).
`filter_query::requests` (with a request key), the child-record id lookup and
`request_scan::scan` name it with `INDEXED BY`. SQLite then reads every
field from the index and never the row's `detail` (`a_request_scan_reads_no_detail_where_the_index_exists`
checks the bytecode; an expression written differently from the index's
would quietly parse the JSON again). The kind filter is a literal (`kind =
'request'`): a bound kind cannot use a partial index.

Not done, and why:

- **Promoted columns.** Real `ip`/`path` columns would need `ALTER TABLE` on
  every file and a backfill rewriting every request row; the index gets the
  same reads with no row rewritten, and Rails' schema stays intact.
- **An `(ip, occurred_at)` index.** `ip:` was 130 ms at 7d with the
  covering index's range read alone 11 ms; a second index is another B-tree
  per request insert for little.
- **A probe flag at ingest.** The 20 LIKEs over the path, read from the
  index, cost ~1 µs a request; the scan of the window is what cost.
- **An hourly security rollup.** The covering index brought the common
  shapes under 0.5 s at 30d; a rollup adds a write path, a backfill and a
  second answer to keep equal, for the remaining 2x on unfiltered scans.

### Method

Tenant: `gen_tenant.py --spread-days 30 --executions 830000 --seed 5
--records-per-batch 2000 --shapes 2000 --tenants 50 --uniform --by-time
--few-logs --prod-shape` (`--prod-shape`: 60% of requests are one hot POST
ingest route with one query each, probes split between `unmatched` 404s and
a catch-all `GET /:org_slug` that sends them 302 to sign-in, browser-sized
headers), posted to a fresh per-day server: 4.4M records, 22k requests a day
(rebulk-system: ~32k on 2026-10-27), 4.4 GB over 93 files, core day files
~54 MB. The same data before (main 0d69f42) and after: the after copy was
migrated by the server's own startup pass, then the two newest days (which
that pass leaves alone) indexed too, to measure the steady state. mcp-dump on
CPUs 8-11, release build, a security-scope token. Warm: median of 5 after one
dropped. Cold: files evicted (`posix_fadvise(DONTNEED)`), a fresh process,
one call. The box was loaded (other work on it), so cold numbers moved by up
to 2x run to run: the summary and `ip:` exceptions cases, which first came
out slower after, were rerun twice interleaved with before, and the second
round is shown.

### Results (ms, warm, before -> after)

| case | 24h | 7d | 30d |
|---|---:|---:|---:|
| security_summary | 154 -> 67 | 332 -> 158 | 340 -> 186 |
| ip by 401 | 8.4 -> 2.2 | 55 -> 12 | 232 -> 53 |
| ip x path, `path:../\|.env` | 24 -> 4.0 | 163 -> 20 | 698 -> 76 |
| user_agent, `path:/admin*` | 21 -> 3.2 | 135 -> 15 | 590 -> 57 |
| user, `status:403` | 8.2 -> 2.0 | 51 -> 11 | 218 -> 45 |
| minute, `status:401 route:session` | 58 -> 74 | 65 -> 20 | 287 -> 76 |
| ip by distinct_users, `status:4xx` | 12 -> 3.9 | 80 -> 33 | 369 -> 106 |
| path, `probe:true` | 70 -> 17 | 484 -> 101 | 2011 -> 406 |
| path x status, `probe:true` | 73 -> 18 | 472 -> 98 | 2033 -> 414 |
| ip, 7 metrics, no filter | 92 -> 53 | 621 -> 365 | 841 -> 498 (capped at 200k) |
| search_requests `probe:true status:2xx` | 65 -> 16 | 430 -> 91 | 1843 -> 379 |
| search_requests `ip:` | 19 -> 2.1 | 130 -> 11 | 400 -> 32 |
| search_telemetry exceptions `ip:` | 21 -> 4.7 | 134 -> 23 | 556 -> 98 |
| search_logs `path:/session` | 35 -> 15 | 220 -> 92 | 1107 -> 414 |

Between the upgrade and the second day after it, today's and yesterday's
files have no index: 24h is unchanged from before (summary 156 ms), 7d is
between (summary 256, probe paths 202), 30d mostly the index's (probe paths
506, `ip:` search 66).

Cold, 7d: probe paths 1416 -> 529, ip x path 1142 -> 425, every metric 1557
-> 850, `ip:` exceptions 1906 -> 1546, summary 1793 -> 1687. 30d: probe paths
4314 -> 902, `probe:true status:2xx` 4222 -> 986, ip by 401 2779 -> 555,
summary 2670 -> 2484, exceptions `ip:` 3470 -> 1931. The cold summary barely
moves: it reads 100k requests per window either way, and cold that is the
pages, not the JSON.

- `minute, status:401 route:session` at 24h is 58 -> 74: `route:` brings its
  own plan (`named_like`, group hash), so it does not use the index, and the
  per-row work grew (more fields read). It is a case where the window is one
  day file and the route already narrows.
- What stays slow is what reads most of the window: the summary (up to 100k
  requests per window) and an unfiltered `query_requests` (up to 200k).
  ~2.5 µs a request warm.
- search_logs `path:` at 30d (414 ms) is mostly the logs, not the requests:
  the request ids are read from the index in ~30 ms and bound as a list.

### Migration cost

The startup pass on the bench tenant: 29 closed core day files (54 MB each)
in 19.3 s of building (pauses excluded), +9% of their size. A 500 MB core
day file (10 bench days, 222k requests) built in 4.3 s cold, 2.3 s warm.
Each build is one write transaction on that day file. Ingest writes a closed
day only for a late record (accepted up to 30 days old), which then waits
for the build inside busy_timeout (5 s), or is answered 503 and retried by
the gem past it. The build goes through the WAL (about 4x the index's size),
which is checkpointed and truncated right after; a checkpoint a reader keeps
busy is tried again on the next pass (hourly while any day lacks the index,
which also builds the days that were today and yesterday at the upgrade).

Ingest with the index, the same 400 batches into a fresh server, twice:
wall 137.0 -> 140.9 s and 138.5 -> 144.4 s, server CPU +2.3% and +2.7%.

## Complete security_summary (from main 2f5cf8f)

security_summary folded one scan of the window capped at the newest 100k
requests. rebulk-system (v0.2.13, 2026-10-28) sends ~300k requests a week,
60% one ingest route: the 7d summary covered ~2.3 days, reported 355 probe
requests where `query_requests q=probe:true` counted 4,105, and skipped the
previous window. The cap was said only in a top-level `scan` note.

### What changed

Each finding reads only the requests it is about, on the request index:

- the 4xx (auth failures: 401/403, and 422 on sign-in routes; scanner
  candidates: each address's 4xx and their paths), one read for both;
- `probe:true`;
- rate-limited, halted or unpermitted, ORed in one read.

Those are a small part of any window (on the bench below ~1,800 4xx,
~180 probes and ~120 halted a day against 43k requests), so they fold
whole. Each read still stops at 200,000 matching requests; a finding whose
read did says so in its own `summary`, its count is a floor, and it is not
graded on the comparison. The top-level `scan` note is gone.

The rest is SQL, per day file (`storage::reader::aggregate_hinted`, which
now leaves an added index's hint off a file without it, as the hinted
views do):

- the window's total and status classes: `SUM` columns (a `GROUP BY` sorts
  every row);
- every address's requests and 4xx, for `new_ips`: each file sorts its rows
  by address and then groups them. A plain `GROUP BY` over ~16k addresses a
  day built its temp B-tree row by row: 78 ms a day file against 16 ms
  sorted first. The files' rows are added up in Rust (grouping them again
  in SQL was slower: 7d 1.04 -> 1.20 s);
- the shown scanners' (top 10) requests and served counts: one read of
  their own requests (`ip IN (json_each(?))` on the index).

Both windows are always read and compared.

### Method

Tenant: `gen_tenant.py --spread-days 29.5 --executions 1590000 --seed 5
--records-per-batch 2000 --shapes 2000 --tenants 50 --uniform --by-time
--few-logs --prod-shape`, posted to a fresh per-day server (main 2f5cf8f)
and left to build the request index on every day: 43k requests a day
(rebulk-system ~43k), 60% `POST /api/v1/ingest/scale_live`, 5.5 GB over 31
core days. mcp-dump on CPUs 8-11, release build, a security-scope token
(and one without it). Warm: median of 5 after one dropped, two rounds.
Cold: files evicted (`posix_fadvise(DONTNEED)`), a fresh process, one
call, two rounds interleaved. The 30d window's previous window is empty
(the tenant holds 29.5 days).

### Results (ms)

| | 24h | 7d | 30d |
|---|---:|---:|---:|
| warm, before (7d and 30d: newest 100k, not compared) | 127 | 166 | 188 |
| warm, after | 146-170 | 1040-1160 | 2200-2470 |
| warm, after, no security scope | 97 | 662 | 1394 |
| cold, before | 1281-2872 | 2791-3078 | 4705-4860 |
| cold, after | 1126-1347 | 3859-3990 | 7623-7722 |

Counts at 7d, before -> after, against `query_requests` on the same
data: probe requests 412 -> 1,244 (query_requests 1,244), halted 272 ->
820 (820), requests 100,000 -> 301,707, previous window not read ->
302,620 requests and 1,228 probes. 24h answers are byte-identical
before and after.

Where 7d warm goes (both windows): probe paths ~360 ms (20 LIKEs over each
of ~600k paths, read from the index, ~0.6 µs a request), the address read
for `new_ips` ~400 ms (95k addresses at 7d: the bench draws client addresses
from 260k; production's count is unmeasured, as the token reading it has no
security scope), the 4xx and flag reads ~90 ms
each, the totals and exceptions the rest.

- 7d and 30d are over 1 s and 2 s with the security scope. Without it (no
  address findings) they are 0.66 and 1.39 s. Capping `new_ips` alone
  (allowed, if its summary said "of the newest N") would save most of the
  difference; not done, as the counts it gives are exact now.
- **No index added.** A probe flag (or a probe index expression) would
  take the 20 LIKEs off every request; "Security reads" above weighed it
  against a write path and kept the LIKEs.

## Compact agent output (from main 4bcf959)

Security reads were mostly repeats: on rebulk-system, `search_requests
status:401 route:/api/mcp` over 24h (read live on 2026-10-28) was 59 KB for
113 rows, the 60 KB cap cutting 87 more, all one kind of request. The replay
below, of that day's 338 such requests, shows 119 rows and cuts 81. Grouping folds alike
rows into counted entries (docs/security-agent.md, "Grouped results").

### Method

Bytes: two tenants. **Replay**: the requests and warn log lines production
returned for the baseline reads (rebulk-system, 2026-10-28 04:30Z; 5,479
records: every `status:401 route:/api/mcp` and `status:4xx` request of 24h,
every `probe:true` request of 7d, the 73 warn lines), paged out of the
production MCP read-only and posted to a fresh server; read at the same
instant without the security scope, as the baseline was. **Bench**:
`gen_tenant.py --spread-days 8 --executions 220000 --seed 5
--records-per-batch 2000 --shapes 2000 --tenants 50 --uniform --by-time
--few-logs --prod-shape`, 1.2 GB over 9 day files, ~22k requests a day,
security scope. mcp-dump, release build, CPUs 8-11, the main build and this
one on the same data; warm is the median of 5 after one dropped.

### Bytes

Replay (production rows):

| read | before | after | |
|---|---:|---:|---:|
| `search_requests` `status:401 route:/api/mcp`, 24h | 58,850 | 1,144 | 51.4x |
| `search_requests` `status:4xx`, 24h | 58,798 | 18,979 | 3.1x |
| `search_logs` level warn, 24h | 40,381 | 4,069 | 9.9x |
| `query_requests` path x outcome, `probe:true`, 7d | 59,052 | 8,798 | 6.7x |
| `security_summary`, 7d (unchanged) | 3,986 | 3,986 | 1.0x |
| the 401 read with `shape: rows` | 58,850 | 58,850 | 1.0x |
| `search_requests` `probe:true`, 24h | 18,976 | 1,897 | 10.0x |

Bench:

| case | before | after | |
|---|---:|---:|---:|
| r401@24h | 52,822 | 2,202 | 24.0x |
| r4xx@24h | 58,705 | 51,370 | 1.1x |
| rprobe@24h | 47,725 | 3,887 | 12.3x |
| rlatest@24h | 29,834 | 19,161 | 1.6x |
| logs@24h | 58,890 | 1,383 | 42.6x |
| logs_path@24h | 20,959 | 3,020 | 6.9x |
| exc_ip@24h | 67 | 435 | 0.2x |
| qr_probe@24h | 3,082 | 3,082 | 1.0x |
| qr_ip401@24h | 1,738 | 1,738 | 1.0x |
| ip_activity@24h | 1,370 | 1,644 | 0.8x |
| summary@24h | 5,156 | 5,156 | 1.0x |
| r401@7d | 58,796 | 5,026 | 11.7x |
| r4xx@7d | 58,705 | 50,465 | 1.2x |
| rprobe@7d | 58,736 | 9,641 | 6.1x |
| rlatest@7d | 29,834 | 19,161 | 1.6x |
| logs@7d | 58,890 | 1,628 | 36.2x |
| logs_path@7d | 20,959 | 3,865 | 5.4x |
| exc_ip@7d | 1,173 | 1,212 | 1.0x |
| qr_probe@7d | 3,996 | 3,996 | 1.0x |
| qr_ip401@7d | 1,754 | 1,754 | 1.0x |
| ip_activity@7d | 5,363 | 6,116 | 0.9x |
| summary@7d | 5,357 | 5,357 | 1.0x |
| last_execution | 4,648 | 4,648 | 1.0x |

Before is cut at the 60 KB cap in every read over ~200 rows (the 401 read
lost 81 rows to it). After, nothing is cut: entries past the ~52 KB budget
are counted in `more` (bench r4xx: 35 entries, 162 requests at 24h). The
bench's `status:4xx` barely shrinks: the generator gives each request a
random user, address and tenant, so `varying` stays long (each entry's ten
commonest values with counts); the same read over production rows is 3.1x
smaller. `exc_ip@24h` grows (67 -> 435 B) for an empty result: the
`how_to_read` text.

### Time

Grouping reads every request `q` matches, where the rows read one page. On
the bench, warm:

| case | before | after |
|---|---:|---:|
| r401@24h | 8.0 | 1.7 |
| r4xx@24h | 5.6 | 4.7 |
| rprobe@24h | 14.8 | 14.2 |
| rlatest@24h | 0.7 | 0.7 |
| logs@24h | 2.8 | 6.2 |
| logs_path@24h | 8.9 | 8.8 |
| exc_ip@24h | 2.7 | 3.2 |
| qr_probe@24h | 13.6 | 14.0 |
| qr_ip401@24h | 1.3 | 1.2 |
| ip_activity@24h | 1.4 | 1.4 |
| summary@24h | 73.0 | 74.5 |
| r401@7d | 22.7 | 13.4 |
| r4xx@7d | 6.0 | 29.0 |
| rprobe@7d | 39.5 | 98.3 |
| rlatest@7d | 0.7 | 0.8 |
| logs@7d | 3.9 | 50.6 |
| logs_path@7d | 63.9 | 63.8 |
| exc_ip@7d | 25.3 | 25.4 |
| qr_probe@7d | 91.9 | 98.3 |
| qr_ip401@7d | 13.1 | 13.1 |
| ip_activity@7d | 12.2 | 12.1 |
| summary@7d | 303.9 | 313.9 |
| last_execution | 1.6 | 1.4 |

- The grouped reads cost what `query_requests` costs over the same `q`
  (r4xx@7d grouped 29 ms, `query_requests group_by route` 20 ms;
  probe:true@7d 100 vs 95 ms): the scan of the window, not the fold.
- A grouped log read folds up to 20,000 lines (logs@7d: 20,000 info lines,
  51 ms, against 4 ms for a page of 200).
- The other tools are unchanged; get_execution and last_execution are not
  touched.

## Session journeys (from main 1d5ce91)

A session journey (docs/sessions.md) reads one browser session's requests
and visits, across every day file. Without an index that is a scan of every
request of every day for a `json_extract(detail, '$.session_id')` match.

### What changed

Two partial indexes (docs/DIFFERENCES.md #15): `idx_executions_sessions`
on `(json_extract(detail, '$.session_id'), occurred_at, json_extract(detail,
'$.tab_id'), status, user_ref) WHERE kind = 'request' AND json_extract(detail,
'$.session_id') IS NOT NULL`, and `idx_visits_sessions` on `(session_id,
occurred_at) WHERE session_id IS NOT NULL`. A journey, `session:` in the
filter grammar, and the child-record lookup name them with `INDEXED BY`; the
per-day reader leaves the hint off a day file without them.
`a_journey_reads_through_the_session_indexes` asserts the plans (`SEARCH ...
USING INDEX idx_executions_sessions (<expr>=?)`, no temp B-tree for the
ORDER BY).

### Method

Tenant: `gen_tenant.py --spread-days 8 --executions 220000 --seed 5
--records-per-batch 2000 --shapes 2000 --tenants 50 --uniform --by-time
--few-logs --prod-shape --sessions`, posted to a fresh per-day server: 1.2
GB over 9 core day files, ~22k requests a day, ~8.7k of them carrying a
session (the hot ingest route, jobs and probes carry none). `--sessions`:
one day-long session (a dashboard left open: 2,153 requests) and ~290
others a day of ~20 minutes (median 23 requests, two thirds with more than
one tab). mcp-dump, release build, CPUs 8-11, security-scope token, warm
median of 5. "Without" is the same data with the two indexes dropped from
every day file: the state before the background build.

### Results (ms, warm)

| case | without the indexes | with |
|---|---:|---:|
| session_journey, typical session (23 requests) | 157.1 | 0.6 |
| session_journey by execution_id | 158.0 | 0.7 |
| session_journey, day-long session (2,153 requests) | 193.1 | 31.3 |
| same, shape rows | 182.6 | 24.8 |
| search_requests `session:<id>`, 30d | 157.3 | 0.4 |
| list_sessions 24h | 32.0 | 12.1 |
| list_sessions 7d, has_errors | 223.3 | 87.1 |
| list_sessions 7d, one user | 2.0 | 2.0 |

Every result was byte-identical with and without the indexes. The
day-long session's time is building and folding 2.2k events (and
`max_tokens`/the cap keeping the newest 116 entries), not reading them.
`list_sessions` by user reads `idx_executions_people`, already there.

### Costs

- Size: `idx_executions_sessions` 6.75 MB and `idx_visits_sessions` 0.2 MB
  over the 9 core files (485.3 MB against main's 472.2 MB, +2.8%; the
  request index is 48.7 MB). Rows without a session are not in them.
- Ingest: 593 batches posted to each build, the same machine and CPUs:
  10,538 CPU ticks on main, 10,401 on this branch (201 s and 206 s wall),
  within run-to-run noise.

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
