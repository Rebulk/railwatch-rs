# How many apps fit on a small box: Rust vs Rails (Track F, 2026-11-14)

The question: on a box the size of a small VPS (2 threads, 4 GiB), how many
realistic apps can one Railwatch server take, Rust vs Rails? Where does each
break, and what does each app cost in memory and disk?

The numbers only mean something next to each other: one machine, one disk,
one load generator, the same 2 threads and 4 GiB for both. The point is the
root cause of each limit, so each side's section leads with the mechanism and
the evidence for it.

Scripts: [`bench/scale/`](../bench/scale/). Raw results:
[`bench/results/scale/`](../bench/results/scale/).

## Short version

| server | apps that fit (largest step that did not break) | breaks at | what breaks it |
|---|---:|---:|---|
| Rust, origin/main `ad5a091` | **250** | 500 | memory: about 6 MiB per active tenant at once, then about 3-5 MiB more per tenant per 10 min, until the 4 GiB cap kills it |
| Rust, PR #90 `f4ad31b` | **250** | 500 | the same; #90 holds p99 down at 250 (34 vs 632 ms) but does not move the limit |
| Rails, railwatch-cloud `71020d3` | **100** | 250 | CPU: both threads pinned during every minute's :00-:15 burst, and for about a minute during each 5-minute rollup catch-up |

- **Rust fits 2.5x as many apps as Rails** on this box (250 vs 100), with the
  typical-app profile from production.
- **The two run out of different things.** Rust runs out of memory with CPU
  at 7-14%. Rails runs out of CPU with memory at 1.1 GiB of 4.
- **Per app, at 10-100 apps:**
  - Rust: 8.9 MiB of private memory, 0.03% of the two threads, 17 file
    descriptors.
  - Rails: 1.4 MiB, 0.23% of the two threads (about 8x Rust), 5 file
    descriptors.
  - Rails starts from about 830 MiB; Rust from about 54 MiB.
- **Latency below each side's limit:** Rust p99 was 8-10 ms at 10-100 apps.
  Rails was 27-39 ms at 10-50 apps and 123 ms at 100.
- **Disk:** a 10-minute step cannot give a per-day figure on either side.
  The writes are mostly WAL, and the database files grow too little to time
  (see Disk). Production's figure for a typical app is 150-181 MiB/day.
  At any moment Rust held about 7.5 MiB of WAL per tenant.

