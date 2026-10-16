# Rails baseline: railwatch-cloud @ 464f0fd (gem railwatch 0.9.0)

The production image of railwatch-cloud, run locally the way production's
`web` and `job` roles run (config/deploy.yml), as the ingest baseline for the
Rust rewrite. Nothing in the railwatch-cloud repo is modified; the build
context is a clean detached worktree at `/tmp/rw-baseline-wt` and every
difference from production is runtime env, listed below.

## Commands

```sh
cd bench/rails
export RAILWATCH_CLOUD_REPO=$HOME/src/railwatch-cloud   # a railwatch-cloud checkout (needs access)
./up.sh              # build railwatch-cloud-baseline:464f0fd (~75 s cold), start both containers, wait for /up
SKIP_BUILD=1 ./up.sh # reuse the image (~10 s to /up)
./seed.sh 50         # 50 environments -> tokens-50.txt (one rw_ token per line)
./smoke.sh           # one batch of all 28 wire types with tokens-1.txt; prints ack + rows per table
./smoke.sh tokens-50.txt
./down.sh            # remove both containers, the volume, and tokens-*.txt (--image also drops the image)
```

`up.sh` recreates the containers but keeps the volume, so tenants and tokens
survive a restart. Run `./down.sh` first if you want a clean database.

## What runs

| container          | command                                         | role in prod |
|--------------------|-------------------------------------------------|--------------|
| `rw-baseline`      | image default: `./bin/thrust ./bin/rails server` (entrypoint runs `db:prepare` first) | web |
| `rw-baseline-jobs` | `bin/jobs` (Solid Queue: supervisor, dispatcher, 2 workers, scheduler) | job |

Both mount the named volume `rw-baseline-storage` at `/rails/storage`.
Thruster listens on :80 inside the container; Puma is behind it on :3000.
Published at `127.0.0.1:8780`. The image's own env is unchanged:
`RAILS_ENV=production`, `LD_PRELOAD=/usr/local/lib/libjemalloc.so` (verified
mapped in every process), `BUNDLE_DEPLOYMENT=1`. Ruby 4.0.0 with YJIT, Puma 8.0.2,
Solid Queue 1.7.0, Thruster 0.1.26.

Not run: kamal-proxy (no TLS, no proxy hop), the `litestream` role.

## Env vars (both containers)

| var | value | why |
|-----|-------|-----|
| `SECRET_KEY_BASE` | random, `.secrets.env` | production's comes from credentials; ENV overrides it |
| `ACTIVE_RECORD_ENCRYPTION_PRIMARY_KEY` / `_DETERMINISTIC_KEY` / `_KEY_DERIVATION_SALT` | random, `.secrets.env` | `EncryptionConfiguration` refuses to boot production without them; ENV wins over credentials |
| `RAILS_MAX_THREADS` | 5 | deploy.yml |
| `WEB_CONCURRENCY` | 2 | deploy.yml (2 Puma workers, preload_app) |
| `RAILS_LOG_TO_STDOUT` | true | deploy.yml |
| `LITESTREAM_INSTALL_DIR` | /usr/local/bin | deploy.yml (inert here) |
| `MAX_REQUEST_BODY` | 67108864 | deploy.yml (Thruster body cap) |
| `RAILWATCH_ALLOWED_HOSTS` | `127.0.0.1,localhost` | production sets its public hostname. Exact names only, no ports |

`.secrets.env` is generated once by `up.sh` (mode 600, gitignored). The jobs
container gets the same env (prod's job role also carries litestream/ops
secrets, which are irrelevant here). `WEB_CONCURRENCY` only matters to Puma.

Deliberately unset:
- `RAILS_MASTER_KEY`: no throwaway credentials file is needed.
  `require_master_key` is not set, and the app reads nothing from credentials
  that ENV doesn't override.
- `RAILWATCH_TOKEN`: the gem's `Configuration#enabled?` is
  `@enabled && (local? || token.present?)`, so without a token **no
  self-monitoring reporter runs**.
- `LITESTREAM_*`: litestream is off.
- `SOLID_QUEUE_IN_PUMA`: unset in prod too.
- SMTP, Google, Linear and `OPERATIONS_*`: no outbound calls. Heartbeats return
  early on a blank URL.

SSL needed no change: production.rb sets `assume_ssl = true`, so every request
counts as HTTPS and `force_ssl` never redirects plain http. Host authorization
stays on, with the local names allowed.

## Seeding

`seed.sh N` runs `bin/rails runner` in `rw-baseline`:

- One account `bench` on **plan `business`** (250 M events/month, 90-day
  retention), with `monthly_event_quota` overridden to 10^12 so
  `Account#quota_exhausted?` (402) cannot trigger during a benchmark. The
  override is a column value, the same mechanism production uses for
  enterprise deals, not a code patch.
- One application `bench`. Environments are `env-0001`..`env-NNNN` with slugs
  `bench-bench-env-NNNN`, and each gets its own tenant SQLite file at
  `/rails/storage/telemetry/<slug>.sqlite3`, created and migrated by the
  `after_create_commit` hook. The script fails if any tenant file is missing.
- Tokens are `rw_` + 44 base58 characters, and only the SHA-256 digest is
  stored (`Environment#assign_token`), so a token is printable only at
  creation. Re-running `seed.sh` **rotates** the existing bench
  environments' tokens and writes a fresh file, so earlier token files go stale.

