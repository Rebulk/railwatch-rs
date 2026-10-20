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
DB. Under the lock as above, every 10 s and at start. Deleted core days move
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
zstd -3, PUT; every closed day file (2+ days old) once, skipped if listed
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
