# Rails vs Rust: the Railwatch ingest server, measured again (2026-11-14)

This is the evidence behind "the Railwatch server left Rails". It replaces the
one-run-each numbers in [GATE1.md](GATE1.md) and is written for a Rails
developer who suspects the comparison was rigged. Every script is in
[`bench/rails-vs-rust/`](../bench/rails-vs-rust/) and every raw run in
[`bench/results/rails-vs-rust/`](../bench/results/rails-vs-rust/).

## Short version

- **Throughput (2 cores, 3 GiB, same disk):** Rust takes 1.5x Rails' events/s
  with 1 app (16 senders), 2.1x with 10 apps, 2.4x with 50 apps. One sender
  to one app: 1.3x.
- **CPU per event:** Rust uses 40-60% of Rails' CPU for the same work
  (48-81 vs 106-168 CPU-ms per 1,000 events). That
  includes Rails' Solid Queue rollups and grouping and Rust's own rollups,
  grouping and watchdog.
- **Memory:** idle, Rust holds 4-44 MiB of private memory and Rails about
  420-500 MiB. Under
  load the gap narrows a lot, and at 50 apps Rust's peak was *higher* than
  Rails' (see Memory).
- **Where Rails is fine:** at a steady 8 batches/s (4,000 events/s) for one
  app, Rails answered with p50 75 ms and p99 168 ms. Rust's figures were 54 ms
  and 115 ms. Rails kept up at every fixed rate tested, up to 15,000 events/s
  across 50 apps. Its p99 at 10 and 50 apps was 1-3 s, against Rust's
  0.1-0.2 s.
- **Mechanism:** "per-request fsync" is the wrong story. With WAL and
  `synchronous=NORMAL` neither side fsyncs per commit (Rails made 1.3-2.9
  fsyncs per batch, Rust 0.2-4). What differs is how concurrent writes to one
  file are serialized.
  - Rust's per-tenant writer coalesced 7.9 batches per transaction at 16
    senders and 29 at 64.
  - Rails runs one transaction per request, and requests wait on SQLite's
    lock in 1 ms sleeps.
  - Turning fsync off entirely moved Rails by +7% and Rust by +9%.
- **Dashboard reads on one identical file:**
  - Rust is 7-50x faster on overview, requests, jobs and exceptions (1-6 ms
    vs 14-68 ms).
  - The warm queries page is a tie (15 vs 16 ms; Rails serves it from its
    1-minute cache).
  - Cold, the queries page took 341 ms on Rails and 22 ms on Rust.
- **The counterfactual:** I prototyped a Rails group commit per tenant: same
  image, one initializer. It coalesced only about 2 batches per transaction
  with one app, and 1.0 with many apps. It did **not** close the gap. The
  reasons are structural (two Puma processes, the GVL around mapping), not a
  missing line of Ruby. Details below.

## What was compared

| | Rails | Rust |
|---|---|---|
| code | railwatch-cloud @ `71020d3`, the image production last ran (its idle web/litestream containers on the prod host carry that tag) | railwatch-rs @ `ad5a091` (origin/main), `bin/build` |
| processes | the two roles production runs: web (Thruster + Puma, `WEB_CONCURRENCY=2` x `RAILS_MAX_THREADS=5`, preload) and job (`bin/jobs`, Solid Queue: dispatcher, 2 worker pools, scheduler) | one `railwatch serve` process: ingest, per-tenant writers, incremental rollups (5 s flush), grouping, follow-up drain, watchdog, alert loop |
| runtime | Ruby 4.0.0 + YJIT, jemalloc, Rails 8.1.3.1, sqlite3 gem 2.9.6 (SQLite 3.53.2), railwatch gem 0.9.0 | Rust release build, rusqlite 0.41 (bundled SQLite) |
| SQLite pragmas | `database.yml`: WAL, `synchronous=normal`, `cache_size=2000`, `mmap_size=128M` | the same, plus `temp_store=MEMORY` and `cache_spill=OFF` on the writer |
| storage layout | one SQLite file per environment | one meta file plus per-day files per environment ([STORAGE.md](STORAGE.md)) |
| ingest path | direct (not receipted), as gem 0.9.x sends | the same |

Both get the same load generator (`rw-load`, gem-shaped 500-record gzip NDJSON
batches, 8 queries/2 cache events/1 view/2 logs per execution, 5% exceptions).
Every batch is distinct, built before the timed phase and sent once.

