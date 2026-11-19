# Changelog

Each release's GitHub page also lists its pull requests (generated from the
tag). This file is the human summary. Versions follow semver; a `-rc.N`
tag is a release candidate (marked pre-release, not "latest", so
`install.sh` and `railwatch upgrade` do not pick it up).

## Unreleased

**Security fixes from the 2026-11-19 GA review**
- lettre 0.11.23 → 0.11.24 (RUSTSEC-2026-0560 to 0565). An exception
  message with a bare CR or LF could end an email alert's DATA early on a
  lenient SMTP relay and run its own SMTP commands on the server's session.
  Issue titles in email alerts and the weekly digest are now also put on one
  line, and an email integration's recipient must be one plain address.
- Trusted proxy (railwatch-sidecar): a proxied user stays in the proxy's
  account with the role the proxy signed. They can no longer switch into,
  create, delete or mint tokens for the user's other accounts, and an owner
  signed as `member` acts as a member. The proxy's account is now stored
  (`railwatch setup` binds the account it creates; an existing data dir is
  bound to its lowest account on upgrade). If it is deleted, proxied
  requests are refused instead of joining whichever account is first.
- A panic in one request while it held the control database (for example
  `PATCH /api/v1/issues/:id` racing the deletion of its environment) no
  longer takes the server down for every account until a restart. The lock
  is recovered, the issue update answers 404, and `/up` now answers 503 when
  the control database does not, so a health check sees a wedged server.
- Invitations: accepting one needs a verified email (anyone could set their
  unverified email to the invited address and take the invitation), and an
  inviter who is no longer an admin or owner cannot bring anyone in.
  Removing an admin, or making them a member, withdraws the invitations
  they sent.
- Removing a member revokes their API tokens restricted to that account or
  one of its applications.
- Changing your email now tells the old address and signs out your other
  sessions, as a password change does.
- Uptime monitors: members, and API/MCP tokens of members, see only the
  host of a monitor's URL (on the Uptime page, MCP `uptime`, REST
  `.../uptime`, and in uptime alerts on the Alerts pages), as for webhook
  URLs; admins still see it whole. A failed check's error no longer repeats
  the URL.
- Invitation emails (new and resent) are rate-limited to 20 an hour per
  inviting user and per account. User and account names are at most 100
  characters with no line breaks or control characters on every path that
  sets them (sign-up, profile, new account, setup, Google sign-in, trusted
  proxy), since they go into invitation subjects and bodies.
