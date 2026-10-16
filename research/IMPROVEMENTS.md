# railwatch-rs: ranked improvements, measured (2026-10-13)

Workstation: 16 cores, 27 GiB, encrypted btrfs on NVMe (88% full, shared), load average
9-17 from other agents throughout. Every timing pins CPUs with `taskset`. "RAM" means
/dev/shm, "disk" means bench/scratch on that btrfs. Most runs were done once; a few
were repeated (noted). Anything not measured says so.

Datasets:
- **SYN-5G**: a 5.25 GB tenant made by the stock server + rw-load (`scripts/build_days.sh`):
  6 "days" (consecutive rw-load runs, 1.5M records each; day 1 has 3M) = 5.06M queries,
  639k executions. Real gem schema, every index. Its "days" are about 90 s apart in
  wall time, so date-range queries are realistic but absolute ages are not.
- **REAL-RS**: an extract of a real Rails app's tenant (40k executions, 758,662 queries,
  682 MB). Not published (it is production data); any tenant of similar size reproduces
  the shape. It predates `idx_queries_slowest`.

## Ranking

| # | Improvement | Measured effect | Rails can still read the file? | Effort |
|---|---|---|---|---|
| 1 | Per-day partition files (same gem schema per file) | Retention: 126 s on RAM / >30 min on disk (killed) to delete 1 day from 5 GB, leaving 27.7% of the file as free pages → **0.1 ms unlink** | Each partition yes. The *set* no (gem expects one file); a `railwatch export` that merges partitions is the way back | L |
| 2 | zstd Parquet cold tier for days older than N | REAL-RS queries **557 → 7.2 B/event (77x)**, executions 2,024 → 116 B/row, lossless; dashboard scans 28-34 ms vs 3-4 ms SQLite hot path | No (Parquet). Way back = rehydrate to SQLite (not measured) | L |
| 3 | Watchdog: per-host heartbeat + external dead-man's switch + alert outbox | Real health streams: per-host max gap **10.6 s / 4.8 s** over 32k / 11k gaps; 0 gaps > 30 s per host | Not affected (control DB) | M |
| 4 | Zero-gap restart (inherited listening socket) | Plain restart: 16-38 ms listener gap, 1 error in 2 of 3 runs. Handoff: **0 errors over 3 swaps x 2 rates** | Not affected | S |
| 5 | Drop/partialize dead indexes | 174 MB (3.3%) of SYN-5G in 2 dead queries indexes; partial swap takes 9.3 s + 17.5 s under the write lock | Yes, if partial (the gem's planner can't tell); dropping is safe for gem 0.9.1 reads | S |
| 6 | Compact query encoding (Rust-only format) | REAL-RS **557.1 → 142.0 B/event (3.9x)** all-in, 0 round-trip / 0 timestamp diffs | No, unless a gem-schema `queries` VIEW is kept (not measured) | L |
| 7 | zstd'd nightly snapshots (the HA backup tier) | 5.25 GB → 0.88 GB in 8.7 s (2 threads); REAL-RS 682 → 78.5 MB (8.7x); restore decompress 11.4 s | Yes (byte-identical file) | S |
| 8 | Durable spool before SQLite | Ack cost on disk ≈ SQLite synchronous=FULL; it buys stall isolation, not latency | Yes (spool is replayed into the same schema) | M |

## 1. Storage

### 1(iv) Partition by day: retention becomes `rm` — **rank 1**

**What.** One SQLite file per (tenant, UTC day), each with the gem's exact DDL. Ingest
writes today's file; reads ATTACH the days in the window; retention DETACHes and unlinks.
Small/global tables (query_shapes, receipts, people, rollups, ingest_batches) stay in one
`meta` file per tenant.

**Measured** (`retention/prune_day.py`, `partition/split_days.py`, `partition/drop_and_query.py`):

| | result |
|---|---|
| Rails `PruneTelemetryJob` replica, cutoff = day 2, SYN-5G **on RAM**, 1 core | **125.75 s** (queries alone 93.1 s for 1.69M rows; executions 10.3 s; logs FTS unindex 4.5 s); 3.24M rows. Afterwards the file is **still 5.258 GB**, with 1.455 GB (27.7%) on the freelist. Getting it back takes a VACUUM, i.e. rewriting the whole file |
| Same on **disk** | **Killed at the 30-min tool limit, not done**: queries 756,706 rows still left (≈55% through), executions done, logs not started. Single run. Each DELETE autocommits with synchronous=normal under WAL, and on this btrfs that I/O dominates. A dedicated VPS NVMe will be faster; I did not measure one |
| Split SYN-5G into per-day files with identical DDL + FTS, on RAM | 55-120 s/day (one-off migration cost) |
| Drop a day partition (DETACH + unlink, 2.94M rows, 1.83 GB) | **0.1 ms**, and the bytes really are free |
| Slowest-51 page over 6 attached partitions (top-k per day, merged) | 0.1-0.5 ms (monolith with INDEXED BY: 3-4 ms warm, 226 ms cold) |
| Grouped counts over a 2-minute window | 6 partitions 1,600 ms vs monolith 1,657 ms (warm, 4-5 runs). The planner picks a different index per file (partition: covering group_hash scan; monolith: occurred_at search). Equal wall time; neither is a regression |