**Two changes to Rails, both disclosed:**
1. The per-environment ingest limit (600 requests/min, hard-coded) is removed
   by a mounted initializer
   ([`rails/zz_bench_no_environment_limit.rb`](../bench/rails-vs-rust/rails/zz_bench_no_environment_limit.rb)).
   Without that, one app cannot exceed 10 batches/s. Rust's equivalent limit is
   off (`RAILWATCH_RATE_LIMIT_ENV=0`). Removing it saves Rails a Solid Cache
   write per request, which flatters Rails slightly. The per-IP limiter stays
   on for both; rw-load rotates `X-Forwarded-For` so it runs but never trips.
2. After each run the harness enqueues `RollupCatchupJob` from a throwaway
   container outside the measured cgroup. Production does this every 5
   minutes. Without it, Rails' debounced rollups would not yet cover the run,
   and Rails would be credited for work it had not done. The catchup's
   RollupJobs run in the jobs container inside the cgroup and are counted.

## Hardware and isolation

- **Machine:** an AMD Ryzen 7 8745HS (8 cores / 16 threads, up to 4.97 GHz,
  16 MiB L3) with 27 GiB RAM, running Linux 7.2.4 (Arch) and Docker 29.7.2
  (cgroup v2, systemd driver). It is a shared workstation. Other agents were
  building and benchmarking on it the whole time: the 1-minute load average
  before a run ranged from **6.9 to 36**, and host IO pressure (`full avg10`)
  from 0.3% to 48%. Both are recorded per run in the summary.
- **CPU:** a systemd slice `rvr.slice` holds an **exclusive cpuset
  partition** of CPUs 6, 7 and their SMT siblings 14, 15, so nothing else on
  the machine can run there. Each server's container(s) are pinned to CPUs
  6-7, which are two physical cores with idle siblings. Rails' web and jobs
  containers share those two cores, as they share the host in production. The
  load generator runs on CPUs 2-5.
- **Memory:** the same slice is capped at `MemoryMax=3G` with no swap. That
  cap covers Rails' web and jobs together, or the single Rust process.
- **Disk:** the workstation's NVMe could not be used. It is a 94%-full
  encrypted btrfs shared with other builds, and its `fdatasync` p99 was
  **2.4 s** (`fsync_probe.py`, measured at the start). Instead, both servers
  write to **ext4 on zram behind `dm-delay` with a 1 ms delay on every write
  bio**. That disk gives `fdatasync` a p50 of 3.0 ms and a p99 of 3-6 ms,
  roughly a modest cloud NVMe, and identical for both sides. Reads are not
  delayed. Caveat: real NVMe has more variance and real read latency; see
  Caveats.
- **Networking:** host networking for both servers, so neither pays a
  docker-proxy hop. Each server listens on 127.0.0.1.
- **Measured by** the slice's cgroup files:
  - CPU: `cpu.stat usage_usec`
  - private memory: `memory.stat anon`
  - private memory plus page cache: `memory.current`
  - bytes written to the device: `io.stat`
- **fsyncs:** `perf stat -G rvr.slice` on `sys_enter_fsync` /
  `sys_enter_fdatasync`.

## Method

`bench/rails-vs-rust/matrix.sh` runs 9 cases x 2 sides x **3 repetitions**.
Odd repetitions run Rails first and even ones Rust first, so host drift does
not land on one side. Each run (`run.sh`) does the following:

1. Starts a **fresh server on a fresh data directory** and creates N
   environments: Rails through its own models (one tenant file each), Rust
   through `railwatch env create`.
2. Waits 20 s, then records idle memory, the load average and IO pressure.
3. Runs rw-load for the duration, either closed loop (max throughput) or
   open loop at a fixed batch rate. It samples memory every 0.5 s and counts
   fsyncs.
4. Keeps measuring after the load stops, until the slice is idle for 6 s in a
   row (at most 120 s; the longest tail was 17 s). That counts deferred work
   in CPU: Rails' Solid Queue rollups and grouping, and Rust's 5 s rollup
   flush.
5. Records memory after load, telemetry bytes on disk and device bytes
   written. For Rails it also checks Solid Queue: 3 failed jobs in all 54
   Rails runs, 0 pending.

Results are reported as **median (min-max) of 3 runs**.

## 1. Ingest