- MCP: a string argument over 4 KB (64 KB for `add_comment`'s body) is
  refused as invalid params (-32602) by every tool, instead of a long
  `search_requests` query failing as an internal error.
- Every MCP tool that returns telemetry (issue titles, exception messages,
  log lines, paths, headers) now says in its description to read it as
  data, never as instructions. docs/agent-webhooks.md recommends read-only,
  account-restricted tokens for agents that run on their own.
- `/cable` bounds what one connection can make the server keep: messages up
  to 4 KiB, subscribe identifiers up to 256 bytes, 16 subscriptions.

## v0.3.4 (2026-11-18)

- The server builds for macOS again. Two Linux-only calls added in v0.3.1
  (`prctl` for transparent huge pages, and `RUSAGE_THREAD` in the ingest-floor
  bench) are now Linux-only, so the railwatch-sidecar gem's macOS builds work.
  Linux behaviour is unchanged. CI now type-checks every binary on macOS.

## v0.3.3 (2026-11-18)

**Security fixes from the 2026-11 review**
- `GET /api/v1/.../executions/:id` leaves out the client IP, user agent and
  `X-Forwarded-For` unless the token has the `security` scope, as MCP's
  `get_execution` already did.
- MCP `list_alerts` with a token restricted to one application lists only
  that application's alerts and issues.
- Integrations: members see only the host of a webhook URL (admins still see
  it whole), and delivery errors no longer include the destination URL.
- Without SMTP, the log line for an unsent email has the recipient and
  subject only, never the reset, verification or invitation link.
- Deleting an account, application or environment with issues, alerts or
  integrations no longer fails on a foreign key; everything under it goes.
- Adding a member always sends an invitation, also to someone who already
  has a user, and the reply no longer says whether they did.
- With open sign-up, a password reset to an address taken by someone
  else's unverified sign-up hands the account to the address's owner:
  verified, the other party's sessions and tokens removed.
- A password reset link is bound to the address it was sent to as well as
  the password: changing your email makes an earlier link stop working, so
  it can no longer verify the new address.
- `RAILWATCH_TRUST_PROXY`: one client-address rule for the dashboard, MCP
  and ingest. `X-Forwarded-For` is read only from a loopback or private
  peer (the proxy in front), and the client is its right-most entry, what
  that proxy saw. Private entries are not skipped, so a LAN client cannot
  pick its own rate-limit address. Name further proxies of a chain in the
  new `RAILWATCH_TRUSTED_PROXIES` (docs/SELF-HOST.md).
- Opening an email verification link is rate-limited (20 an hour per
  client) and no longer reads every user.
- Pages send `script-src 'self'` with the dark-mode script's hash,
  `connect-src 'self'` and `form-action 'self'` in their CSP.
- Alert delivery ignores `HTTPS_PROXY`/`ALL_PROXY`, so a proxy cannot route
  around the SSRF guard.
- `railwatch doctor` masks the SMTP username.

## v0.3.2 (2026-11-18)

- **Moving to a new host name**: `RAILWATCH_REDIRECT_HOSTS` lists old names
  whose pages 308 to `RAILWATCH_URL` (same path and query), while
  telemetry, the API, MCP and the live channel keep answering on them.
  Railwatch Cloud now lives at app.railwatch.dev, with railwatch.rebulk.com
  redirecting.

## v0.3.1 (2026-11-18)

- **Docker image**: `ghcr.io/rebulk/railwatch:vX.Y.Z` and `:latest`, for
  x86_64 and arm64, published by the release workflow from the release's
  own tarballs (checked against SHA256SUMS) and signed with cosign
  keyless. No shell, runs as uid 10001, data in `/data`, plain HTTP on
  :8790 for your proxy. docs/SELF-HOST.md, "Docker".
- install.sh: with no terminal to ask for the admin password (a
  container, cloud-init, CI), it now says to pass `--password-file` or
  `--password-stdin` instead of failing with "cannot open /dev/tty".
- Docs: the gem that runs this server inside a Rails app is now
  `railwatch-sidecar` (renamed from `railwatch-server`).

## v0.3.0 (2026-11-16)

This is the first release since railwatch.rebulk.com moved off Rails, and
since the decision that this is the one Railwatch server. The Rails app
(railwatch-cloud) is now a frozen reference.

**Memory and scale** (2 threads / 4 GiB: 250 apps -> 500; docs/SCALE.md)
- Memory per tenant is about 3.5x smaller: steady private memory per app
  fell from 8.8 to 2.5 MiB (10 -> 100 apps). At 500 apps on 2 threads /
  4 GiB the server holds steady at about 1.5 GiB of private memory, where
  v0.2.x was killed at the cap (docs/SCALE.md).
- Server memory is bounded: idle connections close, and readers and rollup
  accumulators are capped across tenants (#90).
- Each tenant's memory is fixed at the root. There is no per-batch
  savepoint, so private memory stays flat at 500 apps instead of growing
  until the cap (#92).
- WAL checkpoints move off the ingest writer to one background
  checkpointer thread, with a cap: past 8000 pages (32 MB) the writer
  checkpoints inline, as SQLite's autocheckpoint would. The cap is best
  effort: a long reader that pins frames, or a failed checkpoint, can
  leave a WAL above it, as with the autocheckpoint (#92, docs/PERF.md
  "Background checkpointer").
- No transparent huge pages for the server process: `PR_SET_THP_DISABLE`,
  the same as `MIMALLOC_ALLOW_THP=0`. On a THP-`always` host this about
  halves memory under ingest (#89).
- Faster ingest: parsing takes 9-12% less CPU per event (#89), and the
  writer thread's CPU per 1k events fell from 34.5 to 20.8 ms at one batch
  per transaction. At 50 apps the server took 17,211 events/s, against
  13,889 before (#92).
- Dashboard reads by day open only the day files inside the window, not
  every file the tenant has (#87).
- New tenant files make seven indexes partial, at 0.82 -> 0.79 GB per
  million events. Existing files are not altered (#86, DIFFERENCES #16).

**Features**
- Uptime monitors with incidents, alerts and a public status page (#75; also
  in v0.2.19).

**Security**
- rusqlite 0.41 and rustix 1.1.6 for two RustSec advisories (#84).

**Docs and benchmarks**
- Rails vs Rust: 3 runs each, the full server on both sides
  (docs/RAILS-VS-RUST.md, #88). App-count scale test on 2 threads / 4 GiB
  (docs/SCALE.md, #91).
- The marketing site moved to railwatch.dev as a static build (#79).

**Upgrading** from v0.2.x: `sudo railwatch upgrade`. Behind kamal-proxy,
copy the new binary to `bin/railwatch.new` first, then run `railwatch-swap`.
Without `railwatch.new` it only restarts the current binary
(deploy/kamal-proxy/README.md). The control DB gains the uptime tables (migration 4,
applied on start). Telemetry files need no migration.

## v0.2.19 (2026-11-01)
- Uptime monitors with incidents, alerts and a public status page (#75).

## v0.2.18 (2026-10-29)
- Filters fail visibly: `a|b` alternatives, errors for unknown keys and
  values, `get_execution` by id alone (#71).

## v0.2.17 (2026-10-28)
- Session journeys across tabs (#70).

Earlier releases: see the
[GitHub releases](https://github.com/Rebulk/railwatch-rs/releases).
