# Per-day tenant storage (branch `storage`, 2026-10-14)

Code: `crates/server/src/storage/`. Decided 2026-10-14;
reasoning for the layout is research/IMPROVEMENTS.md 1(iv).

## Layout

```
data/telemetry/<slug>/meta.sqlite3              rollups, release_health, ingest_receipts, people,
                                                query_shapes, export_destinations, export_deliveries
data/telemetry/<slug>/core/YYYY-MM-DD.sqlite3   executions, exceptions, sessions, health_samples,
                                                processes, ingest_batches, profiles, every other row table
data/telemetry/<slug>/detail/YYYY-MM-DD.sqlite3 queries, cache_events, view_renders, logs (+ logs_fts), spans
```

Every file is a full gem-schema tenant file (same DDL, same
schema_migrations, the two partial `queries` indexes of DIFFERENCES #11), so
any one of them opens in Rails. A row's day is its `occurred_at`
(`sampled_at`, `booted_at`, `received_at`) in UTC. New environments are
per-day; an existing single file keeps working unchanged until
`railwatch split <slug>` (or `--all`). `railwatch export <slug> out.sqlite3`
merges a directory back into one Rails file.

**Ids are environment-wide.** Each day table's AUTOINCREMENT high-water mark
lives in meta's own `sqlite_sequence`. Before inserting a run of rows into a
day file the writer sets that file's `sqlite_sequence` to the mark, and
reads it back after, so ids continue exactly as in one file (an ignored
insert burns an id in both). Consequences: `export` copies rows with their
ids, `executions.profile_id` stays valid across files, and the union reader
never sees two rows with one id.

**Reads.** `storage::reader` opens an in-memory main database, ATTACHes
meta and every day file read-only, and creates one TEMP view per gem table
(`UNION ALL` over the files that can hold it; detail tables also read the
core files, which hold exemplars). `logs_fts` is a view with `rowid`, the
hidden column and `message`, so `MATCH`, `rowid IN (...)` and `snippet()`
work. `INDEXED BY` / `NOT INDEXED` go to hinted views with the hint on
every branch (docs/PERF.md #5). SQLite does not push ORDER BY ... LIMIT,
GROUP BY or COUNT into a view's branches, so the statements that were slow
over the union are written per file and combined: cursor pages
(`cursor_page::per_file`, each file's top N, FTS paired to the file's own
index) and the aggregates (`reader::aggregate`); docs/PERF.md, "Slow
dashboard pages". Readers re-attach when a day file appears or is deleted (checked on
pool checkout). `SQLITE_MAX_ATTACHED` is raised to 125
(`.cargo/config.toml`): 90 days x 2 tiers would not fit, but detail is 7
days, so 30-day retention is 1 + 30 + 7 = 38 files, 90 days 98.

## Exactly-once (decision 3)

There is no atomic commit across WAL files, so the commit point is a single
file: **meta commits last, and its marks say which day-file rows exist.**

1. `BEGIN IMMEDIATE` on meta. Every writer of the environment takes meta
   first; holding it means nothing else writes any of its files.
2. Per run of rows: `BEGIN IMMEDIATE` on the day file (first touch), set
   its `sqlite_sequence`, insert, read the sequence back as the new
   in-flight mark. Receipts, people and query shapes go to meta.
3. `COMMIT` every day file, then write the marks into meta's
   `sqlite_sequence` and `COMMIT` meta.
4. **Recovery** (`DayWriter::recover`, at writer open and after a partial
   commit): any day-file row with `id >` meta's mark for its table belongs
   to a transaction whose meta commit never happened. It is deleted (FTS
   entries first), and profile links pointing past the profiles mark are
   cleared.

So a crash before meta's COMMIT leaves nothing once recovery has run (no
receipt exists, no 200 was sent; the gem's retry writes the batch once). A
crash after it leaves everything (a replay of a receipted delivery answers
`already_committed`; a direct-path retry is at-least-once, as in Rails).
I chose this over an ATTACH transaction with a rollback journal because
that would take every day file out of WAL mode (readers would block the
writer), and over ATTACH in WAL mode because SQLite documents that a WAL
multi-database transaction is atomic per file, not across files.

Power loss under `synchronous=NORMAL` can drop any one file's WAL tail.
Meta's tail lost with a day file intact leaves rows above the marks, which
recovery removes. A day file's tail lost with meta intact loses rows the
receipt acknowledged, which is the same acked-batch loss a single Rails file
has under NORMAL (DIFFERENCES #13).

**Crash test** (`tests/storage_crash.rs`): the real binary, a receipted
batch of 40 requests x (1 request + 3 queries + 2 logs) alternating between
23:59 yesterday and 00:00:30 today, so one transaction writes four day files
plus meta, after an earlier committed batch of 20. `RAILWATCH_CRASH_AT`
aborts the process at `mid_transaction:3`, `after_day_commit:1`,
`after_day_commit:3`, `before_meta_commit:1`, `after_meta_commit:1`. Then
restart, the same delivery retried until 200, and once more. Result at every
point: executions 60, queries 180, logs 120, FTS hits 120, receipts 2,
ingest_batches 2, no id twice; the retry answers `committed` (or
`already_committed` for the after-meta point) and the replay
`already_committed`.

## Retention (decision 2)

| tier | files | default | knob |
|---|---|---|---|
| detail | `detail/<day>` | 7 days | `RAILWATCH_DETAIL_RETENTION_DAYS` |
| core | `core/<day>` | the account's `retention_days` (30), capped | `RAILWATCH_CORE_RETENTION_DAYS_MAX` (90) |
| rollups, release_health | meta, DELETE | 13 months | (PruneTelemetryJob's) |

Daily at 03:00 UTC, as PruneTelemetryJob, and once at start (a server that
was down at 03:00 would otherwise never prune). A day file goes once its whole
day is past the tier (deleting is `unlink`). Before a detail day goes, its
**exemplars** are copied into that day's core file with their ids: per
(kind, group_hash, hour), the `RAILWATCH_EXEMPLARS_PER_HOUR` (3) slowest
executions and every errored one (RollupJob's predicate: request status
>= 500, command status != 0, job/task/channel outcome "failed"), with all
their queries, cache events, view renders, logs (re-indexed) and spans. A
marker in the core file's `ar_internal_metadata` makes the copy idempotent.

**Size budget** (`storage/budget.rs`, off by default): `max_data_bytes`
deletes whole day files past the budget, oldest detail day across all
environments first, then the oldest core day with its detail day (no
exemplar copy: the point is space); never today's files, meta or the control
DB. Under the lock as above, every 10 s and at start, and at once when
ingest crosses the budget: each ingest writer adds what it commits (SQLite's
count of WAL frames written, doubled for the checkpoint that later grows the
main file) to a running estimate that every pass resets to what it measured,
and a request over the estimate wakes the pass and waits up to 1 s for it,
so a burst overshoots by about one batch per request in flight. A WAL not
yet checkpointed counts the main file at the size its `-shm` header says it
will reach. Over the budget with only today's files left, ingest answers 503
with `Retry-After: 60` and the JSON error `"telemetry over max_data_bytes"`
until a pass finds it within; accepting more would grow today's files until the floor stops it.
Deleted core days move
`raw_pruned_through` to the end of the newest one, so the ingest floor
rejects rows that would recreate them. `min_free_bytes` refuses ingest with
503 + Retry-After while the filesystem is below it. After any deletion
(retention or budget) every handle to the deleted files is closed, since an
open handle keeps an unlinked file's space allocated: idle dashboard readers
are dropped, a reader checked out at the time is dropped when it is
returned, ingest writers drop cached day files, and the worker re-attaches
its own union readers; the budget does this before it reads free space.
Single-file environments
count against the budget but are never trimmed (split them). Every deletion
and every crossing is logged (docs/SELF-HOST.md, "Disk budget").

## Backups (decision 4)

Off unless `RAILWATCH_BACKUP_S3_ENDPOINT`, `_BUCKET`, `_ACCESS_KEY_ID` and
`_SECRET_ACCESS_KEY` are all set (`_REGION` default us-east-1, `_PREFIX`,
`RAILWATCH_BACKUP_INTERVAL_SECONDS` default 300). Path-style SigV4 (checked
against AWS's published example). Each pass: control DB, every meta file and
every single-file tenant via the SQLite online backup API into a temp file,
zstd -3, PUT, each only if it changed since its last upload (`PRAGMA
data_version`), and a meta file at most hourly (`META_INTERVAL`); every closed day file (2+ days old) once, skipped if listed
(a core day re-uploads as `<day>.ex` after its exemplar copy). `railwatch
backup` runs one pass; `railwatch restore` downloads into an empty data dir,
control DB last.

**MinIO proof** (`bench/storage/minio_restore.sh`, pgsty/minio = MinIO
RELEASE.2026-08-04; docker.io/minio and quay.io refused anonymous pulls):
a live env ingested over HTTP plus the split props tenant (5 closed days).
Pass 1: 3 snapshots + 8 day files, 25.7 MB -> 3.5 MB (7.3x). Pass 2: 0 day
files uploaded, 8 skipped. Restore into an empty dir: 11 files,
`integrity_check` ok, **40,481 rows, 0 tables different**. Today's and
yesterday's files are not in a backup by design (that is the RPO for
telemetry rows; control and meta are at most one interval old).

## Measurements (workstation, /dev/shm, pinned CPUs, busy box)

**Ingest** (`bench/flame/ingest.sh`, 10 envs, conc 32, 20 s, PERF=none,
main @ 74b8cc7 built separately, alternated):

| run | main | storage |
|---|---|---|
| 1 | 78,200 ev/s, 44.8 CPU-us/ev | 78,750 ev/s, 44.5 CPU-us/ev |
| 2 | 77,375 ev/s, 46.0 CPU-us/ev | 83,700 ev/s, 42.2 CPU-us/ev |
| `--spread-days 5`, 25 s, once | 106,220 ev/s, p99 470 ms | 117,360 ev/s, p99 314 ms |

Same throughput within noise when every row is "now"; +10% when rows spread
over days (smaller per-file B-trees). Not measured on disk.

**Bytes on disk** (that `--spread-days 5` run, env e1, after checkpoint):
single file 554 B/row (161 MB, 291k rows). Per-day: detail 480 B/row
(128 MB), core 1,713 B/row (46 MB, executions carry most columns), meta
2.1 MB; total 176 MB = +9%. The overhead is the fixed schema per file
(~100 KB of empty tables and indexes x 13 files) and less index sharing; it
shrinks relative to data as days fill. The point is what retention costs:
after 7 days the detail tier (73% of bytes here) is gone except exemplars,
so a 30-day tenant holds 7 days of detail + 30 of core instead of 30 of
everything: by these per-row numbers about 0.33x of the single file's size.
Not measured over a real 30 days.

**Dashboard** (`bench/storage/pages.sh` on the same 291k rows, split vs
single, 122 cases, 5 warm runs): props **122/122 identical**; sum of
per-case medians 786 -> 904 ms (+15%), per-case median 1.28x, p90 2.9x
(small pages: 1-2 ms -> 4-12 ms, the union's fixed cost over 13 files).
Slowest pages: queries.q@7d 199 -> 131 ms (faster), queries@7d 85 -> 96,
queries.parallel_cursor@7d 41 -> 77, logs.fts@7d 52 -> 67.

**Rails equivalence:** `REUSE=<work> SPLIT=1 bench/diff/props/run.sh`
130/130 identical (the props tenant split into 11 files).
`SPLIT=1 bench/diff/issues/run.sh <work>` 42/42, the same as the
single-file layout (2026-10-15, after the detectors work). The earlier
40/42 was not storage: two detector-made "scheduled task missed" issues
(#289, #290) whose `related_issues` differed (0 vs 2/1) because Rails'
LIKE has no ESCAPE clause (DIFFERENCES issues #17 and "Former Rails bugs" 3).

## Sizing (2026-11-14, branch perf/storage)

**Capacity number: about 0.79 GB on disk per million events per day.**
At the default retention (7 days of detail, 30 days of core) that is
roughly **14 GB per million events/day**, plus meta (rollups, kept 13
months). These numbers come from a bench day shaped like rebulk-system;
real apps vary with how many queries each request makes. Before this
branch: 0.82 GB per million and 14.8 GB.

### Method

One UTC day of `gen_tenant.py --spread-days 0.95 --executions 320000 --seed 5
--records-per-batch 2000 --shapes 2000 --tenants 50 --uniform --by-time
--few-logs --prod-shape`: 1,701,146 events (1.07M queries, 320k executions,
100k cache events, 83k view renders, 42k logs). Main's binary and this
branch's each posted the same batches to a fresh per-day server with
`bench/storage/day.sh`. The server is stopped before measuring, so every WAL
is checkpointed. Bytes per table and per index come from `dbstat`
(`bench/storage/bytes.py`). Data on /dev/shm.

### Bytes per event

| | main | this branch |
|---|---:|---:|
| core day file | 678.2 MB (399 B/event) | 636.7 MB (374 B/event) |
| detail day file | 691.8 MB (407 B/event) | 677.3 MB (398 B/event) |
| meta (one day of rollups) | 23.4 MB (14 B/event) | 23.6 MB (14 B/event) |
| **GB per million events** | **0.82** | **0.79** (-4.0%) |
| 7 d detail + 30 d core, per million events/day | 14.8 GB | 14.0 GB (-5.3%) |

Bytes per row, table plus its indexes: executions 1,986 -> 1,856 B, queries
555 -> 541 B. Cache events (393 B), view renders (439 B) and logs (432 B) are
unchanged. On main, indexes are 42% of the bytes of `executions` and 54% of
the bytes of `queries`.

**Where the bytes go (main).** Text is most of every row:

- `executions`: `detail` (the request JSON: URL, headers, user agent) is 57%
  of the row payload, 561 B a row. `stages` and `counters` are another 11%.
- `queries`: `trace_id`, `execution_id` and `group_hash` are 43% of the
  payload, 36 + 36 + 32 bytes of UUID or hex text on every row. The SQL
  text is already interned: meta's `query_shapes` holds it once per group,
  and the row stores `''`.
- logs: `message` is 27%.

**Production check** (`du` and `ls` only, 2026-11-14): rebulk-system's closed
days are 600-650 MB detail and 230-390 MB core. It holds 4.2 GB of detail
(7 days), 4.0 GB of core (15 days so far) and 0.9 GB of meta. At 30 days of
core that is about 4.2 + 8 + 0.9 = 13 GB, close to the bench's 14 GB per
million events a day.

### What changed

**Partial indexes** (`schema::PARTIAL_INDEXES`, DIFFERENCES #16), in new files only:

- `idx_queries_duration_cursor` and `index_executions_on_deploy_and_occurred_at` hold no rows, because no read plan uses them.
- `..._on_parent_id`, `..._on_app_tenant_and_occurred_at`, `idx_executions_kind_tenant_cursor` and `..._on_user_ref_and_occurred_at` leave out rows whose column is NULL.

Together that saves 41 MB of a core day file and 14 MB of a detail day file. Old files are not altered and stay readable.

**Page size: tried and backed out.** 8 KiB pages saved another 5% of a full core day (16 KiB saved 7%). But they broke `tests/disk_budget.rs`, the burst test: the budget's per-batch estimate counts WAL frames and checkpoint pages, and with bigger pages one batch overshot the "one batch per sender" bound (peak 25.2 MB against a 25.1 MB limit). They also double an empty file, which matters for quiet environments: 0.6 MB at 4 KiB, 1.2 MB at 8 KiB, 2.4 MB at 16 KiB, paid on each of a tenant's 38 files. Left for a change that also retunes the budget's estimate.

### Proof that reads are unchanged

**Index audit** (`bench/storage/index_audit.sh`): an 8-day prod-shaped tenant
with deploys, sessions and security fields.

- Traced with `RW_SQL_TRACE=1`: every props-diff page case (130), MCP case
  (155) and API case (195), plus a `railwatch serve` run whose ingest drove
  the rollup and detector workers.
- `plan-audit` ran EXPLAIN QUERY PLAN through a union reader on every
  distinct statement: 1,165 page, MCP and API statements and 135,445 worker
  statements. All of them plan.
- No plan names the two `WHERE 0` indexes. The partial ones are only used
  under filters that imply their clause.
- The same 136,640 statements planned against main's files and against this
  branch's files chose **the same index set, statement by statement (0
  differ)**.

**Unused on day files, left alone:**

- the `group_hash` indexes of the smaller core tables (broadcasts, mails, ...)
- the ingest_batches and sessions indexes
- `idx_executions_sessions` and `idx_visits_sessions`, which are this server's own; the bench's session cases did not reach them

Each is under 1% of a busy day.

**Dashboard latency.** props-dump, 128 page cases over the measured day, 5 warm runs, best of 2 rounds, CPUs 4-7, load average 12-28 (a shared box):

- Per case, new/base: median 0.99x, p10 0.86x, p90 1.11x.
- The ten slowest cases, re-timed best of 3 interleaved rounds: 0.90x to 1.02x. Examples: tenants@7d 297 -> 288 ms, queries.parallel_cursor@7d 276 -> 265 ms, requests.step_15m@24h 489 -> 495 ms.
- Props: either binary reading the same files gives 128/128 identical.
- Two ingests of the same batches give 113/128, even when both use main's binary. Routes tied on count swap places between ingests, so that is not this change.

**Ingest.** `bench/flame/ingest.sh`, 10 envs, conc 32, 20 s, PERF=none, alternated, 4 runs each, load average 12-14. That run included 8 KiB pages; the partial indexes alone only remove B-tree inserts.

| | ev/s | CPU-us/event |
|---|---|---:|
| main | 76.2k, 73.4k, 81.9k, 72.7k | 48.4-52.6 |
| this branch | 75.7k, 70.3k, 73.4k, 73.6k | 50.6-54.8 |

That is within this box's noise. Not measured on a quiet machine or on disk.

### Considered, not done

- **zstd per row with a trained dictionary** on `executions.detail`: 561 -> 38 B a row on the bench day (179.6 -> 12.1 MB; plain per-row zstd only reaches 119 MB). That is about 25% of a core file. It is not a side change:
  - the security index (`idx_executions_requests`) and the pages read `detail` with `json_extract`;
  - Rails needs the column to stay JSON;
  - a dictionary must be stored and versioned per file.
- **Interning ids** (`trace_id` and `execution_id` as integers) would save most of 43% of a query row. But the gem schema and every join use the text.
- **Whole-file compression of closed days**: zstd -3 gives 6.4x on a core day and 4.2x on a detail day. SQLite cannot read such a file in place, so this needs a compressing filesystem (btrfs `compress=zstd`, ZFS) or a cold tier (research/IMPROVEMENTS.md 1(ii)). Production is on ext4.
- **Meta** is 0.9 GB for rebulk-system: 13 months of hourly rollups, mostly query rollups (per group per hour: a 115 B t-digest plus the SQL as `name`). The obvious next step is to roll hours older than 90 days (the longest custom window) into days.

**For Track D:** none of the indexes made partial here serves a slow query. The two biggest query indexes, `idx_queries_slowest` (57 B a query row) and `index_queries_on_execution_id_and_occurred_at` (84 B), both show up in the audit's plans and are kept.