| case | side | n | events/s | p50 ms | p99 ms | CPU s (incl. tail) | CPU ms / 1k events | peak anon MiB | peak anon+cache MiB | fsync / batch | non-200 |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| max-1app | rails | 3 | 9100 (8878-10178) | 844 (782-858) | 2338 (1621-2447) | 68.2 (67.2-71.5) | 166.5 (156.1-168.2) | 1083 (1053-1225) | 3072 (3046-3072) | 1.34 (1.30-1.37) | 0 |
| max-1app | rust | 3 | 13500 (12711-14022) | 639 (557-708) | 943 (902-981) | 42.6 (37.7-43.5) | 68.9 (62.1-74.5) | 294 (291-353) | 761 (601-922) | 0.73 (0.72-0.73) | 0 |
| max-10app | rails | 3 | 13867 (12000-14144) | 1068 (1067-1378) | 2649 (2175-3118) | 73.7 (69.8-73.8) | 118.3 (109.7-136.5) | 1184 (1168-1206) | 1816 (1743-1864) | 2.54 (2.44-2.58) | 0 |
| max-10app | rust | 3 | 29722 (29711-30222) | 522 (522-532) | 965 (963-1051) | 81.5 (79.7-82.3) | 60.5 (59.6-60.9) | 568 (477-583) | 1544 (1474-1595) | 2.38 (2.37-2.42) | 0 |
| max-50app | rails | 3 | 17178 (16756-20378) | 1742 (1568-1875) | 3466 (3004-3722) | 91.1 (87.0-93.7) | 120.8 (94.9-121.2) | 1467 (1457-1583) | 2345 (2297-2612) | 1.41 (1.35-1.46) | 1 |
| max-50app | rust | 3 | 41356 (41344-44900) | 763 (689-784) | 1375 (1169-1936) | 92.3 (89.5-92.4) | 48.1 (45.7-49.7) | 1784 (1763-1841) | 3072 (3072-3072) | 2.45 (2.44-2.57) | 0 |
| rate-1app | rails | 3 | 3989 (3989-3989) | 75 (68-83) | 168 (153-181) | 22.9 (21.5-25.2) | 127.6 (119.8-140.4) | 764 (734-812) | 1045 (985-1067) | 2.92 (2.92-2.94) | 0 |
| rate-1app | rust | 3 | 3989 (3989-3989) | 54 (51-77) | 115 (95-7750) | 12.9 (12.3-13.4) | 71.9 (68.5-74.7) | 136 (133-191) | 367 (364-400) | 3.84 (3.26-3.85) | 0 |
| rate-10app | rails | 3 | 12489 (12044-12489) | 107 (60-610) | 1074 (980-1985) | 59.5 (56.9-71.4) | 105.9 (101.2-131.7) | 1154 (1150-1180) | 1701 (1698-1756) | 2.54 (2.51-2.56) | 0 |
| rate-10app | rust | 3 | 12489 (12489-12489) | 42 (38-47) | 115 (99-187) | 30.9 (26.9-34.1) | 55.0 (47.9-60.7) | 525 (511-525) | 1073 (1063-1125) | 2.97 (2.96-2.97) | 0 |
| rate-50app | rails | 3 | 14989 (14789-14989) | 204 (72-1499) | 2771 (2699-3283) | 75.8 (72.9-82.1) | 112.4 (108.1-123.4) | 1394 (1383-1423) | 2150 (2146-2256) | 1.34 (1.32-1.38) | 0 |
| rate-50app | rust | 3 | 14989 (14989-14989) | 24 (21-24) | 228 (164-235) | 31.5 (29.7-33.0) | 46.7 (44.0-48.9) | 1094 (1085-1106) | 1923 (1735-2242) | 2.11 (2.10-2.11) | 0 |
| conc1-1app | rails | 3 | 6050 (6033-6583) | 80 (76-83) | 140 (130-149) | 25.4 (24.6-27.2) | 135.9 (128.6-149.9) | 663 (638-679) | 908 (841-936) | 2.95 (2.92-2.97) | 0 |
| conc1-1app | rust | 3 | 7817 (7650-8717) | 65 (58-65) | 113 (108-120) | 19.2 (19.0-19.3) | 81.0 (73.8-83.7) | 161 (151-183) | 389 (369-449) | 4.01 (3.99-4.05) | 0 |
| conc4-1app | rails | 3 | 8150 (7833-10233) | 183 (180-192) | 1043 (514-1584) | 41.1 (39.2-47.9) | 160.3 (156.0-174.9) | 984 (926-990) | 2325 (2013-2722) | 1.56 (1.32-1.60) | 0 |
| conc4-1app | rust | 3 | 10667 (9200-11000) | 195 (193-200) | 326 (296-427) | 24.0 (19.8-25.1) | 75.0 (71.7-76.1) | 217 (179-218) | 488 (440-508) | 2.32 (2.19-2.32) | 0 |
| conc64-1app | rails | 3 | 10317 (10000-11183) | 3277 (3090-3343) | 4832 (4561-5801) | 51.9 (51.1-54.7) | 167.7 (163.0-170.3) | 1102 (1069-1102) | 2442 (2316-3072) | 1.39 (1.28-1.47) | 0 |
| conc64-1app | rust | 3 | 22450 (21350-24067) | 1550 (1412-1607) | 2260 (1974-2456) | 36.6 (36.2-38.3) | 54.3 (53.0-56.5) | 378 (374-456) | 867 (804-1006) | 0.20 (0.20-0.20) | 0 |