What is not settled yet: best-case Rails (Track G), the next Rust round
(Track H), Rust #89 on top of #90, and the 10%-busy mixes. They are queued;
see [Still running](#still-running).

## Why Rust breaks: a per-tenant memory floor, then growth

**Mechanism.**
- Each tenant that sends data costs about **6 MiB of private memory** the
  moment it becomes active.
- After that, memory **keeps growing by 3.5-5 MiB per tenant over a 10-minute
  step** without levelling off.
- At 500 apps, the floor alone is about 3.3 GiB, and the growth pushes the
  server into the 4 GiB cap within 8-9 minutes. The kernel then kills it.
- CPU was never the limit: 13.6% of the two threads at 500 apps.

**Evidence** (framework, steady disk; `steps.jsonl`, `raw/*.samples` at 2 s):

- **The floor arrives with the first batches, not with the data.** Private
  memory just before the load starts, and then 4-10 s in:

  | apps | before load (MiB) | 4-10 s in (MiB) | per app (MiB) |
  |---:|---:|---:|---:|
  | 100 | 83 | 569-579 | about 5.0 |
  | 250 | 172 | 1,437-1,457 | about 5.1 |
  | 500 | 307 | 3,295-3,343 | about 6.1 |

  - At that point each app has sent only a few health pings, so this is a
    fixed cost of a tenant being open, not of what it stored.
  - #90 shows the same jump: 310 to 3,005 MiB at 500 apps, 5.4 MiB per app.
- **It is per tenant.**
  - Private memory fits 54 MiB + 8.91 MiB per app from 10 to 100 apps
    (152, 269, 496 and 948 MiB).
  - Open file descriptors rise by exactly **17.0 per app** (185 at 10 apps,
    8,630 at 500).
  - What they are: a listing of the Rust process's descriptors at 100 apps
    (`/proc/<pid>/fd`, the 10%-busy step) showed per tenant **3 connections
    to `meta.sqlite3`** and **2 to the day's `core` file**, each with its
    `-wal` (and one `-shm` per file). 39 of the 100 tenants also had the day's
    `detail` file open twice. That is about 14 descriptors per tenant, plus a
    socket each.
  - So each active tenant keeps about 5 SQLite connections open, and each
    connection has its own page cache and prepared statements. That fits the
    5-6 MiB per tenant on arrival. **Which part of the 5-6 MiB is page cache
    versus statements and rollup state was not measured**; that split is
    Track H's subject.
- **Then it grows without settling.** Growth over the last 5 minutes of each
  step:

  | apps | main (MiB/min) | #90 (MiB/min) |
  |---:|---:|---:|
  | 100 | +35 | +30 |
  | 250 | +150 | +154 |

  Rails at 100 apps grew +10 MiB/min. Ten minutes is too short to say where
  Rust would level off, if it does.
- **The break is memory, and only memory.**
  - At 250 apps, main's total memory (private memory plus page cache) reached
    the 4 GiB cap halfway through the step. The cgroup recorded 6,902
    cap-hit events and 353 throttled sockets.
  - Health pings then took up to 2 s, which is where the 632 ms p99 comes
    from.
  - At 500 apps the kernel killed `railwatch` at about 540 s, with 4.1 GB of
    private memory resident.
  - The disk sync p99 was 1.8-2.1 ms in those steps, and CPU was 7-14%.
  - At 100 apps, before the cap, health pings had p99 5 ms and max 35 ms;
    at 250 they had p99 599 ms and max 2,048 ms.

**What #90 changes.**
- #90 caps readers and rollup accumulators across tenants, and closes idle
  connections.
- At 250 apps it reached the cap later than main (about 66% of the way through
  the step against about 50%), with about half as many cap hits (3,728). Its
  p99 was 34 ms against 632 ms, and steady memory was 2,100 MiB against 2,507.
- It does not lower the floor: 5.4 MiB per app on arrival, against main's
  5.1-6.1.
- At 500 apps it was killed at about 460 s, against main's 540 s. With both
  sitting a few hundred MiB under the cap from the first minute, I read the
  earlier kill as chance in a single run, not as #90 being worse. One run
  each.
- What would raise the limit is a smaller floor per open tenant. That is
  Track H's subject; its root-cause analysis will be linked here when it
  lands.

## Why Rails breaks: CPU, in two kinds of burst

**Mechanism.**
- Rails' web and job processes share the two threads.
- At 250 apps they run out of CPU twice over:
  - **every minute**, during the :00-:15 burst in production's traffic;
  - **every 5 minutes**, for about a minute, when `RollupCatchupJob` enqueues
    two `RollupJob`s per active environment (the current and the previous
    hour) and the job workers run all of them.
- Requests queue behind both. Nothing was dropped, but p99 reached 12.5 s.

**Evidence** (framework, steady disk, Rails 250 apps):

- **Both threads were pinned in bursts, not on average.**
  - CPU averaged 69% of the two threads over the step.
  - It swung between 30-55% and 95-108% in a one-minute cycle: up for 20-30 s
    starting at each minute's :00, down for the rest.
  - 43 of the 60 ten-second windows had a p99 above 1 s; the worst were
    during the pinned stretches.
- **The minute burst is in the traffic, not in Rails.** In the production
  profile, the first three 5-second buckets of each minute carry 31% of data
  batches and 1.4-1.9x the records per batch (rebulk-system;
  `minute_weights` and `minute_size_mult` in `profiles.json`). It is what
  production's own scheduled tasks produce. Rust takes the same bursts at
  250 apps with p99 34 ms (#90).
- **The 5-minute storm is the rollup catch-up.**
  - At t≈130 s and t≈430 s, exactly 300 s apart, Solid Queue's ready queue
    jumped from 0 to 493 and to 677 jobs.
  - Both threads stayed at 100-107% for about 60 s while it drained.
  - Those two minutes hold the 10-13 s latencies.
  - `RollupCatchupJob` (railwatch-cloud `71020d3`,
    `app/jobs/rollup_catchup_job.rb`) runs every 5 minutes. It enqueues
    `RollupJob.perform_later(env, now)` and `(env, now - 1.hour)` for every
    environment seen in the last 2 hours: 500 jobs for 250 apps.
  - The job classes behind each burst are recorded per step from the 10%-busy
    Rails sweep on (`job_mix`). The 250-app numbers above come from the queue
    depth and timing, not from a class count.
- **It is not memory.** Rails' private memory was 1,064 MiB steady at
  250 apps, and grew at about 1.4 MiB per app from 10 to 100 apps. It never
  came near the cap: no cap hits, no kills.
- **The CPU cost per app rises before the break:**

  | apps | CPU (% of 2 threads) | per app |
  |---:|---:|---:|
  | 10 | 2.7% | 0.27% |
  | 25 | 5.0% | 0.20% |
  | 50 | 9.2% | 0.18% |
  | 100 | 23.0% | 0.23% |
  | 250 | 68.9% | 0.28% |

  It rises because the catch-up's work grows with the number of environments,
  and each `RollupJob` recomputes up to an hour of one environment.
- Track G is building the best case (Rails 8.1.4, tuned Puma, a dedicated
  writer process). Its root-cause analysis will be linked here when it lands.

## Results: steady disk (framework)

Typical-app profile unless marked. Each step is a fresh server and fresh data,
10 minutes of load, and latency counted from t = 60 s.
- "rest busy" is how busy the CPUs outside the bench were (0-5 and 12-17),
  where Track G and Track H work. "load max" is the highest 1-minute load
  average during the step.
- "private" is the cgroup's anonymous memory, the same measure on both sides.
- Rails' RSS sums the two Puma workers, their master, Thruster and the Solid
  Queue processes (two containers).
- No Rails step had a failed job, and every Rails step ended with 0 ready jobs.
- The tables are generated by `bench/scale/tables.py` from
  [`steps.jsonl`](../bench/results/scale/steps.jsonl); every field there is
  per step.

### Rust, origin/main `ad5a091`

| apps | p50 ms | p99 ms | not 200 | CPU % of 2 threads | steady / peak private MiB | RSS MiB | fds | rest busy | load max | breaks |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 10 | 1.5 | 8.0 | 0 | 0.3 | 152 / 164 | 187 | 185 | 2.2% | 2.6 | - |
| 25 | 1.5 | 9.0 | 0 | 0.7 | 269 / 309 | 327 | 436 | 2.3% | 0.8 | - |
| 50 | 1.5 | 8.8 | 0 | 1.4 | 496 / 554 | 592 | 861 | 5.4% | 1.8 | - |
| 100 | 1.6 | 9.8 | 0 | 2.8 | 948 / 1,073 | 1,115 | 1,717 | 2.0% | 0.6 | - |
| 250 | 1.7 | 632.3 | 0 | 7.0 | 2,507 / 2,746 | 2,859 | 4,272 | 3.3% | 2.1 | - |
| 500 | 1.9 | 1,665.4 | 7,153 | 13.6 | 3,767 / 3,997 | 3,883 | 8,630 | 2.3% | 22.6 | p99 > 1 s, drops, killed (out of memory) |

### Rust, origin/main `ad5a091`, 10% busy apps

| apps | busy apps | p50 ms | p99 ms | not 200 | CPU % of 2 threads | steady / peak private MiB | RSS MiB | fds | rest busy | load max | breaks |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 10 | 1 | 1.2 | 11.7 | 0 | 0.3 | 150 / 172 | 180 | 185 | 18.7% | 5.9 | - |
| 25 | 3 | 1.2 | 11.8 | 0 | 0.9 | 318 / 331 | 370 | 441 | 19.3% | 5.2 | - |
| 50 | 5 | 1.1 | 11.1 | 0 | 1.6 | 535 / 611 | 629 | 866 | 19.3% | 6.4 | - |

### Rust, PR #90 `f4ad31b` (perf/memory)

| apps | p50 ms | p99 ms | not 200 | CPU % of 2 threads | steady / peak private MiB | RSS MiB | fds | rest busy | load max | breaks |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 10 | 1.5 | 7.9 | 0 | 0.3 | 139 / 157 | 171 | 185 | 2.0% | 4.2 | - |
| 25 | 1.5 | 8.6 | 0 | 0.7 | 278 / 305 | 335 | 441 | 2.1% | 0.6 | - |
| 50 | 1.4 | 8.6 | 0 | 1.3 | 469 / 524 | 562 | 861 | 8.6% | 3.3 | - |
| 100 | 1.1 | 8.6 | 0 | 2.4 | 878 / 974 | 1,036 | 1,713 | 22.2% | 12.7 | - |
| 250 | 1.6 | 34.1 | 0 | 6.8 | 2,100 / 2,675 | 2,455 | 4,267 | 10.5% | 6.1 | - |
| 500 | 1.9 | 1,822.1 | 19,009 | 11.5 | 3,511 / 4,014 | 3,785 | 8,500 | 6.7% | 13.8 | p99 > 1 s, drops, killed (out of memory) |

### Rails, railwatch-cloud `71020d3` (the image production last ran)

| apps | p50 ms | p99 ms | not 200 | CPU % of 2 threads | steady / peak private MiB | RSS MiB | fds | oldest ready job s | rest busy | load max | breaks |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 10 | 7.1 | 29.9 | 0 | 2.7 | 830 / 840 | 1,492 | 313 | 0 | 19.3% | 5.0 | - |
| 25 | 7.3 | 27.3 | 0 | 5.0 | 887 / 912 | 1,504 | 511 | 1 | 19.1% | 4.6 | - |
| 50 | 7.4 | 38.5 | 0 | 9.2 | 900 / 940 | 1,592 | 845 | 3 | 17.2% | 5.0 | - |
| 100 | 12.6 | 122.9 | 0 | 23.0 | 973 / 1,048 | 1,641 | 804 | 10 | 19.0% | 5.7 | - |
| 250 | 1,477.6 | 12,549.6 | 0 | 68.9 | 1,064 / 1,145 | 1,733 | 1,499 | 44 | 24.9% | 10.9 | p99 > 1 s |

In the 10%-busy mixes, every tenth app sends the busy profile
(rebulk-system's shape, about 4.7x a typical app's records).

## Load profile: derived from production, read-only

Source: production (5.161.187.226), each tenant's `core/2026-11-13.sqlite3`
(a full UTC day), table `ingest_batches` plus per-type `count(*)`.
- The production host has no `sqlite3` binary, so the queries ran in
  `python3 -I` with `sqlite3.connect("file:...?mode=ro", uri=True)`:
  `SELECT count(*)`, `sum()` and `GROUP BY` over `received_at` only.
- Nothing was copied off the server. Request and job counts were
  cross-checked with the Railwatch MCP (`query_requests`, `list_jobs`).

| tenant | events / day | batches / day | health-only batches | data batches | data batch size p50 / p90 / p99 / max | events / min p50 / p90 / max |
|---|---:|---:|---:|---:|---|---|
| rebulk-system production (busy) | 1,181,309 | 73,296 | 48,648 | 24,648 | 20 / 92 / 523 / 2,913 | 434 / 1,707 / 6,043 |
| rebulk-crm production (typical) | 253,362 | 24,019 | 22,895 | 1,124 | 63 / 575 / 616 / 2,965 | 16 / 696 / 3,852 |
| rebulk-crm customer | 23,766 | 23,093 | 23,037 | 56 | 6 / 28 / 44 / 46 | 16 / 16 / 201 |
| lantern-cloud production | 23,037 | 23,037 | 23,037 | 0 | - | 16 / 16 / 19 |

**The mix.** What production sends, per day (2026-11-13):
- rebulk-system:
  - query 812,420; cache_event 116,706; transaction 62,744; health 51,844;
    broadcast 43,002;
  - request 34,588; n_plus_one 15,663; outgoing_request 13,033;
    view_render 7,195; session 6,770; log 6,479; scheduled_task 5,570;
  - visit 2,359; job_attempt 836; enqueued_job 777; and fewer than 500 each
    of the rest.
- rebulk-crm production:
  - query 178,310; transaction 29,253; health 23,040; n_plus_one 6,228;
    outgoing_request 6,060; cache_event 4,523;
  - job_attempt 2,226; enqueued_job 1,741; scheduled_task 1,281; log 547.
- Queries are 69-70% of all records on both.
- Logs are far fewer than the bench in Track C assumed: 0.5% of records
  against its 2 per execution.

**Burst pattern.** Two kinds:
- **Every minute:**
  - In rebulk-system, the 5-second buckets at :00, :05 and :10 carry 10.5%,
    12.4% and 8.3% of the day's data batches, against 7.1-8.1% for the other
    nine buckets. Their batches are 1.4-1.9x the mean size.
  - rebulk-crm production's bursts are sharper: 40% of its data batches land
    at :00-:05.
- **Hourly:** rebulk-system's busiest hour was about 2.1x its median hour
  (84,891 vs 40,456 events), around 15:00 UTC.

**The two profiles** (`bench/scale/profiles.json`):
- **Typical app = rebulk-crm production.**
  - A 1-record `health` batch every 3.75 s.
  - 0.78 data batches per minute as a Poisson process, weighted by
    production's 12 five-second buckets.
  - Batch sizes drawn from its 101 size quantiles, scaled per bucket.
  - Each execution is filled with rebulk-crm's per-type counts per execution.
- **Busy app = rebulk-system production**, the same way: a health batch every
  1.67 s, 17.1 data batches per minute.

**rw-load `--profile`** (`crates/load/src/scale.rs`):
- Every app runs on its own schedule, open loop: a batch is sent when it is
  due, whether or not the previous one was answered. A slow server shows as
  latency and requests in flight, not as a lower offered rate.
- A send not answered within 60 s counts as dropped.
- Delivered load matched the profile: at 25 apps the typical profile stored
  43,738 records in 600 s, which is 225-252k records per app per day against
  rebulk-crm's 253k.

## Disk per app: not measurable in a 10-minute step

**What the data shows:** a 10-minute step is too short to give a
per-app-per-day disk figure for either side. I am not reporting one.

- **Most of what is written is WAL.** At the end of each step, the `-wal` and
  `-shm` files held:

  | | WAL + shm at the end | database files at the end |
  |---|---:|---:|
  | Rust, 10-500 apps | 7.0-7.6 MiB **per app** | 19-1,009 MiB |
  | Rails, 10-50 apps | 4.5-5.6 MiB per app | 16-72 MiB |
  | Rails, 250 apps | 0.5 MiB per app | 378 MiB |

  A WAL is reused once it reaches its checkpoint size, so it is a fixed cost
  per open file, not growth. Rust keeps several files per tenant (meta, and the
  day's core and detail), each with its own WAL. That is why its WAL total
  scales with apps and holds near 7.5 MiB each. Rails checkpoints more as it
  gets busier, so its WAL total shrinks at 250 apps.
- **The database files themselves grow too little to time.**
  - Rust's database files grew 1.0-7.1% over the second half of each step.
  - Second-half growth per record came out 126-252 bytes on Rust and 524-707
    on Rails.
  - Whole-run growth per record came out 1,177-1,326 on Rust and 848-924 on
    Rails.
  - The two methods disagree by 2-10x and in opposite directions between the
    sides. The whole-run figure includes each new tenant's empty files and
    their first pages. The second-half figure depends on when SQLite moved
    pages from the WAL into the main file.
  - Neither is a rate I would extrapolate to a day.
- **What a day costs is better read from production:**
  - rebulk-crm production, the typical-app shape, writes 150-181 MiB/day per
    tenant (2026-11-07 to 2026-11-13, core + detail, from `ls -l`).
  - rebulk-system writes 569-881 MiB/day.
  - Both are on Rust 0.2.18, ext4. Production has no Rails tenant to compare.
- **The bench's records are also smaller than real ones.** They are built from
  the gem's small wire fixtures, so their SQL text, log lines and payloads are
  shorter than a real app's. So even a clean rate here would understate
  production.
- **To measure it properly:** run one step at a fixed app count for several
  hours, and read the database files after a `wal_checkpoint(TRUNCATE)`. That
  would also settle whether Rust's per-day files cost more than Rails' single
  file at many small tenants. Track C saw 1,549 against 1,041 MiB per million
  events at 50 apps; not re-measured here. This is a TODO.

What the data does show about disk: **at the same app count, Rust held about
7.5 MiB of WAL per tenant at any moment, and Rails 0.5-5.6 MiB.** On a host
sized close to its disk, that is real space: about 1.9 GB of WAL at 250 apps
for Rust. It is not counted as growth.

## The shared-disk run: rebulk, before the move

The sweep first ran on rebulk (AMD 9955HX), with the data directory on a
loop-mounted ext4 image on rebulk's shared btrfs root. Other people's work ran
on the other cores.

| side | apps | p99 ms | not 200 | CPU % | steady private MiB | breaks |
|---|---:|---:|---:|---:|---:|---|
| Rust main | 10 | 25.9 | 0 | 0.2 | 142 | - |
| Rust main | 25 | 35.1 | 0 | 0.5 | 262 | - |
| Rust main | 50 | **2,500.5** | 0 | 0.8 | 588 | p99 > 1 s |
| Rust #90 (local `e2b7892`) | 10 | 25.3 | 0 | 0.2 | 121 | - |
| Rust #90 (local `e2b7892`) | 25 | 447.0 | 0 | 0.5 | 326 | - |

**It was the disk.** The same Rust main binary, at the same 50 apps on the
steady disk on framework, had a p99 of **8.8 ms**. On rebulk:
- At 50 apps:
  - Even 1-record health pings had a p99 of 2,379 ms and a max of 8,106 ms.
  - The server's own cgroup showed CPU pressure and IO pressure of 0.00.
  - Host IO pressure (`full avg10`) reached 12.6% while sampled.
- At 25 apps (#90), a per-second 4 KiB write + `fdatasync` on the bench disk
  took p50 8.4 ms, **p99 560 ms and max 7,117 ms**.
  - 3 of the 4 ten-second windows with an ingest max above 1 s had a sync
    above 2 s within 20 s.
  - The fourth was the first 10 s of a fresh server.
- That is what decided the move to a RAM-backed disk with a fixed write delay.
- The raw files stayed on rebulk when it went offline. These rows come from
  the step summaries
  ([`rebulk-shared-disk-2026-11-14.txt`](../bench/results/scale/rebulk-shared-disk-2026-11-14.txt)).

## Incident: rebulk was knocked offline

At about 19:15 CT on 2026-11-14, stopping the rebulk sweep took rebulk off the
tailnet.

- The stop command looked the runner up by matching its name, and that also
  matched the shell running the command.
- It returned the wrong process group (954), and SIGTERM was sent as root to
  that whole group without checking what it was.
- The ssh session dropped at once. Since then rebulk shows offline in
  tailscale and does not answer ping or ssh.
- A low-numbered process group on that box is a long-running system service,
  most likely `tailscaled` or `sshd`. A clean exit on SIGTERM does not trigger
  `Restart=on-failure`, so it did not come back.
- It needs a fix at rebulk's console. Which service it was is not confirmed.
- **What the bench may have left there:** the sweep runner may still be
  cycling, confining other slices to CPUs 0-11,16-27 during each sweep. Each
  sweep's trap releases that, and so does a reboot.

**Changed since then:**
- No signal goes to a process group.
- No PID is signalled unless the harness wrote it or started it in the same
  command.
- No `pkill` or `pgrep` runs as root.
- Every long runner is a `systemd-run --unit=rw-scale-*` unit and is stopped
  only with `systemctl stop`.
- The harness signals only the two sampler processes it starts itself, by the
  PID it recorded.

## Machine and noise

**framework** (all the steady-disk results):
- AMD Ryzen AI 9 HX 370: 12 cores, 24 threads; CPU n and n+12 share a core.
- 30 GB RAM, btrfs root, Linux 7.2, Docker 29.7.2 (cgroup v2, systemd driver).
- **Server under test:** `bench-server.slice`, CPUs **11 and 23** (one
  physical core and its SMT sibling, "2 threads"), `MemoryMax=4G`, no swap.
  Rails' web and jobs containers share it; Rust is one process.
- **Load generator:** `bench-load.slice`, CPUs 6-10 and 18-22 (cores 6-10).
- **Other work:** Track G (`rwg-*`) and Track H (`rwh-*`) worked on CPUs 0-5
  and 12-17 from about 21:30 on.
  - Those CPUs were 2-9% busy for Rust main and #90's first three steps.
  - They were **17-25% busy** for every Rails step, for #90 at 100 apps, and
    for the mixed sweeps.
  - The highest load average during a step was 22.6, during Rust main's
    500-app step; Rails' 250-app step saw 10.9.
  - The server's CPUs were never shared, but memory bandwidth, the L3 and the
    kernel were.
- **Disk:**
  - ext4 on a 10 GB zram device (lz4), behind dm-delay with 1 ms on every
    write bio. Reads are not delayed. It is the disk Track C used.
  - Its 4 KiB `fdatasync`, sampled every second in every step: p50
    0.7-0.9 ms, p99 1.3-2.1 ms; Rails at 250 apps 4.8 ms, when the server's
    own writes were heaviest.
  - The worst single sync per step was 2-36 ms, and above 4 ms in only 6 of
    the 11 steps checked. Against rebulk's shared disk (p99 560 ms, max
    7,117 ms) that is steady.
  - It is fair to both sides, but it is not a real NVMe.

**Breaks**, defined before any run: p99 above 1 s, any send not answered with
200, falling behind for more than 60 s, or out of memory.
- Falling behind means one of three things:
  - Rails' oldest ready job older than 60 s;
  - a post-load tail longer than 60 s;
  - a tail that never went idle.
- No step had a dropped send without also being killed for memory.

## Still running

- **Mixed sweeps, 10% busy apps:** Rust main (in progress), then Rails.
- **Rust #89 on #90:** `ef4b907` merged onto `f4ad31b`. The merge's only
  conflict was `docs/PERF.md`, so this is a local bench-only build. It runs
  after the mixes.
- **Best-case Rails (Track G)** and **Rust next (Track H):** `followup.sh`
  runs them once their image and binary are on framework.
- TODO: rerun the Rust sweeps once Track E (perf/ingest, PR #89) lands on
  main.

## Reproduce

```sh
# binaries (from each tree, separate target dirs)
cargo +1.99.1 build --release -p railwatch-server -p railwatch-load       # perf/scale = origin/main + bench
git worktree add --detach ../rw-membuild f4ad31b && (cd ../rw-membuild &&
  CARGO_TARGET_DIR=$HOME/.cache/rw-scale/target-memory cargo +1.99.1 build --release -p railwatch-server)
mkdir -p ~/.cache/rw-scale/bin && cp target/release/{railwatch,rw-load} ~/.cache/rw-scale/bin/ &&
  mv ~/.cache/rw-scale/bin/railwatch ~/.cache/rw-scale/bin/railwatch-main
cp ~/.cache/rw-scale/target-memory/release/railwatch ~/.cache/rw-scale/bin/railwatch-memory
# Rails image: railwatch-cloud @ 71020d3, as in docs/RAILS-VS-RUST.md
git -C ~/src/railwatch-cloud archive 71020d3 | tar -x -C /tmp/rw-71020d3
docker build -t railwatch-cloud-baseline:71020d3 /tmp/rw-71020d3

# check the topology and set SERVER_CPUS / LOAD_CPUS to one core + sibling / other cores
lscpu -e=CPU,CORE

# everything, as one stoppable unit (sweeps run one at a time; each cleans up after itself)
sudo systemd-run --unit=rw-scale-all --uid=$(id -u) --gid=$(id -g) --setenv=HOME=$HOME \
  --setenv=SWEEPS="rust 0; rust-memory 0; rails 0; rust 10; rails 10" \
  --property=KillMode=mixed --property=TimeoutStopSec=180 --working-directory=$PWD \
  bench/scale/runall.sh
journalctl -u rw-scale-all -f
sudo systemctl stop rw-scale-all           # to stop early

# one step by hand: bench/scale/reserve.sh on; bench/scale/disk.sh up;
#   bench/scale/step.sh rust 50; bench/scale/disk.sh down; bench/scale/reserve.sh off
# on a shared box, SHARED=1 bench/scale/reserve.sh on also confines every other slice
python3 -I bench/scale/stalls.py bench/results/scale/raw/<run>   # disk stalls vs slow windows
```
