# Gate 1: Rust ingest vs Rails cloud (2026-10-13)

## Headline: an 8-core Hetzner VPS (real hardware)

A Hetzner cloud VPS: 8 vCPU AMD EPYC Rome, 15 GiB, ext4 on a virtio disk, shared with a live
workload during the runs. Each server got **2 cores (4-5) and a 3 GiB cap**; the load
generator ran on 6-7. Rails = railwatch-cloud @ 464f0fd production image (2 Puma workers x 5 threads + bin/jobs, both
containers capped together). Rust = commit a976e64 (`temp_store=MEMORY`, CPU semaphore).
45 s per run, one run each. Raw per-run output is not included in this repository (it carries host monitoring logs).

| run | Rust ev/s | Rails ev/s | Rust p50 / p99 | Rails p50 / p99 | Rust peak mem | Rails peak mem (web+jobs) | CPU s Rust / Rails |
|---|---|---|---|---|---|---|---|
| max load, 10 apps | **20,156** | 9,422 | 0.77 / 1.90 s | 1.63 / 3.76 s | 400 MiB | 1,193 MiB | 69 / 76 |
| max load, 50 apps | **30,456** | 13,156 | 0.90 / 2.90 s | 2.34 / 5.78 s | 1,031 MiB | 1,379 MiB | 73 / 92 |
| 4k ev/s, 1 app | 3,989 | 3,989 | **102 / 131 ms** | 153 / 894 ms | **44 MiB** | 1,069 MiB | **19** / 43 |
| 12.5k ev/s, 10 apps | 12,489 | **10,300 (could not keep up)** | **69 / 833 ms** | 5.5 / 9.4 s | 259 MiB | 1,116 MiB | **41** / 83 |
| 15k ev/s, 50 apps | 14,989 | **13,200 (could not keep up)** | **23 / 52 ms** | 4.8 / 6.2 s | 722 MiB | 1,352 MiB | **29** / 92 |

Idle private memory: **Rust 8 MiB** (one process) vs **Rails ~385-440 MiB** (web + jobs, 8 processes).

Read it as:
- **Throughput 2.1-2.3x on 2 cores.** More than on the workstation (1.5x), because on real disk
  Rails' per-batch transaction pays an fsync per request while Rust coalesces queued batches into one.
- **At a fixed rate, Rust uses a third to half the CPU** (19 vs 43 s, 29 vs 92 s) and stays at
  tens of ms; Rails fell behind at 25 and 30 batches/s and queued into seconds.
- **Memory: 25x less at idle, 1.3-24x less under load.** Rust's peak at 50 apps (1 GiB) is mostly
  50 open SQLite connections' page caches (2,000 pages x 4 KiB each, Rails parity), not the server.
- **The co-located workload was not harmed:** its health check answered 200 every 5 s through all
  10 runs, with no 5xx in the window. Host load peaked at 23.7 during the 50-app Rust max-load run
  (CPU contention between the unpinned workload and the bench), back to 0.2 after.
- Caveats: one run each; Rails' CPU includes its rollup jobs, Rust has none yet (Phase 2).

## Workstation runs (earlier, kept for the record)

Same workstation (16 cores, busy: load avg 3-45 from unrelated jobs), server pinned to CPUs 8-11,
load generator to 12-15. rw-load closed loop, 500-record gem-shaped batches, 45 s per run, one run
each (no repeats). Rails = railwatch-cloud @ 464f0fd production image, 2 Puma workers x 5 threads + bin/jobs.
Per-IP limiter on in both; Rails' per-env 600/min limit is hardcoded on, Rust's is off.

## RAM storage (tmpfs both sides) - measures the servers, not this disk

| envs | conc | Rust ev/s | Rails ev/s | Rust p99 | Rails p99 | Rust CPU s | Rails CPU s (web+jobs) | Rust peak anon | Rails peak anon (web+jobs) |
|---|---|---|---|---|---|---|---|---|---|
| 1  | 8  | 11,656 | n/a (hit 600/min env limit: 2,600 x 429) | 449 ms | - | 50 | 33+2 | 92 MiB | 815+409 MiB |
| 10 | 32 | 38,600 | 24,678 | 3,278 ms | 827 ms | 154 | 90+10 | 422 MiB | 843+719 MiB |
| 50 | 64 | 45,133 | 29,067 | 1,574 ms | 1,409 ms | 143 | 91+31 | 885 MiB | 1,425+830 MiB |

Idle private memory: Rust 8 MiB (one process). Rails ~600 MiB anon across web + jobs (8 processes).
CPU per event: Rust 70-95 us, Rails ~90 us. Rust's throughput gain comes from using all 4 cores
(SQLite writes run in parallel across tenants) rather than from cheaper work per event.

## Real disk (encrypted btrfs, 88% full, shared) - see gate1-matrix-disk-2026-10-13.txt

Both sides are fsync-bound here (Rust writer threads sit in wait_log_commit). Rust 1.4-2.4x Rails.
Not representative of a VPS NVMe; see the Hetzner VPS headline above.

## Not yet known
- Where Rust's CPU goes: profiling needs perf_event_paranoid <= 1 (sudo). Suspect SQLite index
  maintenance (same 31-index schema on both sides).
- Rust p99 at 10 envs (3.3 s) is worse than Rails: the coalescing writer queues unfairly under
  saturation. Needs a look before any production claim.
- Rails CPU includes its rollup jobs; Rust has no rollups yet (Phase 2), which flatters Rust slightly.

## Fixes found by profiling (commit a976e64)
- `temp_store=MEMORY`: the per-batch SAVEPOINT made SQLite write statement journals to /var/tmp
  (~35% of server CPU). 50-app throughput on the workstation 45k -> 65k ev/s.
- CPU semaphore around parse+map (one batch per core). The "worse p99" in the first matrix was
  mostly closed-loop queueing; at a fixed rate Rust's p99 is far below Rails'.

## Rerun with the full server (rollups, grouping, watchdog on), 2026-10-14, HEAD 344eebf

Workstation, RAM storage, server on 4 pinned cores, same harness as the RAM matrix above.
Closes the caveat that Rails' CPU included rollup jobs while Rust had none.

| run | before (ingest only, a976e64) | full server | Rails (RAM matrix) |
|---|---|---|---|
| max load, 10 apps | 49,122 ev/s, p99 1.18 s, peak 505 MiB | **51,611 ev/s**, p99 1.04 s, peak 892 MiB | 24,678 ev/s |
| max load, 50 apps | 65,422 ev/s, p99 1.53 s, peak 1,232 MiB | **68,111 ev/s**, p99 0.86 s, peak 1,546 MiB | 29,067 ev/s |
| 12.5k ev/s, 10 apps | p99 21 ms | p99 **27 ms**, 21 CPU s, peak 523 MiB | p99 176 ms |
| 15k ev/s, 50 apps | p99 13.5 ms | p99 **17.5 ms**, 18 CPU s, peak 878 MiB | p99 50 ms |

Rollups are kept incrementally at commit time, so throughput is unchanged (within run noise);
the cost is memory: the in-memory per-(tenant, type, group, hour) accumulators roughly double peak
private memory under load. Idle is 12 MiB (was 8). Raw: bench/results/full-server-ram-2026-10-14.txt.