Notes on the table:
- `CPU s` is everything in the slice from load start to idle, deferred work
  included. `CPU ms / 1k events` divides it by accepted events.
- `peak anon` is private memory. `peak anon+cache` also counts the page cache
  of the tenant files charged to the cgroup, capped by the 3 GiB limit (it
  reaches the cap in the heaviest runs on both sides).
- `non-200`: one 503 (`database busy`) from Rails in one 50-app run, out of
  about 18,000 Rails requests. Rust: none.

**Max throughput (closed loop).**
- With 1 app and 16 senders: Rails 9,100 events/s, Rust 13,500 (1.5x).
- With 10 apps: 13,900 vs 29,700 (2.1x).
- With 50 apps: 17,200 vs 41,400 (2.4x).

Rails' throughput barely grows with more apps. On 2 cores its eight Ruby
processes (2 Puma workers + master, 5 Solid Queue) are CPU-bound: 120-170 CPU-ms per 1,000 events, against Rust's
48-75.

**At fixed rates** both sides kept up with every rate offered: 8, 25 and 30
batches/s, which is 4,000, 12,500 and 15,000 events/s.

- One app at 4,000 events/s: Rails p50 75 ms / p99 168 ms, Rust 54 / 115 ms,
  with half the CPU (12.9 vs 22.9 s).
- 10 apps at 12,500 events/s: Rails p99 was 1.07 s, Rust's 0.12 s.
- 50 apps at 15,000 events/s: Rails p99 2.8 s, Rust 0.23 s.
- Rails' p50 at those rates was 107 and 204 ms, with run-to-run spread up to
  0.6-1.5 s. Rails is near its CPU ceiling there (59-76 CPU-s in 45 s on
  2 cores, so 65-85% busy).

This differs from Gate 1, where Rails "could not keep up" at these rates. This
time it could, on a faster CPU.

One Rust outlier: one 4,000 events/s run had a 2 s stall at t=17 s (p99 7.75
s; host IO pressure was 31% at the time). The other two runs had p99 95 and
115 ms. It is reported in the spread, not hidden.

## 2. Concurrent writes to one app, and the mechanism

Senders to one app at 1, 4, 16 and 64 concurrency (`conc*-1app`, and
`max-1app` = 16):

| senders | Rails events/s | Rust events/s | Rails p50 / p99 ms | Rust p50 / p99 ms |
|---:|---:|---:|---|---|
| 1 | 6050 (6033-6583) | 7817 (7650-8717) | 80 (76-83) / 140 (130-149) | 65 (58-65) / 113 (108-120) |
| 4 | 8150 (7833-10233) | 10667 (9200-11000) | 183 (180-192) / 1043 (514-1584) | 195 (193-200) / 326 (296-427) |
| 16 | 9100 (8878-10178) | 13500 (12711-14022) | 844 (782-858) / 2338 (1621-2447) | 639 (557-708) / 943 (902-981) |
| 64 | 10317 (10000-11183) | 22450 (21350-24067) | 3277 (3090-3343) / 4832 (4561-5801) | 1550 (1412-1607) / 2260 (1974-2456) |

- With **one sender**, Rails does 6,050 events/s and Rust 7,800. The 1.3x
  there is per-batch cost alone (Ruby parse/map/insert vs Rust), since nothing
  is concurrent.
- From 1 to 64 senders, Rails goes from 6.0k to 10.6k events/s while its p50
  climbs from 80 ms to 3.2 s.
- Rust goes from 7.8k to 23k, and its p50 grows to 1.5 s: with 64 batches in
  flight, closed-loop queueing is the arithmetic. Rust's fsyncs per batch
  fall from 4.0 to 0.2.

