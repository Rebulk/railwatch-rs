# Self-hosting Railwatch

One static binary, one command. It serves HTTPS itself (Let's Encrypt), runs
under systemd, upgrades without dropping a single ingest post, and backs up
to any S3-compatible bucket.

## The 1-minute install

You need a Linux VPS with systemd (Ubuntu, Debian, Rocky/Alma, Fedora,
Arch; x86_64 or arm64), a DNS A/AAAA record for your hostname pointing at
it, and ports 443 and 80 open to the internet.

```sh
curl -fsSL https://github.com/Rebulk/railwatch-rs/releases/latest/download/install.sh \
  | sudo sh -s -- --domain monitor.example.com --email you@example.com \
                  --alert-slack https://hooks.slack.com/services/T.../B.../...
```

It asks for the admin password, then prints the two lines your Rails app
needs:

```
RAILWATCH_INGEST_URL=https://monitor.example.com
RAILWATCH_TOKEN=rw_...
```

What it did, in order (each step is skipped if already done):

1. Downloaded `railwatch-<version>-<arch>-linux-musl.tar.gz` and
   `SHA256SUMS` from the release over https only (curl `--proto '=https'
   --tlsv1.2`, wget `--https-only`), checked the tarball against the sha256
   that `bin/release` baked into that release's `install.sh` (and, when
   `cosign` is installed, the signature on `SHA256SUMS`; see
   [Releases](#releases)), refusing on any mismatch, and installed
   `/usr/local/bin/railwatch`.
2. Created the `railwatch` system user, `/var/lib/railwatch` (data, 0700)
   and `/etc/railwatch/railwatch.env` (config, root 0600: `KEY=VALUE` lines
   that systemd and the binary read as data; no shell ever sources it).
3. Installed `railwatch.socket` (systemd holds ports 443 and 80) and
   `railwatch.service` (runs as `railwatch`, no capabilities, read-only
   system, syscall filter, private users; `systemd-analyze security`
   exposure 1.1).
4. Ran `railwatch setup`: admin user, account, first app and environment,
   and the alert destination. The ingest token is printed once at the end
   and kept nowhere in plain text (the database stores its digest): copy it
   then. Lost it? `sudo railwatch env create <name>` makes a new one.
5. Started it, waited until `https://<domain>/up` answered with a real
   certificate, and ran `railwatch doctor`.

Measured on a fresh Ubuntu 24.04 container against Pebble (Let's Encrypt's
test CA): **3.6 s** from `curl` to a verified HTTPS `/up`, certificate
issuance included (`bench/install/pebble_proof.sh`). Against Let's Encrypt
proper, add its validation time, usually a few seconds.

Non-interactive (CI, cloud-init): put the password in a file and pass
`--password-file /root/pw` (with `curl | sh`, stdin is the script, so
`--password-stdin` only works when you run the script as a file).

**Alerts are part of setup, on purpose.** Pass one of `--alert-webhook URL
[--alert-webhook-secret S]`, `--alert-slack URL`, `--alert-email ADDR`
(needs SMTP, below). That destination gets new and regressed issues and,
most importantly, "your app stopped reporting". Without one nothing tells
you the app is down.

### Behind your own proxy

`--no-tls [--url https://monitor.example.com]` listens on `127.0.0.1:8790`
(plain HTTP) for nginx, Caddy, a load balancer or a Cloudflare tunnel in
front. With an https `--url`, `RAILWATCH_TRUST_PROXY=1` is set so the
per-IP limit uses `X-Forwarded-For`.

### Without the installer

The binary is the whole app: `RAILWATCH_DATA_DIR=./data railwatch setup`,
then `railwatch serve`. Any process supervisor works; socket activation
(`LISTEN_FDS`) is used when present.

## HTTPS: why it is built in

The installer used to put Caddy in front. It now serves TLS from the binary
(rustls + rustls-acme), because:

- **Nothing else to install.** Caddy meant a third-party apt repository
  whose signing key broke on 2026-10-14 (we fell back to Ubuntu's
  two-year-old caddy 2.6.2), and no package at all on Rocky or Alpine. The
  static binary runs everywhere; now so does HTTPS.
- **Zero-drop restarts cover TLS too.** systemd holds ports 443 and 80, so
  an upgrade queues connections in the kernel instead of refusing them. With
  a proxy, the proxy's own restarts were a second, uncoordinated gap.
- **One process to monitor and one log** (`journalctl -u railwatch`
  shows certificate issuance and renewal).

How it works: the certificate is obtained with the **TLS-ALPN-01** challenge
on port 443 itself, so issuance needs only 443. Port 80 only answers with a
308 redirect to https. The ACME account key and certificate live in
`/var/lib/railwatch/acme/` (back it up with the rest of the data dir; a
restart serves the cached certificate immediately). Renewal starts when a
third of the validity is left (30 days for Let's Encrypt's 90) and the new
certificate is swapped in without a restart. The Pebble proof checks this
with 120-second certificates: renewed after 80 s, same process.

Caddy's strengths we give up: HTTP/3, on-demand certificates for many
hostnames, and its long track record. Use `--no-tls` and your own proxy if
you need them.

## Configuration reference

Everything is a `RAILWATCH_<KEY>=value` line in `/etc/railwatch/railwatch.env`
(one per line, no quoting needed, nothing expanded; or `<key> = ...` in a
`railwatch.toml` pointed at by `RAILWATCH_CONFIG`).
Restart to apply: `sudo systemctl restart railwatch` (no connection is
refused). `railwatch doctor` prints every setting and where it came from.

| key | default | |
|---|---|---|
| `TLS_DOMAIN` | | built-in HTTPS for this hostname (installer `--domain`) |
| `TLS_EMAIL` | | ACME account contact (expiry warnings from the CA) |
| `TLS_ACME_DIRECTORY` | Let's Encrypt production | another ACME CA (LE staging, ZeroSSL, step-ca, Pebble) |
| `TLS_ACME_CA_FILE` | | PEM CA to trust for the ACME directory itself |
| `TLS_CERT_FILE`, `TLS_KEY_FILE` | | serve a certificate you manage instead of ACME |
| `TLS_REDIRECT_BIND` | `<host>:80` when bind is :443 | the http to https redirect listener; `off` disables it |
| `URL` | `https://<TLS_DOMAIN>`, else `http://<bind>` | public URL: links, cookies, the printed ingest URL |
| `BIND` / `PORT` | `127.0.0.1:8790` | used only without systemd's sockets |
| `DATA_DIR` | `./data` (`/var/lib/railwatch` under the installer) | |
| `TRUST_PROXY` | off | key the per-IP limit on `X-Forwarded-For` |
| `SIGNUP` | `invite` | `open` lets anyone create an account |
| `GOOGLE_CLIENT_ID`, `GOOGLE_CLIENT_SECRET` | | both turn on "Continue with Google" ([below](#sign-in-with-google)) |
| `SMTP_ADDRESS`, `_PORT` (587), `_USERNAME`, `_PASSWORD`, `_SECURITY` (`starttls`/`tls`/`none`), `_FROM` | | password resets, invitations, email alerts, the weekly digest ([below](#weekly-digest)) |
| `BACKUP_S3_ENDPOINT`, `_BUCKET`, `_ACCESS_KEY_ID`, `_SECRET_ACCESS_KEY` | off | all four turn backups on |
| `BACKUP_S3_REGION`, `_PREFIX` | `us-east-1`, none | |
| `BACKUP_INTERVAL_SECONDS` | 300 | |
| `DETAIL_RETENTION_DAYS` | 7 | queries, logs, spans, cache and view events |
| `CORE_RETENTION_DAYS_MAX` | 90 | cap on an environment's retention |
| `MAX_DATA_BYTES` | off | size budget for telemetry (`5G`, `500M`, bytes). Over it, the oldest day files are deleted, detail days before core days; never today's, rollups or the control DB. Still over with only today's files left, ingest answers 503 + `Retry-After`. For a server sharing its disk with an app ([below](#disk-budget)) |
| `MIN_FREE_BYTES` | off | below this much free space on the data dir's filesystem, ingest answers 503 + `Retry-After` (the gem keeps its buffer and retries) |
| `RATE_LIMIT_IP`, `RATE_LIMIT_ENV` | 2400, 600 per minute | `0` disables |
| `SILENCE_SECONDS` | 60 | quiet time before "app stopped reporting" |
| `DEADMAN_URL` | | pinged every 30 s while healthy (healthchecks.io etc.) |
| `ALLOW_PRIVATE_ALERT_TARGETS` | off | alert destinations on private/loopback addresses (SSRF risk; README) |
| `ALERT_DELIVERY` | `on` | `off` records alerts as skipped and withholds weekly digests (shadow installs) |
| `SECRET_KEY_BASE` | generated into the data dir | |
| `LOG` | `info` | `debug` logs every batch |
| `HTTP_HEADER_TIMEOUT_SECONDS` | 30 | a request head must arrive within this; also the idle keep-alive limit |
| `HTTP_BODY_TIMEOUT_SECONDS` | 600 | a request body must arrive within this (32 MiB at ~450 kbit/s) |
| `HTTP_IDLE_TIMEOUT_SECONDS` | 300 | a connection with no bytes moving either way is closed |
| `MAX_CONNECTIONS` | 4096 | open connections; more wait in the kernel's backlog |
| `MAX_CONNECTIONS_PER_IP` | 256 | open connections from one address (an IPv6 /64); more are closed at once. Off with `TRUST_PROXY` (every connection comes from the proxy: limit per client there) |
| `NEW_ISSUES_PER_HOUR`, `NEW_ISSUES_PER_BATCH` | 1000, 200 | per environment: past these, new exception groups count onto one "Too many new issues" issue |
| `NEW_ISSUES_PER_ACCOUNT_PER_HOUR` | 3000 | the same across all of an account's environments |
| `BASE_PATH` | none | serve every page under this prefix ([below](#running-behind-an-app)) |
| `TRUSTED_PROXY_SECRET_FILE` | off | accept a signed `X-Railwatch-Proxy-Auth` header as the signed-in user ([below](#running-behind-an-app)) |
| `STANDBY_RELAY` | off | a second server on a held data dir relays to the first ([below](#running-behind-an-app)) |

## Running behind an app

For the railwatch-server gem, or anything else that runs this server next
to one Rails app and shows the dashboard inside it (at `/railwatch`, behind
the app's own login). A normal install needs none of this, and all three
settings are off by default.

**`RAILWATCH_BASE_PATH=/railwatch`** serves every page, asset, the live
channel (`/railwatch/cable`), `/railwatch/mcp` and `/railwatch/api/v1`
under the prefix. Redirects, cookie `Path`s, links in emails and alerts,
and the MCP and Google callback URLs carry it too. **`/ingest` and `/up`
stay at the root**, so the agent's `RAILWATCH_INGEST_URL` is the same with
or without it. The prefix is letters, digits, `-`, `_` and `.` between
single slashes. `..`, `//`, percent-encodings, `/up` and `/ingest` are
refused at startup. Paths are matched as sent, never decoded or normalized
first, so `/railwatch/../x` or `/railwatch%2fx` reach no page, and a
redirect back to the Referer only goes to a path under the prefix.

**`RAILWATCH_TRUSTED_PROXY_SECRET_FILE=/path/to/secret`** lets the app in
front say who is signed in, so nobody signs in to Railwatch separately.
The file holds a random key of at least 32 bytes, e.g.
`openssl rand -hex 32 > secret && chmod 600 secret`. The server will not start if the file is
missing, unreadable or shorter. On every request the proxy sends

```
X-Railwatch-Proxy-Auth: v1.<base64url(claims)>.<base64url(HMAC-SHA256(key, "v1." + base64url(claims)))>
claims: {"email": "ada@example.com", "name": "Ada", "role": "admin" | "member", "exp": <unix seconds>}
```

(base64url without padding, `exp` at most 300 seconds ahead; the
railwatch-server gem signs a fresh one per request, valid for 60 s).

The security model:

- **Off, the header means nothing.** Without the setting, a forged header
  is ignored and stripped, not checked, so it cannot do anything to a
  normal install.
- **On, a present header must verify or the request is refused (401)** on
  every dashboard route (pages, the live channel, MCP, the API). It is
  never quietly treated as signed out. `/ingest` and `/up` never read it. The MAC is compared in constant
  time; an expired header, one valid more than 300 s ahead, an unknown
  claim, or a role other than `admin`/`member` is refused. The header is
  removed before any handler, so it is never logged, stored or forwarded.
- **What a valid header grants**: that user, created on first sight
  (verified, with a random password nobody knows) in the server's first
  account, the one `railwatch setup` made, with the claimed role. The role
  follows the proxy on every request, but the proxy can never create an
  owner, demote the owner, or add anyone to another account. An email that
  matches an existing user (one who signed up directly, or the owner) *is*
  that user, with their access: the proxy is trusted to name people, so
  only run it in front of an app whose login you trust with that. Each
  proxied user has one session row, reused; Settings > Sessions shows it
  as "railwatch trusted proxy". Proxy-created users have no usable
  password, so the actions that ask for one (changing email or password,
  deleting the user or an account) are not available to them.
- **CSRF still applies**: a state-changing request needs the Origin check
  and the session's token, which the browser gets and sends through the
  proxy like any cookie.
- **Replay**: there is no nonce. A header captured in transit can be
  replayed until its `exp`, at most 300 s (60 s from the gem), the same
  exposure as the session cookie that travels with it. Keep the hop between
  proxy and server on loopback or a Unix socket, and have the proxy drop
  any `X-Railwatch-Proxy-Auth` the browser sent.
- **Who can reach the server** decides everything else: anyone who can
  send it requests and knows the key is whoever they claim. Bind it to
  loopback (`RAILWATCH_BIND=127.0.0.1:...`) and keep the key file
  readable only by the app's user (a world-readable one is warned about).
  Requests without the header still use cookie sign-in as usual.

**`RAILWATCH_STANDBY_RELAY=1`** is for two containers of one app sharing
the data volume during a blue/green deploy. Each container has its own
loopback, and only one server may write the data (`serve.lock`). The
server holding the lock also listens on `<data_dir>/serve.sock`. A second
server started on the same data dir binds its own address at once and
relays every connection to that socket until it gets the lock, then serves
the address itself. The new container's agent is never refused during the
overlap. A connection that arrives in the gap between the old server
exiting and the new one listening waits for up to 30 s. Batches are
stored once: a receipted batch whose answer was lost when the old server
exited is retried by the agent and acknowledged as a duplicate by the
new one.

- No new network listener: the standby binds the address it would serve
  anyway, and `serve.sock` is a Unix socket, mode 0600. Both ends check
  the other's uid, so only the data dir's user can connect, and a standby
  relays only to a server of that user.
- The relay copies bytes and nothing else. Every relayed request is
  authenticated by the holder exactly as if it had come in directly
  (cookies, tokens, the proxy header, CSRF), and it can reach nothing but
  the holder.
- At most `MAX_CONNECTIONS` are relayed at once; more wait in the kernel's
  backlog. The holder's HTTP timeouts apply to relayed connections, and it
  sees them all as `127.0.0.1` (the agent's own address in this setup), so
  `MAX_CONNECTIONS_PER_IP` and `RATE_LIMIT_IP` count them together.
- Not used with built-in TLS or socket activation, or when the data dir's
  path is too long for a socket (about 100 bytes). In those cases, and
  when the holder already has the address (same host and network), the
  second server waits for the lock as before.

### Disk budget

Retention bounds telemetry by time (7 days of detail, the account's
retention of core), not size. On the app's own volume, a busy app can
still fill it, and then the app fails too (sign-in, `db:prepare`). Two
settings bound it; both are off unless set:

- `MAX_DATA_BYTES=5G`: at start, every 10 s, and as soon as ingest
  crosses it, while telemetry is over this, the oldest day file is
  deleted: detail days (queries, logs, cache, views, spans) of every
  environment first, then core days (requests, jobs, exceptions). Today's
  files, the rollups (charts, issue history) and the control DB are never
  deleted. Ingest counts what it writes, so a burst goes over by about one
  batch per sender, not by 10 s of writes. Each pass that deletes logs a
  warning with what went; an environment's lost core days move its ingest
  floor, so late rows for them are rejected rather than recreating the
  files. Still over with nothing older to delete (today alone is over the
  budget), `/ingest` answers 503 with `Retry-After: 60` and
  `"telemetry over max_data_bytes"` until telemetry is within it again
  (usually after midnight UTC, when today's files become yesterday's).
  Logged as an error once when it starts and once, with the number of
  refused requests, when it ends. Size the budget for at least a day of
  telemetry.
- `MIN_FREE_BYTES=1G`: below this much free space on the data dir's
  filesystem, `/ingest` answers 503 with `Retry-After: 60` (the gem keeps
  the batch and retries) and nothing is written; the dashboard keeps
  working. Logged once when it starts and once, with the number of
  refused requests, when space is back. This covers what the budget
  cannot: the app's own files filling the volume.

`railwatch doctor` shows both. Size the budget below the volume's free
space minus `MIN_FREE_BYTES`, so the budget acts first.

## Weekly digest

Every Monday at 08:00 UTC the server emails each account member who has
"Weekly digest" on (Settings > Profile; on by default) a summary of the
past 7 days per environment: requests, p95, 5xx rate, job attempts and
failures, new and resolved issues, deploys and the three most frequent open
issues, linking to `URL`. Accounts and environments with no requests and no
job attempts in the week are skipped.

It needs SMTP. Without it, and with `ALERT_DELIVERY=off`, the digests are
built and logged as not sent (the log line counts them), never emailed. A
run missed while the server was down is not caught up. To see what the
next run would send, without sending anything:

```sh
sudo railwatch digest send --dry-run              # every account
sudo railwatch digest send --dry-run --account 3  # one account
```

## `railwatch doctor`

Run it any time: `sudo railwatch doctor`. As root, the binary reads
`/etc/railwatch/railwatch.env` itself (`RAILWATCH_ENV_FILE` names another
file) and switches to the data dir's owner (`railwatch`) before touching
anything, so it never leaves root-owned files in the data dir; the same
holds for every other command except `serve` and `upgrade`.
It checks: the data dir is writable; disk over 10% free; the control DB;
SMTP accepts a connection; **Google sign-in** (configured or not, the
redirect URI to register, Google's signing keys reachable); **DNS** for the public host resolves; **ports**
443 (and 80 with built-in TLS) accept connections at that address; the
**certificate** names the domain and is not past its renewal point (that
means renewal is failing); **backups** reach the bucket and the last
successful pass is recent. Exit status 1 if anything failed.

## Sign in with Google

Optional. With a Google OAuth client configured, the sign-in and sign-up
pages show "Continue with Google". Without one there is no button.

1. In the [Google Cloud console](https://console.cloud.google.com/), pick or
   create a project, then **APIs & Services → OAuth consent screen**: user
   type External (or Internal for a Workspace-only install), app name
   "Railwatch", your support email; scopes `openid`, `email`, `profile`
   (the non-sensitive defaults, so no verification is needed).
2. **APIs & Services → Credentials → Create credentials → OAuth client
   ID**, type **Web application**. Under **Authorized redirect URIs** add
   exactly:

   ```
   https://<your host>/auth/google_oauth2/callback
   ```

   (`railwatch doctor` prints it from your `URL`.) Google requires https,
   except for `http://localhost`. No JavaScript origins are needed.
3. Copy the client ID and secret into the env file and restart:

   ```
   RAILWATCH_GOOGLE_CLIENT_ID=1234-abc.apps.googleusercontent.com
   RAILWATCH_GOOGLE_CLIENT_SECRET=GOCSPX-...
   ```

Who may sign in: an existing user whose email Google has verified is linked
to their Google account on first use (an email this server never verified
is refused, so a Google account cannot take over an account). A new email
creates a user only when sign-up is open (`SIGNUP=open`) or the email has a
pending invitation. Google-linked users confirm destructive actions
(deleting their user or an account) with Google, not a password.

Moving from Railwatch cloud: the redirect URI path is the same, so an
existing OAuth client keeps working once its redirect URI names this host.

## Backups

At install: `--backup-s3-endpoint https://s3.us-east-1.amazonaws.com
--backup-s3-bucket my-railwatch --backup-s3-access-key-id ...
--backup-s3-secret-access-key ... [--backup-s3-region ...]
[--backup-s3-prefix monitor/]`. Re-running the installer with these adds
them to an existing install. Works with AWS S3, Cloudflare R2, Backblaze
B2, Wasabi, MinIO (path-style SigV4).

Every 5 minutes the control DB is snapshotted (SQLite online backup, zstd)
and uploaded if it changed; each environment's meta file (rollups,
receipts, people) at most hourly, when it changed; each day file is
uploaded once, after it closes (two days old). `sudo -u railwatch
railwatch backup` runs a pass now. Details and the restore proof:
docs/STORAGE.md, "Backups".

Restore onto a new server: install with `--no-tls` (or anything), `sudo
systemctl stop railwatch railwatch.socket`, empty `/var/lib/railwatch`, run
`railwatch restore` as the `railwatch` user with the same `BACKUP_S3_*`
settings, then start again. `secret_key_base` is not in the bucket: copy
`/var/lib/railwatch/secret_key_base` separately (or set `SECRET_KEY_BASE`),
or integration secrets must be re-entered.

## Upgrades

```sh
sudo railwatch upgrade                   # the latest release
sudo railwatch upgrade --version v0.3.0  # a specific one
sudo railwatch upgrade --version v0.2.0 --allow-downgrade  # back to an older one
```

An older release than the running one (semver order, so `v1.0.0-rc.1` is
older than `v1.0.0`) is refused without `--allow-downgrade`: it may have
vulnerabilities fixed since. `install.sh` does the same when it would
replace a newer installed binary.

That downloads the release's `install.sh`, verifies it (the release's
signed `SHA256SUMS`, see [Releases](#releases); no cosign needed),
and runs it with `--upgrade`: download, checksum, atomic swap of
`/usr/local/bin/railwatch`, `systemctl restart railwatch.service`. Re-running the original install command does
the same, and also applies any new flags.

**No ingest post is dropped.** systemd keeps the listening sockets open
across the restart; connections that arrive while the new process starts
wait in the kernel's queue; the old process finishes in-flight requests and
commits queued writes before it exits (SIGTERM, up to 60 s). Measured
(`bench/install/upgrade_proof.sh`): 4 upgrades back and forth through
`railwatch upgrade` while rw-load sent 10 batches/s of 500 records over
HTTPS: **641 posts, 641 answered 200, 641 batches in the database, 0
errors**.

**One server per data directory.** `railwatch serve` holds an exclusive
lock on `<data_dir>/serve.lock` for its whole life; a second `serve` on the
same directory waits until the first has drained and exited, and `split`,
`import-rails` and `restore` refuse to run while a server holds it. Behind
kamal-proxy (a host shared with Kamal apps), `deploy/kamal-proxy/` uses
that for zero-drop blue/green restarts; see its README.

## Uninstall

```sh
sudo systemctl disable --now railwatch.service railwatch.socket
sudo rm /etc/systemd/system/railwatch.{service,socket} /usr/local/bin/railwatch
sudo systemctl daemon-reload
sudo rm -rf /var/lib/railwatch /etc/railwatch   # the data, certificates and config: gone for good
sudo userdel railwatch
```

Keep `/var/lib/railwatch` (or the backup bucket) if you might come back.

## Moving from Railwatch cloud (Rails)

1. Copy the Rails install's `storage/production.sqlite3` (+ `-wal`) and
   `storage/telemetry/` to the new server, with the Rails app stopped or
   not (the import reads a snapshot and never writes the source).
2. Install as usual (`--domain ...`), then swap the fresh control DB for
   the imported one (`import-rails` only fills an empty control DB; the
   certificate and `secret_key_base` in the data dir stay):

   ```sh
   sudo systemctl stop railwatch.service railwatch.socket
   cd /var/lib/railwatch && sudo mkdir fresh && sudo mv control.sqlite3* telemetry fresh/
   sudo -u railwatch env RAILWATCH_DATA_DIR=/var/lib/railwatch \
     ACTIVE_RECORD_ENCRYPTION_PRIMARY_KEY=... ACTIVE_RECORD_ENCRYPTION_KEY_DERIVATION_SALT=... \
     railwatch import-rails --primary /srv/old/production.sqlite3 --telemetry-dir /srv/old/telemetry
   sudo systemctl start railwatch.socket railwatch.service
   ```

   Users, accounts, apps, environments (with their tokens: the Rails apps
   keep working unchanged), issues, alert rules and integrations carry
   over; everyone signs in again. The telemetry files are linked, not
   copied: keep `/srv/old/telemetry` (or move it under the data dir first
   and pass that path).
3. **Integration secrets.** Rails encrypted webhook URLs, secrets and Slack
   URLs with Active Record encryption. Pass Rails' keys (from
   `bin/rails credentials:show`, `active_record_encryption.primary_key` and
   `key_derivation_salt`, or the `ACTIVE_RECORD_ENCRYPTION_*` env vars the
   cloud's deploy sets; rotated keys: `--ar-primary-key` once per key,
   oldest first) and each one is decrypted and re-encrypted with this
   install's key. Without them the import lists each such integration as
   UNREADABLE and you re-enter it. `railwatch integrations` shows whether
   any are left. Proof against Rails 8.1's own encryption:
   `bench/install/ar_import_proof.sh`.
4. Point DNS at the new server; the Rails apps' `RAILWATCH_INGEST_URL`
   stays the same if the hostname does.

Telemetry files are adopted in place (single-file Rails layout);
`railwatch split --all` (server stopped) converts them to the per-day
layout (docs/STORAGE.md).

## Releases

Each `v*` tag builds, in GitHub Actions (`.github/workflows/release.yml`):

- `railwatch-<v>-x86_64-linux-musl.tar.gz`, `railwatch-<v>-aarch64-linux-musl.tar.gz`:
  one statically linked binary each (musl; SQLite, zlib, zstd, ring, rustls
  all compiled in), plus this document
- `install.sh` with the version baked in
- `SHA256SUMS`, and `SHA256SUMS.sigstore.json`: a cosign keyless signature
  (Sigstore; the identity is the release workflow itself, no key exists)

Verify by hand:

```sh
sha256sum -c --ignore-missing SHA256SUMS
cosign verify-blob SHA256SUMS --bundle SHA256SUMS.sigstore.json \
  --certificate-identity https://github.com/Rebulk/railwatch-rs/.github/workflows/release.yml@refs/tags/vX.Y.Z \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  --certificate-github-workflow-trigger push \
  --certificate-github-workflow-repository Rebulk/railwatch-rs
```

`sudo railwatch upgrade` does this itself before running the new
`install.sh`: it fetches `install.sh`, `SHA256SUMS` and the bundle over
https and verifies the bundle in-process, the same checks as the command
above (Fulcio certificate chain to Sigstore's public-good trusted root,
which the binary embeds; the certificate's SCT; the Rekor inclusion proof
and signed entry timestamp; the signature over `SHA256SUMS`; the exact
identity, issuer, trigger and repository), with
[sigstore-verify](https://crates.io/crates/sigstore-verify). If cosign is
installed it runs it as well, and both must agree. Then it checks
`install.sh` against `SHA256SUMS` and pipes the verified bytes to `sh`
(nothing lands in `/tmp`). The trusted root is the snapshot built into the
binary, not fetched over TUF: an older binary may need `install.sh` by
hand once Sigstore rotates its Fulcio or Rekor keys.

Replace `vX.Y.Z` with the release you downloaded: the exact identity pins
the signature to that tag's run of the release workflow, triggered by the
tag push (a manual dispatch never publishes), in this repository.

**Reproducible.** `bin/release vX.Y.Z` is the whole build: a
digest-pinned cargo-zigbuild image (Rust 1.93 + zig as the musl linker,
both arches from one x86_64 host), `--locked`, paths remapped,
`SOURCE_DATE_EPOCH` = the commit time (also pinned into C's
`__DATE__`/`__TIME__`, which mimalloc embeds), normalized tarballs. Two
runs with empty caches gave identical `SHA256SUMS`, and so did the GitHub
workflow run locally under `act` against a local `bin/release` of the same
commit. To check a release: `git checkout vX.Y.Z && SOURCE_DATE_EPOCH=$(git
log -1 --format=%ct) RAILWATCH_REPO=Rebulk/railwatch-rs bin/release vX.Y.Z
&& diff dist/vX.Y.Z/SHA256SUMS <downloaded SHA256SUMS>`. (The frontend is
built by Vite first; its output is deterministic for a given lockfile.)

Tested on (`bench/install/matrix.sh`, both arches, aarch64 under qemu):
Alpine 3.22, Debian 13, Ubuntu 24.04, Rocky 9: version, setup, doctor,
serve, `/up`. The installer itself needs systemd: proven on Ubuntu 24.04
(`pebble_proof.sh`); it uses only POSIX sh, coreutils, `useradd` (or
busybox `adduser`), `setpriv`, `ss`, and curl or wget.

## Proofs (for maintainers)

```sh
SKIP_CARGO=1 bin/build && bin/release v0.0.0-test.1        # dist/v0.0.0-test.1
bench/install/matrix.sh dist/v0.0.0-test.1                 # 4 distros x 2 arches
docker build -t rwi-vps:ubuntu24 - < bench/install/vps.Dockerfile
bench/install/pebble_proof.sh dist/v0.0.0-test.1           # install, verified TLS, renewal, backups, re-run
bin/release v0.0.0-test.2                                  # a second release to upgrade to
# a static rw-load for the VPS container (into dist/tools/rw-load):
docker run --rm -u "$(id -u):$(id -g)" -e HOME=/tmp -v "$PWD:/src" -w /src -e CARGO_TARGET_DIR=/src/target/zig \
  ghcr.io/rust-cross/cargo-zigbuild@sha256:d8313491ec5798de0633fdc1c5753761bff79967bea69076020dc78121b2cca8 \
  cargo zigbuild --release -p railwatch-load --target x86_64-unknown-linux-musl
mkdir -p dist/tools && cp target/zig/x86_64-unknown-linux-musl/release/rw-load dist/tools/
bench/install/upgrade_proof.sh dist/v0.0.0-test.1 dist/v0.0.0-test.2 10 4
bench/install/ar_import_proof.sh target/release/railwatch   # needs the rw-baseline container
```
