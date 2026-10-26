# Railwatch (railwatch-rs)

Self-hosted monitoring for Rails apps: requests, jobs, queries, exceptions,
logs, deploys, issues and alerts, on one static binary and SQLite. It is the
server for the [railwatch gem](https://github.com/Rebulk/railwatch): point an
unmodified app's `RAILWATCH_INGEST_URL` at it and the gem's dashboard, MCP
server and JSON API work as they do against the hosted Railwatch.

One process on a small VPS serves many apps. Telemetry files stay
byte-compatible with the gem's schema, so data moves between this server, the
gem's embedded store and the hosted platform without conversion.

- `crates/server` - the server (`railwatch` binary: ingest, dashboard, alerts, MCP, `/api/v1`)
- `crates/load`   - `rw-load`, a load generator that speaks the gem's wire protocol
- `frontend/`     - the dashboard (React + Inertia), embedded at build time
- `bench/`        - differential harnesses against the Rails implementation, install proofs, benchmarks
- `research/`     - measured experiments behind the storage and restart designs
- `docs/`         - [SELF-HOST.md](docs/SELF-HOST.md) (install, config, backups, releases),
  [DIFFERENCES.md](docs/DIFFERENCES.md) (every known difference from the Rails server),
  [STORAGE.md](docs/STORAGE.md), [PERF.md](docs/PERF.md), [GATE1.md](docs/GATE1.md) (benchmarks),
  [agent-webhooks.md](docs/agent-webhooks.md) (alert webhooks an agent can act on, and a GitHub Action recipe)

## Self-hosting: the default path

On a Linux VPS with systemd, DNS for `monitor.example.com` pointing at it and
ports 443/80 open (full guide: docs/SELF-HOST.md):

```sh
curl -fsSL https://github.com/Rebulk/railwatch-rs/releases/latest/download/install.sh \
  | sudo sh -s -- --domain monitor.example.com --email you@example.com \
                  --alert-slack https://hooks.slack.com/services/T.../B.../...
```

It downloads the static binary for the CPU (checksum-verified), creates a
`railwatch` user, installs `railwatch.socket` + `railwatch.service`, serves
HTTPS itself with a Let's Encrypt certificate (no proxy package), runs
`railwatch setup`, and prints the two lines the Rails app needs
(`RAILWATCH_INGEST_URL`, `RAILWATCH_TOKEN`). `sudo railwatch upgrade` (or
re-running the command) upgrades with zero dropped ingest posts: systemd
holds the sockets across the restart. `--no-tls` puts it behind your own
proxy instead; `--backup-s3-*` turns on backups.

**Alerts are part of setup, on purpose.** Pass one of `--alert-webhook URL
[--alert-webhook-secret S]`, `--alert-slack URL`, `--alert-email ADDR` (to
`install.sh` or `railwatch setup`) and that destination gets `new_issue`,
`regressed_issue`, `silent_environment` ("your app stopped reporting", 60 s
without data by default) and `environment_recovered`. Without one, setup says
nothing will alert you. Change them later in Settings > Integrations; "Send
test" proves a destination works. Webhooks are signed:
`X-Railwatch-Signature: sha256=<hex HMAC-SHA256(secret, body)>`.

Alert destinations must be public HTTPS by default (no SSRF into the box's
network). `RAILWATCH_ALLOW_PRIVATE_ALERT_TARGETS=1` allows private and loopback
addresses (ntfy on the same box, a LAN relay); that lets anyone who can edit an
integration make the server POST to anything it can reach (cloud metadata,
localhost admin ports), so only set it when every account admin is trusted with
that. `RAILWATCH_ALERT_CA_FILE=/path/ca.pem` trusts a private CA for such a
receiver. Integration secrets (webhook URL/secret, Slack URL) are encrypted at
rest with a key derived from `secret_key_base` (XChaCha20-Poly1305); losing
`/var/lib/railwatch/secret_key_base` means re-entering them.

**What an ack means.** The server answers 200 after the batch is committed to
SQLite with `synchronous=NORMAL` (WAL), Rails' setting. An acked batch survives a
crash or kill of the railwatch process; it does not survive power loss or a
kernel panic before the next checkpoint (the last few hundred ms of commits).
Apps that need more use the gem's receipted export path (an on-disk outbox in
the app, replayed until a receipt says committed), which loses nothing across a
Railwatch host failure within its 24 h expiry.

Without systemd: `railwatch serve` binds `RAILWATCH_BIND` (default
127.0.0.1:8790); it accepts an inherited socket whenever `LISTEN_FDS=1` is set.