**Rails compatibility.** Each partition is a valid gem tenant file (`cross_open.sh` not
rerun on partitions: **not verified**). The gem's dashboard reads one file, so going back
means `railwatch export --tenant X > tenant.sqlite3`, merging the days. Ids: AUTOINCREMENT
per file restarts at 1, so a merged export has to remap ids, or else each partition has to
seed `sqlite_sequence` from the previous day's max (cheap; not prototyped). Cross-day
reads (trace views, cursor pagination) need UNION ALL over attached files, limited to
`SQLITE_MAX_ATTACHED` = 10 by default (125 at compile time); 90-day retention needs a
compile flag or a day-plus-week layout.

**Effort.** L: writer routing, an ATTACH-aware read layer for the dashboard track, the
export command, the id policy. Unrelated win: backups and restores become per-day and
immutable (yesterday's file never changes), which is exactly the shape litestream is
bad at and rsync/S3 PUT are good at.

### 1(ii) zstd Parquet cold tier — **rank 2**

**What.** Days older than N (e.g. 7) are rewritten once to zstd Parquet sorted by
occurred_at and queried with an embedded engine (DuckDB or DataFusion). SQLite page
compression isn't available stock, as the brief said.

**Measured** (`cold/to_parquet.sql`, `cold/verify_roundtrip.sql`, `cold/read_cost.sh`), REAL-RS:

| table | SQLite (table + indexes) | zstd-9 Parquet | ratio |
|---|---|---|---|
| queries (758,662 rows) | 422.6 MB = 557 B/row | **5.49 MB = 7.2 B/row** | **77x** |
| executions (40,000 rows) | 81.0 MB = 2,024 B/row | 4.62 MB = 116 B/row | 17.5x |
| queries, snappy Parquet | | 11.5 MB | 37x |

Lossless: 758,662/758,662 rows, **0 rows differ** on every compared column (occurred_at as
the exact SQLite text, duration, execution_id, group_hash, sql, source, trace_id,
execution_preview, row_count). REAL-RS has 0 inline SQL rows and 2,224 groups, so this is
the post-dedupe world. The 77x comes mostly from sorting: the envelope repeats (one server,
one deploy, 40 previews), and Parquet stores no indexes.

Read cost, warm, 1 core, 5 runs, DuckDB CLI (including its ~9 ms process start):

| query | SQLite | Parquet |
|---|---|---|
| grouped by group_hash over the window | 356-678 ms (one 4,390 ms outlier) | **27-29 ms** |
| slowest 51 in window (REAL-RS has no slowest index) | 46 ms | 29-31 ms |
| one execution's queries (point lookup) | 3-4 ms | 18-20 ms |

So the cold tier is faster for aggregates and about 5x slower for point lookups, but stays
under 35 ms. In-process DataFusion/arrow-rs would skip the process start: **not
measured**; I didn't build it (adding the arrow/parquet crates is a large dependency
decision for the maintainers). Writing is also unmeasured in Rust; DuckDB wrote both files in under
2 s.

**Rails compatibility.** None: the gem can't read Parquet. The way back is a
`rehydrate` command that writes the gem schema again (not built). Treat this as the
Rust-only, opt-in tier, kept off by default if going back must stay one click.

**Effort.** L (read layer per dashboard page). It combines naturally with (1): a day's
partition is either `dayN.sqlite3` (hot) or `dayN.parquet` (cold).

### 1(i) Compact query encoding as a Rust-only format — rank 6

**Measured** (`compact/04_compact_queries_int.sql`: the spike's SQL with `julianday()`
replaced by integer math), REAL-RS, VACUUMed: **557.1 → 142.0 B/event all-in (3.9x)**,
117.1 events-only; **0 rows that don't round-trip, 0 timestamp differences** (exact
26-char text). Built in 7.7 s for 759k rows. This reproduces the spike (162 B on the
self-monitoring extract) on the heavier workload.