**What the writers actually do** (`mechanism.sh`; Rust with the writer's
debug log on, one line per SQLite transaction):

| Rust, 1 app | transactions | batches | batches per transaction | max in one transaction |
|---|---:|---:|---:|---:|
| 16 senders, rep 1 | 121 | 936 | 7.74 | 13 |
| 64 senders, rep 1 | 48 | 1475 | 30.73 | 33 |
| 16 senders, rep 2 | 123 | 977 | 7.94 | 14 |
| 64 senders, rep 2 | 46 | 1369 | 29.76 | 40 |
| 16 senders, rep 3 | 109 | 857 | 7.86 | 15 |
| 64 senders, rep 3 | 51 | 1346 | 26.39 | 40 |

Rails' `Ingest::Batch#write!` is one `BEGIN IMMEDIATE ... COMMIT` per HTTP
request. Ten Puma threads across two processes contend for the tenant file's
write lock. The sqlite3 gem's busy handler retries in 1 ms sleeps, with the
GVL released, so a waiting request costs little CPU but its latency queues.

Rust's per-tenant writer thread takes whatever is queued (up to 64 batches or
20,000 rows) and commits it as one transaction, with a SAVEPOINT per batch.
That buys three things:
- fewer commits and WAL frame headers;
- index pages written once per group instead of once per batch: Rails wrote
  18 MiB to the device per batch at 16 senders, Rust 10 MiB;
- no lock convoy.

**Is it fsync?** Not here. With WAL and `synchronous=NORMAL`, SQLite fsyncs
the WAL at checkpoints, not at every commit. Rails measured 1.3-2.9 fsyncs per
batch and Rust 0.2-4. Turning sync off entirely at 16 senders to one app:

| 1 app, 16 senders, 30 s | events/s | p50 ms | fsyncs |
|---|---:|---:|---:|
| rails, synchronous=NORMAL (45 s matrix runs) | 9100 (8878-10178) | 844 (782-858) | 1125 (1039-1223) |
| rails, synchronous=OFF | 9750 (8450-10233) | 674 (630-745) | 0 (0-0) |
| rust, synchronous=NORMAL (30 s mechanism runs) | 15600 (14283-16283) | 581 (517-585) | 686 (634-701) |
| rust, synchronous=OFF | 16950 (13983-17217) | 401 (376-509) | 13 (13-13) |

Neither side changes much. Rust goes from 15.6k to 17.0k events/s (+9%),
Rails from 9.1k to 9.75k (+7%; its sync-on figure is from the 45 s runs).
So on a disk with a 3 ms flush, the gap is CPU per batch plus lock serialization, not fsync. On a slower-flush disk,
which is the Gate 1 Hetzner box, coalescing also saves fsyncs, because
checkpoints happen per transaction, and there the gap should be wider. That
was not re-measured here.

## 3. Memory and disk

| case | side | idle anon MiB | anon after load MiB | telemetry bytes / 1M events | device bytes written / 1M events |
|---|---|---:|---:|---:|---:|
| max-1app | rails | 419 | 1365 | 700 MiB | 36409 MiB |
| max-1app | rust | 11 | 127 | 700 MiB | 20015 MiB |
| max-10app | rails | 454 | 1206 | 849 MiB | 19900 MiB |
| max-10app | rust | 18 | 948 | 697 MiB | 19503 MiB |
| max-50app | rails | 460 | 1493 | 1000 MiB | 6979 MiB |
| max-50app | rust | 44 | 1957 | 980 MiB | 11956 MiB |
| rate-1app | rails | 475 | 936 | 641 MiB | 31601 MiB |
| rate-1app | rust | 20 | 135 | 630 MiB | 26524 MiB |
| rate-10app | rails | 448 | 1212 | 827 MiB | 18809 MiB |
| rate-10app | rust | 12 | 520 | 845 MiB | 15816 MiB |
| rate-50app | rails | 496 | 1392 | 1041 MiB | 6262 MiB |
| rate-50app | rust | 34 | 1098 | 1549 MiB | 5757 MiB |
| conc1-1app | rails | 464 | 844 | 633 MiB | 31762 MiB |
| conc1-1app | rust | 13 | 154 | 604 MiB | 28598 MiB |
| conc4-1app | rails | 461 | 1151 | 795 MiB | 32813 MiB |
| conc4-1app | rust | 8 | 206 | 609 MiB | 26099 MiB |
| conc64-1app | rails | 462 | 1293 | 741 MiB | 34097 MiB |
| conc64-1app | rust | 4 | 143 | 756 MiB | 9256 MiB |

