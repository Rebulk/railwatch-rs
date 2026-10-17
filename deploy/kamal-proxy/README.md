# Zero-drop restarts behind kamal-proxy

For a server that shares a host with Kamal apps: kamal-proxy already owns
ports 80/443 and terminates TLS, so railwatch runs as a plain container on
the proxy's docker network, with `--no-tls`-style plain HTTP on :8790.
(On a VPS of its own, use `deploy/install.sh`: systemd socket activation
gives the same guarantee without a proxy.)

`railwatch-swap` restarts or upgrades it without refusing or losing a
request, blue/green on one data volume:

1. start the other colour; `railwatch serve` waits on `<data>/serve.lock`
   (the running one holds it), so it neither opens the data nor listens
2. `kamal-proxy pause`: new requests are held in the proxy
3. SIGTERM the old colour: it finishes in-flight requests, drains its
   tenant writers (queued batches commit), exits, and the lock passes to the
   new one
4. `kamal-proxy deploy --target <new>`: health-checked first
5. `kamal-proxy resume`: the held requests go to the new colour

If anything fails after step 3 (the new colour is unhealthy, or `BETWEEN`
fails), the new colour is stopped, the old one is started again on its own
binary and health-checked, and only then are the services resumed. Every
service the run paused is resumed however it exits.

**Why the lock.** Two servers on one data directory would both write the
same tenant files: each writer caches its high-water marks (per-day
storage), so two of them would hand out the same row ids, and the
scheduler, watchdog and alert loop would run twice. `serve` takes an
exclusive `flock` on `<data_dir>/serve.lock` before opening anything and
holds it until it has drained and exited (the kernel drops it on a crash
too); a second `serve` waits for it, and `split`, `import-rails` and
`restore` refuse to run while it is held (`tests/serve_lock.rs`).

## Setup

```sh
mkdir -p /opt/railwatch/bin && cp railwatch /opt/railwatch/bin/   # built for the image's libc
cp deploy/kamal-proxy/railwatch-swap /opt/railwatch/
cat > /opt/railwatch/swap.conf <<'EOF'
ROOT=/opt/railwatch
ENV_FILE=/opt/railwatch/railwatch.env      # RAILWATCH_DATA_DIR=/data, RAILWATCH_BIND=0.0.0.0:8790, RAILWATCH_TRUST_PROXY=1, ...
VOLUME=railwatch_data
NETWORK=kamal
PROXY=kamal-proxy
SERVICES="railwatch-web=monitor.example.com"
EOF
RAILWATCH_SWAP_CONF=/opt/railwatch/swap.conf /opt/railwatch/railwatch-swap   # first start
```

Upgrade: copy the new binary to `bin/railwatch.new` and run `railwatch-swap`.
Each colour runs its own copy (`bin/railwatch-blue`, `bin/railwatch-green`),
so a rollback runs exactly the binary the old colour ran. A successful swap
makes the new binary `bin/railwatch` (the old one is kept as
`bin/railwatch.prev`); a failed one moves it to `bin/railwatch.failed`.
`railwatch-swap status` shows the serving colour. `SERVICES` may list several kamal-proxy services
(`svc=host1,host2`, or `svc=host/path` for a path on a host whose root
another app serves); every swap pauses, moves and resumes them together.
`BETWEEN="cmd" railwatch-swap` runs `cmd` at the one moment no server holds
the data dir (e.g. to put a prepared control DB in place).

## Timeouts

All in seconds, settable in `swap.conf`:

| | default | |
|---|---|---|
| `DRAIN` | 30 | in-flight requests finish at the pause |
| `STOP_TIMEOUT` | 60 | expected time for the old colour to exit after SIGTERM |
| `STOP_WAIT` | 900 | the most the swap waits for it before giving up |
| `DEPLOY_TIMEOUT` | 30 | the new colour takes the lock, opens and passes `/up` |
| `BETWEEN_TIMEOUT` | 30 | `BETWEEN` (only counted when set) |
| `MAX_PAUSE` | the sum + 10 | how long kamal-proxy holds a request before 504 |