**Why it ranks below Parquet.** It costs the same kind of effort (every queries read path
changes) for 3.9x instead of 77x on cold data, and it breaks Rails compatibility just
as hard. It only wins on *hot* data, and on a box where hot is 7 days of 90, hot is not
where the disk goes. A gem-schema `CREATE VIEW queries AS SELECT ... FROM q2 JOIN ...`
would let the gem read it back; read cost and INSERT-through-view triggers: **not measured**.

### 1(iii) Dead and all-NULL indexes — rank 5

**Measured** (`indexes/query_plans.sql`: EXPLAIN QUERY PLAN after ANALYZE over gem 0.9.1's
read shapes on `queries`): plans use `idx_queries_slowest`,
`index_queries_on_group_hash_and_occurred_at`, `idx_queries_explained`,
`index_queries_on_execution_id_and_occurred_at`, and
`index_queries_on_occurred_at_and_duration`. `idx_queries_tenant_duration_cursor` appears
in **no** plan (the tenant filter is forced onto `idx_queries_slowest` by `cursor_page.rb:35`).
`idx_queries_duration_cursor` only shows up for a `duration < ?` predicate, which no gem
read sends (grep of app/ for duration ordering: `tenant.rb:204` and
`issue_detection_snapshot.rb:35` both order executions, not queries; `view_renders`).

On SYN-5G: tenant_duration_cursor 89.7 MB + duration_cursor 84.1 MB = **173.8 MB (3.3%)**.
`app_tenant` is NULL on all 5.06M rows, so the partial form is 4 KB. On the 5 GB file:
`DROP INDEX` **9.3 s**, then `CREATE INDEX ... WHERE app_tenant IS NOT NULL` **17.5 s**,
both holding the write lock (one run). The freed pages sit on the freelist (21,910 pages)
until VACUUM. Do it at tenant creation in Rust (free), or under partitioning (each new
day file is created with the partial index, so old files age out and no migration is
needed).

**Rails compatibility.** Partial indexes: the gem reads fine (same name, plans unchanged
for its shapes; the spike checked 40 plans). A gem migration that recreates the index
would conflict: **not checked**.

## 2. High availability

### 2(i) Litestream-style WAL shipping

**Not measured here, deliberately.** A past incident on the Rails cloud was exactly this
workload: append-heavy multi-GB tenants turned into tens of thousands of LTX objects, a
corrupt L0 made every compaction re-read the backlog, and leaked sockets wedged the host.
The decision was: telemetry is snapshot-only. Partitioning (1) changes the shape: closed days
are immutable, so the "stream" becomes one PUT per closed day plus streaming *only*
today's file and the small control/meta files. Even then, today's file of a busy tenant
is the same 1-2 GB/day append stream that caused the incident, so I'd stream **only**
control.sqlite3 (~small, the thing that holds tokens, alert rules and heartbeat
expectations) and snapshot the rest.

**Measured, the snapshot side** (rank 7): `zstd -3 -T2` of SYN-5G **5.25 GB → 0.88 GB in
8.7 s**, decompress **11.4 s**, `PRAGMA quick_check` on the restored file 184 s (disk, one
run). REAL-RS 682 → 78.5 MB (8.7x) at -3, 50.3 MB at -19. The Rails cloud uploads
uncompressed `VACUUM INTO` snapshots (census: backups grew 3x faster than live data), so
this alone is roughly a 6-9x cut in backup storage and upload time. Restore RTO is
dominated by download (not measured; 0.88 GB at 100 MB/s ≈ 9 s, an estimate).

### 2(ii) Active-passive pair + the gem's retry ladder: the loss window