- **Idle:** Rails' web and jobs hold **420-500 MiB** of private memory
  (2 Puma workers plus master, 5 Solid Queue processes, Thruster). Rust holds
  **4-44 MiB**: 4-20 MiB with 1-10 apps, 34-44 MiB with 50.
- **After load:**
  - With 1 app, Rust settles at about 130-210 MiB, Rails at 0.85-1.4 GiB.
  - At 50 apps, Rust's private memory after max load (about 1.96 GiB) and its
    peak (1.78 GiB median) are **higher than Rails'** (1.49 / 1.47 GiB). The
    cause is 50 open tenant writers, each with its own page cache and
    in-memory rollup accumulators. It is the cost Rust pays for keeping 50
    writers hot. A self-hoster with 1-10 apps never sees it, but a
    multi-tenant cloud does (see [PERF.md](PERF.md) and the memory track).
- **Disk bytes per million events** (telemetry files after the run):
  - 1 and 10 apps: Rails 630-850 MiB, Rust 600-850 MiB, **about the same**.
    The schema and indexes are the same; Rust's per-day layout neither saves
    nor costs much at this size.
  - At 50 apps and a fixed rate, Rust's files were larger: 1,549 vs 1,041
    MiB per million events. Each environment had little data, and per-day
    files carry fixed overhead per file (meta, core and detail, each with
    WAL/SHM until checkpoint).
- **Bytes written to the device** per million events, a proxy for write
  amplification and SSD wear: with 1 app Rails wrote 1.1-3.7x what Rust
  wrote (36 vs 20 GiB at max load, 34 vs 9 GiB at 64 senders). With 50 apps the order flips at max load
  (Rails 7.0, Rust 12.0 GiB per million events), because Rust's WAL
  checkpoints across more files.

## 4. Dashboard read latency

Same data, same user, same file: Rails ingested 1,500 batches (750,000
records, spread over 7 days) through its own `/ingest`. Its jobs settled every
hour's rollups and grouped the exceptions. Then the whole install was imported
into Rust with `railwatch import-rails`, so both read **one identical
415 MiB SQLite file**: 53,269 executions, 421,804 queries, 34,628 rollups.

Each page is fetched over HTTP the way the dashboard navigates: an Inertia XHR
with the session cookie from signing in through each app's own `/sign_in`.
Every response is checked to be a 200 Inertia page, and the payloads came out
the same size within 50 bytes. "Cold" means the first request to that page
after a server restart and `drop_caches`; "warm" is the median of 10 more.
The overview is the exception: pages.py renders it once, untimed, to read the
Inertia asset version, so its "cold" is the second request and its files are
already in the page cache. Later pages run after the earlier pages' samples,
without another `drop_caches`.
Three repetitions, `window=24h`, both servers in the same 2-core slice.

| page | Rails cold ms | Rust cold ms | Rails warm ms | Rust warm ms |
|---|---:|---:|---:|---:|
| overview | 67.8 (63.3-71.6) | 2.2 (2.2-2.4) | 36.9 (32.5-37.1) | 2.0 (1.8-2.0) |
| requests | 58.9 (58.3-62.0) | 1.2 (1.1-1.2) | 17.4 (16.9-17.7) | 1.0 (1.0-1.1) |
| jobs | 44.6 (44.5-46.2) | 6.4 (6.4-6.7) | 15.6 (14.8-17.1) | 1.9 (1.8-2.0) |
| queries | 341.3 (340.8-342.7) | 21.7 (20.8-41.8) | 16.1 (14.8-18.3) | 15.4 (15.3-16.5) |
| exceptions | 33.6 (33.2-38.9) | 4.0 (3.9-4.0) | 13.9 (12.9-14.2) | 2.0 (2.0-2.1) |

- Rust is 7-50x faster on four of the five pages, cold and warm.
- **Queries, warm, is a tie** (16 vs 15 ms). Rails serves the grouped
  rollups from its 1-minute `Rails.cache` (Solid Cache), as production does.
  Rust recomputes them every time and still matches.
- Cold, the queries page cost Rails 341 ms, against Rust's 22 ms.
- Every Rails page is fast enough for a person. Its slowest warm page is 37
  ms; the 341 ms cold queries page is the only one a user would notice.