`MAX_PAUSE` defaults to `DRAIN + STOP_TIMEOUT + DEPLOY_TIMEOUT
(+ BETWEEN_TIMEOUT) + 10`, 130 s, and a smaller value is refused before
anything is paused: a swap whose old colour exits within `STOP_TIMEOUT`
never answers a held request 504. It is deliberately not based on
`STOP_WAIT` (900 s): if the old colour is slow to drain, it is better that
held requests get a 504 after 130 s, which the gems retry (nothing is
lost: docs/STORAGE.md, "Exactly-once"), than that every request hangs for up
to 16 minutes. So past `STOP_TIMEOUT`, and during a rollback (up to another
`DEPLOY_TIMEOUT`), requests held past `MAX_PAUSE` get 504 and are retried.
To hold requests for the whole wait instead, set `MAX_PAUSE` to at least
`DRAIN + STOP_WAIT + DEPLOY_TIMEOUT + 10`.

**The old colour is never SIGKILLed.** On SIGTERM the server answers its
in-flight requests (a 200 is sent only after the batch commits), then
drains its tenant writers and background work (usage counts, incremental
rollups, issue grouping) and exits. A SIGKILL would lose no acknowledged
batch (docs/STORAGE.md, "Exactly-once": an unanswered request is retried
by the gem and lands once), but it would lose that derived state until the
catch-up jobs redo it. So past `STOP_TIMEOUT` the swap logs it and keeps
waiting, up to `STOP_WAIT` (bound: the 600 s request-body timeout plus the
writer drain); if the server still has not exited, the swap fails, resumes
the services and leaves the container for an operator. The containers'
own `--stop-timeout` is `STOP_WAIT` too, for `docker stop` and host
shutdowns.

## Proof

`bench/swap/lab.sh up <bin dir>` builds a local edge (kamal-proxy v0.9.2,
a CA-signed certificate, a docker volume); `bench/swap/zero_drop.sh RATE
SWAPS DURATION` runs rw-load over HTTPS through the proxy (receipted
batches of 500 records) while upgrading SWAPS times, then counts
`ingest_batches` and `ingest_receipts` in the server's own files and exits
1 unless every post answered 200 and is in both exactly once.
`FAIL_AT=N` makes swap N an upgrade to `bench/swap/bad_upgrade.sh`, which
takes the data dir lock and exits, so that swap must roll back to the old
colour's binary.

2026-10-16, workstation under heavy unrelated load (load average 10-30):

| run | posts | answered 200 | ingest_batches | receipts | swaps | held per swap |
|---|---|---|---|---|---|---|
| 8 batches/s, 150 s | 730 | 730 | 730 | 730 | 8 | 0.76-7.1 s |
| 8 batches/s, 150 s (multi-service script) | 955 | 955 | 955 | 955 | 8 | 0.52-5.9 s |
| 20 batches/s, 120 s | 904 | 904 | 904 | 904 | 6 | 0.6-9.1 s |
| 8 batches/s, 75 s (never SIGKILL, binary per colour) | 462 | 462 | 462 | 462 | 4 | 0.7-6.0 s |
| 20 batches/s, 120 s (same) | 556 | 556 | 556 | 556 | 6 | 0.6-21.9 s |
| 8 batches/s, 100 s, `FAIL_AT=3` | 556 | 556 | 556 | 556 | 4 + 1 rolled back | 0.3-3.3 s |
| 12 batches/s, 90 s, `FAIL_AT=2` | 504 | 504 | 504 | 504 | 3 + 1 rolled back | 0.5-2.9 s |

Every post answered 200 and every one is in the database exactly once,
including the requests held through a rollback (the bad upgrade's swap held
them about 35 s: the old colour's stop, the 30 s health-check timeout, the
old colour's restart). The 21.9 s hold was the old server's drain under a
load spike on the workstation.
The hold is how long requests wait in the proxy: the old server's drain
(its in-flight batches must commit) plus the new one's start. On an idle
machine it is 0.3-0.8 s; under the load above it grew with the commit
backlog. A request is only refused if it is held longer than `MAX_PAUSE`
(the proxy then answers 504, which the gem retries).

For comparison, the plain `docker rm` + `docker run` restart this replaces
refuses every connection for the stop + start time, so a client without
retries (a mirror, a webhook) loses those requests.
