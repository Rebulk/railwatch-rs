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
3. `docker stop` the old colour: it finishes in-flight requests, drains its
   tenant writers (queued batches commit), exits, and the lock passes to the
   new one
4. `kamal-proxy deploy --target <new>`: health-checked first
5. `kamal-proxy resume`: the held requests go to the new colour

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

Upgrade: copy the new binary to `bin/railwatch.new` and run `railwatch-swap`
(the old one is kept as `bin/railwatch.prev`). `railwatch-swap status` shows
the serving colour. `SERVICES` may list several kamal-proxy services
(`svc=host1,host2`, or `svc=host/path` for a path on a host whose root
another app serves); every swap pauses, moves and resumes them together.
`BETWEEN="cmd" railwatch-swap` runs `cmd` at the one moment no server holds
the data dir (e.g. to put a prepared control DB in place).

## Proof

`bench/swap/lab.sh up <bin dir>` builds a local edge (kamal-proxy v0.9.2,
a CA-signed certificate, a docker volume); `bench/swap/zero_drop.sh RATE
SWAPS DURATION` runs rw-load over HTTPS through the proxy (receipted
batches of 500 records) while upgrading SWAPS times, then counts
`ingest_batches` and `ingest_receipts` in the server's own files.

2026-10-16, workstation under heavy unrelated load (load average 10-30):

| run | posts | answered 200 | ingest_batches | receipts | swaps | held per swap |
|---|---|---|---|---|---|---|
| 8 batches/s, 150 s | 730 | 730 | 730 | 730 | 8 | 0.76-7.1 s |
| 8 batches/s, 150 s (multi-service script) | 955 | 955 | 955 | 955 | 8 | 0.52-5.9 s |
| 20 batches/s, 120 s | 904 | 904 | 904 | 904 | 6 | 0.6-9.1 s |

Every post answered 200 and every one is in the database exactly once.
The hold is how long requests wait in the proxy: the old server's drain
(its in-flight batches must commit) plus the new one's start. On an idle
machine it is 0.3-0.8 s; under the load above it grew with the commit
backlog. A request is only refused if it is held longer than `MAX_PAUSE`
(default 60 s; the proxy then answers 504, which the gem retries).

For comparison, the plain `docker rm` + `docker run` restart this replaces
refuses every connection for the stop + start time, so a client without
retries (a mirror, a webhook) loses those requests.