- Rails' "cold" also includes first-hit work in a fresh Puma worker
  (autoloaded constants, prepared statements), and the 10 warm requests may
  alternate between two workers. That is what a user sees after a deploy, but
  it is not purely SQLite.
- What this does **not** cover: Rust's normal storage is per-day files
  ([STORAGE.md](STORAGE.md)), and a long window reads several of them. That
  layout's page timings vs a single file are in `bench/storage/pages.sh`.
  This section compares the servers on the identical file.

## 5. The counterfactual: could Rails have closed the gap?

The single biggest difference above is the writer: Rust coalesces, Rails
commits per request. So I wrote the Rails version of it
([`rails/zz_bench_group_commit.rb`](../bench/rails-vs-rust/rails/zz_bench_group_commit.rb),
about 120 lines, mounted as an initializer into the unchanged production
image):

- a leader/follower group commit per environment, inside each Puma process;
- the leader writes up to 64 queued batches in one transaction with a
  SAVEPOINT each, commits, and hands each request its result;
- leadership passes on after every group;
- parsing, mapping and validation still run on each request's own thread
  before it queues;
- plus `temp_store=MEMORY`, which railwatch-rs needed for the same
  savepoint pattern.

The same `run.sh` measured it, 3 repetitions per case:

| case | side | events/s | p50 ms | p99 ms | CPU ms / 1k events | peak anon MiB | batches per transaction (group commit) |
|---|---|---:|---:|---:|---:|---:|---:|
| max-1app | rails | 9100 (8878-10178) | 844 (782-858) | 2338 (1621-2447) | 167 (156-168) | 1083 (1053-1225) | 1 (by construction) |
| max-1app | rails + group commit | 8522 (8189-8878) | 916 (873-933) | 1790 (1644-2219) | 154 (152-174) | 1353 (1332-1361) | 2.00 (1.91-2.04) |
| max-1app | rust | 13500 (12711-14022) | 639 (557-708) | 943 (902-981) | 69 (62-74) | 294 (291-353) | - |
| max-10app | rails | 13867 (12000-14144) | 1068 (1067-1378) | 2649 (2175-3118) | 118 (110-136) | 1184 (1168-1206) | 1 (by construction) |
| max-10app | rails + group commit | 12689 (11956-13800) | 1206 (1097-1382) | 2164 (2051-2283) | 128 (115-139) | 1208 (1164-1217) | 1.00 (1.00-1.00) |
| max-10app | rust | 29722 (29711-30222) | 522 (522-532) | 965 (963-1051) | 61 (60-61) | 568 (477-583) | - |
| max-50app | rails | 17178 (16756-20378) | 1742 (1568-1875) | 3466 (3004-3722) | 121 (95-121) | 1467 (1457-1583) | 1 (by construction) |
| max-50app | rails + group commit | 18856 (16733-19311) | 1676 (1615-1873) | 3562 (3086-3969) | 103 (97-119) | 1497 (1463-1544) | 1.00 (1.00-1.00) |
| max-50app | rust | 41356 (41344-44900) | 763 (689-784) | 1375 (1169-1936) | 48 (46-50) | 1784 (1763-1841) | - |
| rate-10app | rails | 12489 (12044-12489) | 107 (60-610) | 1074 (980-1985) | 106 (101-132) | 1154 (1150-1180) | 1 (by construction) |
| rate-10app | rails + group commit | 12100 (11844-12489) | 702 (60-774) | 1978 (1168-2032) | 122 (100-130) | 1141 (1024-1150) | 1.00 (1.00-1.00) |
| rate-10app | rust | 12489 (12489-12489) | 42 (38-47) | 115 (99-187) | 55 (48-61) | 525 (511-525) | - |
| conc64-1app | rails | 10317 (10000-11183) | 3277 (3090-3343) | 4832 (4561-5801) | 168 (163-170) | 1102 (1069-1102) | 1 (by construction) |
| conc64-1app | rails + group commit | 9650 (9483-10200) | 3436 (3391-3456) | 5611 (5169-6301) | 171 (154-190) | 1361 (1342-1374) | 1.82 (1.76-1.91) |
| conc64-1app | rust | 22450 (21350-24067) | 1550 (1412-1607) | 2260 (1974-2456) | 54 (53-57) | 378 (374-456) | - |

It does not close the gap, for reasons a Rails developer can check:

1. **There is almost nothing to coalesce per process.**
   - With one app and 16 senders, groups averaged **2.0 batches**; at 64
     senders, 1.9.
   - With 10 or 50 apps, exactly **1.0**. Ten threads spread over many
     tenants rarely land on the same one at the same moment.
   - Rust has *all* of a tenant's batches in one process. Production Rails
     has 2 Puma workers, so at most half of them, and fewer threads than
     senders.
2. **The batch's cost was never the commit.** Rails spends 120-170 CPU-ms per
   1,000 events, mostly in Ruby: JSON parse, the mapper building a hash per
   row, binding values one at a time. The GVL means each Puma process maps one
   batch at a time, so 2 processes on 2 cores is the ceiling. Coalescing
   commits cannot buy back CPU spent before the commit. The sync-off run above
   says the same thing from the other direction.
3. **Measured, it changed nothing that matters.**
   - Throughput moved between -8% and +10% across the five cases, inside
     the run-to-run spread.
   - CPU per 1,000 events moved 154-171 ms against 118-168 ms stock.
   - Fsyncs per batch halved with one app (1.34 to 0.58), and that bought
     nothing. This agrees with the synchronous=off result: fsync was not the
     limit.
   - Latency got slightly worse at 10 apps and a fixed rate (p50 107 to 702
     ms median). The leader also does follower batches' writes while their
     own threads wait.

What would close the gap in Rails is a different architecture, and it isn't a
small change:

- One writer process per tenant set: something like a Solid Queue job per
  tenant fed by a queue, which adds a hop and durability questions for the
  queue itself.
- Mapping and inserting moved out of Ruby into a C extension, at which point
  much of the server is no longer Rails.

My estimate is that a dedicated in-process writer thread (the Rust shape,
in Ruby) would help the 1-app concurrency case by at most the
synchronous-off margin, about 10%. It would not touch the 50-app CPU
ceiling. That is an estimate, not a measurement.

## Caveats

- **Shared, busy machine.** The load average was 7-36 during the runs. The
  server cores were isolated, but memory bandwidth, the L3 and the kernel are
  shared. Spread is reported for every number. Rust's 4,000 events/s p99
  outlier (7.75 s) and Rails' rate-50app p50 spread (72-1,499 ms) are the
  widest.
- **The disk is synthetic:** RAM plus a 1 ms write delay. It is fair to both
  sides, but it is not a production disk. Real NVMe read latency would hurt
  cold dashboard reads more than it does here (page cache is dropped, yet
  zram reads are fast).
- **Not production hardware.** This is a 2-core cgroup on a Zen 4 laptop
  chip. The Gate 1 VPS (EPYC Rome) was slower per core, and Rails could not
  keep up there at 12,500 events/s.
- **Different storage layouts.** Rust's per-day files are part of what is
  being compared on ingest. Pages are compared on an identical single file.
- **Rails' rollups are completed by a catchup** that production runs every
  5 minutes. Its work is counted; its wall-clock delay is not.
- **The Rails per-environment rate limit was removed** (one Solid Cache write
  saved per request), which helps Rails.
- **What I did not measure:**
  - receipted ingest;
  - TLS (kamal-proxy and Thruster TLS are off);
  - Litestream (Rails production ran it; it would add CPU and IO to Rails);
  - more than 45 s of sustained load;
  - the Gate 1 VPS, which was not re-run;
  - the effect of another tenant's dashboard reads during ingest.

## Rerun it

```sh
cargo build --release -p railwatch-server -p railwatch-load   # or bin/build
# Rails image: railwatch-cloud @ 71020d3, built from a clean export
mkdir -p /tmp/rw-71020d3
git -C ~/src/railwatch-cloud archive 71020d3 | tar -x -C /tmp/rw-71020d3
docker build -t railwatch-cloud-baseline:71020d3 /tmp/rw-71020d3
bench/rails-vs-rust/host.sh up          # sudo: cpuset slice + delayed zram disk
bench/rails-vs-rust/matrix.sh           # ~2 h, appends bench/results/rails-vs-rust/runs.jsonl
bench/rails-vs-rust/mechanism.sh        # batches/txn, synchronous=off on both
bench/rails-vs-rust/counterfactual.sh   # Rails + group commit
bench/rails-vs-rust/pages.sh 1500 3     # dashboard pages on identical data
python3 -I bench/rails-vs-rust/summarize.py
bench/rails-vs-rust/host.sh down
```

Check `lscpu -e=CPU,CORE` first and set `SLICE_CPUS`, `SERVER_CPUS` and
`LOAD_CPUS` to suit your topology.