## Rate limits: NOT disabled, and no env switch exists

Both limits are hardcoded constants, and both were verified by hitting them.

| limit | where | key | value | observed |
|-------|-------|-----|-------|----------|
| IP backstop | `lib/ingest_request_rate_limit.rb` (Rack middleware after `RemoteIp`) | `request.remote_ip` | **2,400 req/min** on any `/ingest*` path | 2,399 x 200, then 101 x 429 |
| per environment | `app/controllers/ingest/base_controller.rb` (`rate_limit`, after auth) | `environment.id` | **600 req/min** on every ingest action (records, ping, deploys, kamal) | 600 x 200, then 20 x 429 |

Both use fixed 1-minute windows in Solid Cache (keys
`production:rate-limit:ingest:ip-backstop:<ip>` and
`production:rate-limit:ingest:environment:<id>`), so every ingest request also
pays for two Solid Cache increments in `production_cache.sqlite3`. That cost
is part of the measured path.

What this means for a load test:

- **Per environment, at most 10 req/s.** To go faster, spread load across
  environments (`seed.sh N` and round-robin the tokens).
- **Per client IP, at most 40 req/s summed over all environments.** From the
  host every request arrives from the docker bridge gateway (`172.17.0.1`).
  **`X-Forwarded-For` is honoured here**, unlike behind kamal-proxy in
  production: `RemoteIp` treats the private bridge address as a trusted proxy,
  so a client-supplied `X-Forwarded-For: 192.0.2.N` becomes its own
  rate-limit key. This was verified, and it is how the tests above got clean
  buckets. A load generator that rotates XFF across at least ceil(rps/40)
  addresses and tokens across at least ceil(rps/10) environments never sees a
  429, while the limiter code still runs on every request, as in production.
  The alternative is to stay under 40 req/s in total.
- A 429 returns `{"error":"rate limit exceeded","retry_after":60}`. Count 429s
  separately; they are not ingest throughput.

## Smoke test result (fresh volume)

```
POST 28 records (3734 bytes gzipped), batch 339a12f8-cb4c-46b4-8065-f891fd583fd9
{"accepted":28,"rejected":0,"rejections":[]}
HTTP 200 in 0.401720s      (first ingest after boot; 0.23 s on an earlier warm run)
accepted 28 of 28
```

Rows that landed in `bench-bench-env-0001.sqlite3`:
- One row each in attachments, broadcasts, cache_events, deprecations,
  enqueued_jobs, exceptions, executions, health_samples, llm_calls, logs (plus
  the logs_fts tables), mails, n_plus_ones, notifications, outgoing_requests,
  people, processes, profiles, queries, query_shapes, sessions, spans,
  storage_ops, transactions, view_renders and visits.
- ingest_batches 1, release_health 1, rollups 11.

The jobs container ran RollupJob, GroupExceptionsJob and ReleaseHealthRollupJob
(plus the recurring jobs), and `solid_queue_failed_executions` is empty.

Why 28 records produce only one `executions` row: the five execution-type
fixtures (request, job_attempt, scheduled_task, command, channel_action) share
a single `execution_id`, and the gem's writer upserts executions
`unique_by: :execution_id`. Benchmark payloads should use distinct ids.

## Image and memory

- Image `railwatch-cloud-baseline:464f0fd`: **492 MB** (id `d36a202d33f9`).
- Idle, about 90 s after start with one smoke batch ingested
  (`docker stats --no-stream`):

| container | docker stats MemUsage | per-process VmRSS |
|-----------|----------------------|-------------------|
| `rw-baseline` | **242 MiB** | puma master 172 MB, worker 0 176 MB, worker 1 163 MB, thrust 9 MB |
| `rw-baseline-jobs` | **401 MiB** | supervisor 176, dispatcher 167, worker (default,alerts,recurring) 178, worker (rollups,maintenance) 180, scheduler 176 MB |

The per-process RSS adds up to more than MemUsage because forked processes
share copy-on-write pages, which the cgroup counts once. Use MemUsage for the
head-to-head comparison. For scale, the deploy.yml comment puts each Puma
worker at about 500 MB under production load.

## Surprises and gotchas

- `bin/rails runner` prints Active Storage's `image_processing` warning to
  **stdout** on every boot. The scripts prefix their own output lines
  (`TOKEN `, `SLUG `) and filter on that prefix.
- `X-Railwatch-Batch-Id` without `X-Railwatch-Producer-Id` takes the
  **direct**, non-receipted path, which does no idempotency checks and writes
  no receipt. The receipted path needs all of these:
  - `Producer-Id` and `Batch-Id`, both UUIDs, where Batch-Id is a UUIDv7 that
    gets a time check
  - `Body-SHA256`
  - `X-Railwatch-Policy: everything-v1`

  Decide which of the two paths the Rust rewrite is compared against.
- Two pieces of primary-database work happen on every batch and are part of
  the measured path:
  - every direct-path batch runs a quota check, `Account#events_this_month`,
    which is a SUM over `ingest_usage_months`
  - every accepted batch updates the usage ledger
- The image has no `ps`; read `/proc` instead (that is how the RSS column
  above was measured).
- Recurring Solid Queue jobs (scans, operations status, and so on, every 1-10
  minutes) keep running in the jobs container during a benchmark, as they do
  in production.