## Running from source

Building and testing: [CONTRIBUTING.md](CONTRIBUTING.md).

```sh
bin/build                                     # frontend (Vite) + release binary with it embedded
export RAILWATCH_DATA_DIR=./data              # control.sqlite3, telemetry/<slug>.sqlite3, sourcemaps/
target/release/railwatch env create acme-web-production --retention-days 30   # prints the rw_ token
target/release/railwatch env import <slug> <sha256-of-existing-token>          # reuse a Rails token
target/release/railwatch serve                # 127.0.0.1:8790 (PORT or RAILWATCH_BIND)
```

Env: `RAILWATCH_RATE_LIMIT_IP` (2400/min), `RAILWATCH_RATE_LIMIT_ENV` (600/min), 0 disables;
`RAILWATCH_TRUST_PROXY=1` to key the IP limit on X-Forwarded-For; `RAILWATCH_TENANT_IDLE_SECONDS`
(300) closes an idle tenant writer; `RAILWATCH_LOG=debug` logs one line per batch.

## The web app

```sh
export RAILWATCH_DATA_DIR=./data RAILWATCH_URL=https://railwatch.example.com
target/release/railwatch setup                # admin, account, first app + env; prints RAILWATCH_INGEST_URL + token
target/release/railwatch serve                # pages, /cable, /ingest on one port
target/release/railwatch doctor               # config + sources, data dir, disk, SMTP
target/release/railwatch import-rails --primary storage/production.sqlite3 --telemetry-dir storage/telemetry
```

Config: `RAILWATCH_*` env vars, else `railwatch.toml` (keys: data_dir, bind, url,
signup = "open"|"invite", smtp_address/port/username/password/security/from,
secret_key_base, trust_proxy, rate_limit_ip, rate_limit_env, enforce_quota,
alert_delivery = "on"|"off"; off records alerts as skipped and sends nothing, for a
shadow install fed a mirror of another server).
Alerting (env only): `RAILWATCH_SILENCE_SECONDS` (60), `RAILWATCH_DEADMAN_URL`,
`RAILWATCH_ALLOW_PRIVATE_ALERT_TARGETS`, `RAILWATCH_ALERT_CA_FILE`. Auto-resolve
is the account's setting (application settings page). Email alerts use the smtp_* settings above;
links in alerts use `url`.

Proofs: `bench/alerts/silent_proof.sh` (an app goes quiet; silent_environment and
environment_recovered reach an HTTPS receiver with verified signatures),
`bench/restart/run.sh bind|restart|overlap` (restarts under load).


## Verifying a release

Every release's `SHA256SUMS` is signed keyless with cosign by the release
workflow (no key exists); `sudo railwatch upgrade` checks it before
installing. By hand:

```sh
sha256sum -c --ignore-missing SHA256SUMS
cosign verify-blob SHA256SUMS --bundle SHA256SUMS.sigstore.json \
  --certificate-identity https://github.com/Rebulk/railwatch-rs/.github/workflows/release.yml@refs/tags/vX.Y.Z \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  --certificate-github-workflow-trigger push \
  --certificate-github-workflow-repository Rebulk/railwatch-rs
```

Builds are reproducible: `bin/release vX.Y.Z` on the tagged commit gives the
same `SHA256SUMS` (docs/SELF-HOST.md, "Releases").

## Contributing and security

[CONTRIBUTING.md](CONTRIBUTING.md) covers building, tests and the
differential harnesses. Report vulnerabilities privately:
[SECURITY.md](SECURITY.md).

## License

Railwatch (railwatch-rs) is free software under the **GNU Affero General
Public License, version 3 only** (`AGPL-3.0-only`, [LICENSE](LICENSE)).
If you run a modified version as a network service, section 13 asks you to
offer its users the source of your version: the server points every page
and response at its source (`SOURCE_URL` in `crates/server/src/lib.rs`,
overridable at build time with `RAILWATCH_SOURCE_URL`; `/source` redirects
there).

Third-party code it contains or is built from (ported Ruby/C code, the
railwatch gem's frontend, npm packages, Rust crates) keeps its own licenses:
[THIRD-PARTY-LICENSES](THIRD-PARTY-LICENSES), regenerated by
`bin/third-party-licenses` and shipped in every release tarball.
`cargo deny check` (deny.toml) allows AGPL-3.0-only only for this
workspace's crates and checks every dependency against a permissive list.