From the gem source (v0.9.1), not measured live:
- Direct path: `Reporter` retries 5xx/408/429/402 and transport errors, `MAX_RETRY_ATTEMPTS = 8`,
  delay `min(2^(n-1), 60) * U(0.5,1)` s → worst-case cumulative wait 1+2+4+8+16+32+60+60 =
  **183 s** (≈92 s at the jitter's low end), then the batch is dropped and counted. While one
  batch is retained, new records pile up in the 10,000-record / 16 MiB buffer, which
  drops its **oldest** records on overflow (`buffer.rb:41`). So the real window is
  `min(~1.5-3 min, buffer fill time)`. At the bench rate (500 records per batch every
  ~0.25-2 s per process) the buffer fills in 20 batches, i.e. **seconds to tens of
  seconds** on a busy process. That is the number that matters, and it is shorter than the
  retry ladder.
- Receipted (export outbox) path: durable on the app side, no attempt limit, expiry
  `RAILWATCH_EXPORT_MAX_AGE_SECONDS` (default 24 h), receipts make the replay idempotent.
  **Failover loses nothing** within 24 h.

So active-passive with floating-IP/DNS failover inside ~60 s is lossless for receipted
apps and loses roughly "buffer-fill time" of direct-path telemetry. The passive side needs
the control DB (stream it; it's small) but **not** the telemetry history to start accepting.
History restored afterwards from the nightly snapshot (or synced day partitions) is fine,
because new writes land in today's partition. Not prototyped: the failover itself.

### 2(iii) Durable spool before SQLite

**Measured** (`src/bin/spool_bench.rs`, 400 x 21 KB bodies = rw-load's gzipped batch size,
1 core, 2 runs on disk + 1 on RAM):

| per ack | disk run 1 | disk run 2 | RAM |
|---|---|---|---|
| spool append + fdatasync per batch | p50 10.3 / p99 279 ms (max 13.2 s) | p50 1.26 / p99 21.8 ms | 0.01 ms |
| spool, 16 batches per fdatasync (per group) | p50 16.3 / p99 44 ms | 12.5 / 19.9 ms | 0.14 ms |
| SQLite blob insert, synchronous=FULL | p50 9.6 / p99 2,268 ms | 1.28 / 22.6 ms | 0.03 ms |
| SQLite blob insert, synchronous=NORMAL | p50 0.05 / p99 1.2 ms | 0.05 / 0.73 ms | 0.03 ms |

Read: a durable append costs the **same** fsync as a durable SQLite commit (run 1 caught
another tenant of this disk thrashing it; both paths stalled together). A spool doesn't
make acks faster. It makes them **independent of SQLite work**: index maintenance, a
checkpoint, a prune DELETE holding the write lock, a migration. The server's real ack
today is a commit with synchronous=NORMAL, which is **not durable at ack** (see 4).

Guarantees: direct path stays at-least-once (unchanged). Receipted path: the ack today
is "committed" plus `committed_at`; acking from the spool means the receipt row doesn't
exist yet, so a replay arriving before the spool drains must be checked against the spool
index too, or it double-writes. The gem accepts `ok` with a `reason` as `:deferred`
(`http.rb:268`), and its outbox **keeps** deferred deliveries (`client.rb:89-92`), so a
spool ack must still say `committed` to retire the outbox row. Then it is a promise backed
by the fsync'd spool. That's sound only if drain is crash-safe and idempotent (the
delivery id is the key). Recommend: spool **only when the tenant writer is behind**
(queue depth or lock wait > X), not always: the fast path stays one commit. Not built.

### 2(iv) Graceful restart with zero dropped requests — rank 4

**Measured** (`restart/naive_restart.sh`, `restart/handoff.py`, `restart/handoff_run.sh`,
`src/bin/fdserve.rs` = the stock router/AppState, but with the listener inherited on fd 3
as systemd's LISTEN_FDS does it):

| | runs | client-visible failures | ingest_batches rows = 200s |
|---|---|---|---|
| Stock binary, SIGTERM then start new (20 batches/s, 30 s) | 3 | **1, 0, 1** transport errors; listener gap 16 / 38 / 16 ms | yes |
| Socket handoff, 3 generation swaps, 20 batches/s, 40 s | 1 | **0** of 799 | 799 = 799 |
| Same at 60 batches/s offered (39.7 accepted, saturated) | 1 | **0** of 1,589 | 1,589 = 1,589 |

The old generation drained in 6-34 ms (unsaturated) and 242-288 ms (saturated). Both
generations briefly wrote the same tenant file, and SQLite's lock serialised them, with no
busy errors at busy_timeout 5 s. In production: systemd `.socket` unit with
`FileDescriptorStoreMax`, or a tiny supervisor mode inside the binary (`railwatch serve`
re-execs itself with the fd). The gem's retry would absorb the stock binary's single error,
so this matters most for deploys under heavy load and for the Kamal-less install story.
Effort S: about 20 lines in main.rs to accept `LISTEN_FDS`.

## 3. Watchdog reliability — rank 3 (design; the gap data is measured)

The separate-app promise is "your app died → someone gets paged", even when Railwatch is
itself degraded. Three parts:

**a. Per-host heartbeat expectation (in Railwatch).** The gem already sends a `health`
record every 15 s (`RAILWATCH_HEALTH_INTERVAL`) from every web/worker process. Measured
gaps on REAL-RS and the self-monitoring extract (`watchdog/heartbeat_gaps.sql`): per
(server, pid) avg 12.9-15.3 s, with 3 gaps > 30 s in each (all larger than 220 s: restarts
the pid key doesn't catch). **Per host**: max gap **10.6 s** (32,693 gaps) and **4.8 s**
(11,442), **zero > 30 s**. So: key the expectation per host, not per pid, and fire at
**60 s silence** (4 missed intervals, 5.7x the worst observed gap). Rails'
`SilentHostCheckJob` uses 10 min and only for Kamal-declared servers. Rust can be
event-driven: an in-memory `last_seen[env][server]` updated in the ingest path (no DB
write per batch; flush every 10 s to control), plus a 5 s ticker. Hosts learned
automatically (seen in the last 24 h), and also from `/kamal` markers; a host that stops
after a deploy marker that removed it is not alerted. **Separate signals**: host silent
(no records at all) vs process silent (health stopped but records still flow) vs
ingest 401/402 (the app is fine, Railwatch rejects it: page that too).

**b. External dead-man's switch (Railwatch's own death).** A task GETs
`RAILWATCH_DEADMAN_URL` (healthchecks.io, Better Stack, Cronitor) every 30 s, **only if**
the watchdog ticker has run in the last 15 s, control.sqlite3 accepted a write in the last
30 s, and at least one tenant writer committed in the last 60 s (when any env is active).
So a wedged writer or a stuck loop stops the ping as surely as a dead process does. The
external service pages on 2 missed pings. This is the only piece that survives the box
dying; it costs 1 HTTP request/30 s. Not built.

**c. Alert delivery as a durable outbox.** `alert_deliveries` in control.sqlite3
(rule, channel, payload, attempts, next_attempt_at, state), written in the same transaction
that decides to fire. Retries: 5 s, 15 s, 1 min, then every 5 min for 24 h, honouring
Retry-After. Each channel has an independent circuit (Slack down ≠ email down), and the
dead-man URL is pinged with `/fail` if an alert has been undeliverable for 10 min, so the
external service pages *about* the stuck alert. Fire-and-forget delivery is the failure
mode to avoid. A page that was decided but never sent is an invisible success
(CLAUDE.md rule 12).

The two pieces that matter most are (b) and the per-host threshold. (b) is the only part
that covers "Railwatch itself is down", and the threshold comes from data.

## 4. Other measured findings

**`synchronous=NORMAL` is the real durability window.** Ingest acks after a commit with
`synchronous=normal` (Rails parity, `schema.rs`). In WAL mode that survives process crashes
but not power loss / kernel panic: acked batches since the last checkpoint fsync can be
lost. Measured cost of closing it: the spool table above (FULL p50 1.3-9.6 ms vs NORMAL
0.05 ms per commit). A bounded ingest test (10 envs, conc 32, 30 s, disk, 2 runs each):
NORMAL 10.2k / 11.1k ev/s, `synchronous=off` 12.5k / 11.9k ev/s, so fsync is ≤ ~15% of
throughput here, within run noise. FULL wasn't run end to end (no server knob for it).
Recommendation: keep NORMAL. Document "acked = survives process crash, not host crash",
and let the gem's receipted path be the answer for apps that need more. The coalescing
writer already amortises the commit, so FULL may cost little at load: **not measured**.

**What I did not get to, and why** (hard budget): mmap_size and wal_autocheckpoint tuning,
per-tenant coalescing limits under multi-tenant skew, and in-process DataFusion read cost.
None of these was measured. Gate-1 data says the writer is fsync/IO-bound on real disk,
so checkpoint tuning (a larger autocheckpoint with a dedicated checkpointer thread) is the
likeliest next throughput lever.

## Reproduce

```
research/scripts/build_days.sh /dev/shm/rwr/big 6 3000        # SYN-5G (~10 min)
python3 -I research/retention/prune_day.py FILE CUTOFF_EPOCH    # Rails prune replica
python3 -I research/partition/split_days.py MASTER OUT day*.start
python3 -I research/partition/drop_and_query.py OUT
duckdb < research/cold/to_parquet.sql; duckdb < research/cold/verify_roundtrip.sql; research/cold/read_cost.sh
sqlite3 COPY_OF_rs_base.sqlite3 < research/compact/04_compact_queries_int.sql
sqlite3 SYN-5G < research/indexes/query_plans.sql
sqlite3 -column EXTRACT < research/watchdog/heartbeat_gaps.sql
cd research && cargo build --release   # rw-fdserve, rw-spool-bench (path dep on crates/server, unchanged)
research/restart/naive_restart.sh; research/restart/handoff_run.sh
target/release/rw-spool-bench DIR 400 21000
```
Raw outputs: `research/results/`.
