# Differences from railwatch-cloud (Phase 1, ingest)

The Rust server is checked against railwatch-cloud @ 464f0fd (gem 0.9.0) by:

- `bench/diff/run.sh`: the same gzip NDJSON bytes posted to both, every ack
  compared (status, JSON body, Retry-After), then every table of both tenant
  files compared row by row, ids included;
- `crates/server/tests/mapper_vs_ruby.rs`: `Mapper.row_for` on 280 edge-case
  records, compared against Ruby's output byte for byte;
- `bench/diff/cross_open.sh`: Rails opens, reads and writes a Rust-written file.

What follows is everything that is knowingly different. Anything not listed
here that differs is a bug.

## Not compared, by design (phase 2)

| What | Why |
|---|---|
| `rollups`, `release_health` in `run.sh` | Both servers compute them in the background on their own schedule, so `run.sh` cannot line them up; Phase 2 proves them with `rollup_diff.sh` instead (below). |
| `ingest_receipts.followups` / `followups_completed_at` after a drain | Each server drains on its own minute. The diff compares the column whenever neither copy has been drained yet. |

## Volatile columns (compared for presence only)

- `ingest_batches.received_at` -- each server's own clock.
- `ingest_receipts.committed_at`, `ingest_receipts.retain_until` -- derived from commit time.
- `ar_internal_metadata.created_at`, `updated_at` -- file creation time.
- Ack field `committed_at` (receipted path) -- reduced to present/absent.

## Intentional behaviour differences

1. **Usage accounting is asynchronous and aggregated.** Rails updates
   `ingest_usage_months` and `environments.events_this_month` synchronously in
   every direct-path request (and from the receipt drain for receipted ones).
   Here each committed batch sends its count to an in-process channel that
   flushes one UPDATE per (environment, month) per second to
   `control.sqlite3`. Consequences: the quota check can lag by up to ~1 s of
   traffic, and a crash loses at most the last second of usage counts (never
   rows). Receipted batches are counted once at commit (Rails counts them in
   the drain, under a claim); there is no replay risk because a replayed
   delivery is answered from its receipt and never re-counted.
2. **Quota is per environment, not per account.** Phase 1 has no accounts:
   `monthly_event_quota` lives on the environment (NULL = unlimited). Rails
   sums usage over all of the account's environments.
3. **Rate limits are in memory.** Same limits, keys and fixed 1-minute
   windows as Rails, but per process and reset on restart (Rails keeps them in
   Solid Cache). Configurable: `RAILWATCH_RATE_LIMIT_IP` (2,400),
   `RAILWATCH_RATE_LIMIT_ENV` (600); 0 disables.
4. **Client IP.** `X-Forwarded-For` is honoured only with
   `RAILWATCH_TRUST_PROXY=1`, and then the rightmost non-private address wins
   (Rails' RemoteIp with its default trusted proxies). Without it, the socket
   address. The local Rails baseline trusts XFF from the docker bridge, which is
   why `rw-load --xff-rotate` exists.
5. **Writes are coalesced.** Up to 64 queued batches (or ~20k rows) of one
   tenant commit in one `BEGIN IMMEDIATE` transaction, each in its own
   SAVEPOINT. Statement order per batch is Ruby's, and batches are applied in
   arrival order at the writer, so ids match Rails for sequential traffic.
   Under concurrency neither server promises an id order across requests.
6. **`POST /ingest/sourcemaps` exists.** The gem's `railwatch:sourcemaps`
   task calls it; railwatch-cloud answers 404. Stored at
   `data/sourcemaps/<env>/<deploy>/<filename>.map`; answers
   `201 {"ok":true,"filename":...,"bytes":...}` (the gem accepts any 2xx).
7. **`/up`** answers the same green page but without Rails' extra headers.
   Ingest responses carry no CSP/X-Frame headers (Rails adds them to
   ingest responses too; the gem ignores them). HSTS: see "Security
   hardening" below.
8. **Deploys and Kamal markers live in `control.sqlite3`** (`deploys` table,
   `environments.expected_servers/kamal_config`), not in a Rails primary DB.
   The response codes and bodies match the controllers, including an
   unparseable `timestamp` answering **422 "Deployed at can't be blank"**.
   A JSON body without `deploy` answers 400 `{"error":"param is missing or
   the value is empty or invalid: deploy"}` (it was a 500 in Rails, fixed on
   both sides 2026-10-16). Without `timestamp` a new deploy gets now and a
   re-posted one keeps its `deployed_at` (Rails used to reset it to now; also
   fixed on both sides); an explicit `timestamp` still sets it.
9. **Deploy `timestamp` parsing** handles what the gem and its Kamal hook send
   (ISO 8601 with Z or +hh:mm, "YYYY-MM-DD HH:MM[:SS]"). Rails'
   `Time.zone.parse` accepts many more shapes ("Oct 13 2026"); those answer 422
   here.
10. **`String#inspect` for unassigned code points.** Ruby escapes unassigned
    Unicode code points (e.g. U+0378) when a Hash or Array is stored in a
    string column (its `to_s` is `inspect`). Rust has no Unicode
    assigned-character table in std, so those pass through unescaped. Only
    reachable when a client sends a structure where the schema expects a
    string *and* it contains an unassigned code point.
11. **500 bodies.** Where Rails answers 500, so does Rust, with the same
    `{"error":"internal server error"}`; Rust logs the cause at error level
    instead of reporting it to an error tracker.
12. **Malformed HTTP** (broken chunking, invalid headers) is answered by
    hyper/axum instead of Thruster/Puma. Not compared; the gem never sends it.

## Measured results (2026-10-13)

| run | posts | records | acks identical | tables |
|---|---|---|---|---|
| `run.sh 20 200 7` | 26 (6 receipted replays) | 4,005 | 26/26 (25 x 200, 1 x 500) | all identical |
| `run.sh 30 300 42` | 40 (10 receipted replays) | 9,008 | 40/40 (200) | all identical, receipt follow-ups included |

The one 500 was the same on both sides then: a record whose `t` is invalid
UTF-8 was rejected, its rejection landed in `ingest_batches.rejections`, and
JSON-encoding that column raised, failing the whole batch. Fixed on both sides
(2026-10-16, not re-run against rw-baseline, which has the old Rails): the
rejection's `type` is `t.scrub.first(64)` (each maximal invalid sequence
becomes one U+FFFD) and the rest of the batch is accepted
(`an_invalid_utf8_type_is_rejected_alone_with_a_scrubbed_type`). Rust's
`String::from_utf8_lossy` replaces the same sequences as Ruby's `scrub`
(checked on lone continuation bytes, truncated sequences, surrogates and
overlong forms: identical U+FFFD counts). Against the old baseline that post
is now an expected difference (200 vs 500).

# Dashboard props (Phase 3, `crates/server/src/dashboard/`)

Checked by `bench/diff/props/run.sh`: a multi-day tenant ingested into a
scratch Rails environment, rolled up by Rails' own jobs (run synchronously,
`settle_rails.rb`), the tenant file copied out, every page captured from Rails
as an Inertia XHR under `travel_to(NOW)` and computed by `props-dump` over the
same file at the same NOW, then diffed (`compare.py`). Last runs: **123/123
cases identical** on an 81k-record tenant (`last-diff.txt`), **122/122** on the
2.25M-query tenant (`diff-large.txt`). Cases: 38 page functions x windows
1h/24h/7d, a custom from/to, a step override, sorts, filters (status, method,
class, text, FTS, store, host, model, tenant...), and second cursor pages.

## Normalized, by design

| What | Why |
|---|---|
| Shared props `auth`, `account`, `accounts`, `flash`, `google_oauth`, `applications`, `environment`, `saved_views` | Control-DB/session props p3-control owns; this module returns page props plus `shared_props(ctx)` (window, range, step, steps), which ARE compared. |
| `pagination.next_cursor` (presence compared) | Cursors are signed by each server with its own key. Rust: base64url(JSON {v, model, order, context, value, id}) + "--" + hex HMAC-SHA256; same payload shape as Rails' MessageVerifier, but only round-trips within this server. A Rails cursor gets 422 here. |
| `trace_url` (executions#show), `links` (tenants#show) | URL helpers. Rust takes the paths as params (`_environment_path`, `_logs_path`, `_people_path`) from the router. |

## Not in this module

- issues#index, alerts#index, the control half of issues#show (comments,
  activities, members, related/merged issues, alerts), usage's `account`,
  `environment_events`, `db_bytes`: control-DB or filesystem reads, p3-control.
  `issue_telemetry()` returns the telemetry half of issues#show and
  `usage_show()` the telemetry half of usage. Neither is in the diff cases yet
  (issues#show needs the control half to render; usage reads `Time.current`
  through `Rails.cache` per-day keys that a frozen NOW does not line up with):
  **not verified against Rails**.

## Intentional differences

1. **No one-minute cache.** Rails caches `grouped` and `summary_with_delta`
   for a minute (`Rails.cache`); Rust computes them on every load, so a Rust
   page is never staler than the rollups. The harness clears Rails' cache
   before each timed run so both compute.
2. **Parallel variants.** `overview_show_parallel` and
   `queries_index_parallel` run their independent sections on separate pooled
   read connections (scoped threads); props identical to the serial ones (in
   the diff cases). 7-day tenant: overview 10.3 -> 3.6 ms, queries 229 -> 139 ms.
3. **Read connections are read-only** (`SQLITE_OPEN_READ_ONLY`,
   `PRAGMA query_only`) with `mmap_size` 16 GiB (a ceiling) vs Rails' 128 MiB:
   on the 2.4 GB tenant, people@7d 174 -> 110 ms, queries@7d 258 -> 214 ms.
4. **Fugit subset.** scheduled_tasks' `next_run_at` uses jobs::schedule
   (the missed-task detector's parser): cron with an optional UTC zone and
   fugit's "every ..." forms (`at 3am`, `at 06:30`, `at minute 5`, weekdays,
   trailing `UTC`). A non-UTC zone or another natural form comes back nil
   here. Until 2026-10-16 the page had its own smaller parser that returned
   nil for every "every day at ..." and "every hour at minute N" schedule
   (found by diffing the dashboard against Rails on real deployments' data).
5. **`Time.zone.parse` subset** for `?from=&to=` and `after:`/`before:`: ISO
   8601 (with fraction/offset/Z) and "YYYY-MM-DD[ HH:MM[:SS]]". Ruby also
   accepts "Oct 3", "yesterday"-style strings; those fall back to the preset
   window here.
6. **An unknown t-digest encoding** is skipped (its centroids are left out)
   instead of raising; Rails 500s the page. Rows written by RollupJob never
   have one.

## Ruby semantics ported on purpose (and checked)

- `sort_by` with Integer/Float keys uses MRI 4.0.0's own unstable introsort
  (enum.c `rb_uniform_intro_sort_2`), so ties come out in Ruby's order; other
  keys go through glibc 2.41's stable mergesort (`rb.rs`; test reproduces the
  container's tie orders for n = 2..2000).
- `Float#round(n)` is numeric.c's `round_half_up` (2.675.round(2) = 2.68).
- Unordered control scopes follow the index SQLite walks (EXPLAIN on the
  baseline): `issues.where(group_hash:)` by group_hash, `open.where(... LIKE)`
  by last_seen_at, `where(resolved_in_deploy:)` by id.
# Phase 3: control DB, auth, accounts, app shell (p3-control)

Checked against railwatch-cloud @ 464f0fd by: the control schema copied from
the `sqlite_master` of a Rails-created `production.sqlite3`
(`control_schema/rails_primary.sql`); `railwatch import-rails` run on a copy of
rw-baseline's primary DB (65 environments, the bench tokens still authenticate:
`/ingest/ping` with env-0001's Rails token answered 200); bcrypt digests
verified both ways (Rails' `BCrypt::Password` accepts a Rust digest; a Ruby
cost-12 digest is pinned in `control::tests`); the sign-in page object compared
by eye with rw-baseline's (same props, same order, same `onceProps` key); every
cloud-only page driven in a real browser.

## Intentional differences

1. **Sessions and cookies are not Rails'.** `session_token` / `account_id` are
   HMAC-SHA256 signed and `_railwatch_session` (CSRF secret, flash, errors) is
   XChaCha20-Poly1305 encrypted, with keys derived from `secret_key_base`
   (generated into `data/secret_key_base` when not configured). Rails'
   MessageVerifier/encrypted cookies are not readable here, so an imported
   install signs everyone in again (sessions rows are imported; the cookies
   pointing at them are not). Sessions stay permanent with no idle expiry, as
   in Rails.
2. **CSRF.** One per-session token, accepted in `X-XSRF-TOKEN` (Inertia/axios,
   from the `XSRF-TOKEN` cookie) or `X-CSRF-Token` (the gem's beacon, from the
   meta tag), compared in constant time; plus an Origin / `Sec-Fetch-Site`
   same-origin check. Rails masks the token per request; unmasked here. 422 on
   failure, as Rails.
3. **Rate limits on credential endpoints are in memory** (same limits: sign-in
   30/5 min/IP, sign-up 10/h/IP, password reset and verification 20/h/IP,
   provisioning 30/h/user), reset on restart. Rails' `FailOpenRateLimitStore`
   uses Solid Cache.
4. **Sign-up is invite-only by default** (`RAILWATCH_SIGNUP=open` opens it). A
   self-hosted install is private; railwatch-cloud is public.
5. **Quota is not enforced unless `RAILWATCH_ENFORCE_QUOTA=1`.** It is the
   account's (summed from `ingest_usage_months`, as `Account#events_this_month`),
   so Phase 1's per-environment quota (difference 2 above) is gone. Accounts
   created here get plan `self-hosted` and a 10^12 quota; Rails' plan
   validation (`free/team/business`) does not exist.
6. **Account slugs get a numeric suffix** when taken (`acme-2`) instead of
   failing "Slug has already been taken". `parameterize` keeps ASCII letters
   and digits only (Rails transliterates accented Latin letters first).
7. **Signed links** (password reset, email verification) are HMAC tokens
   (`<user>.<expiry>.<mac>`), not Rails' `generates_token_for`. Same lifetimes
   (20 min, 2 days); the reset token is bound to the bcrypt salt, so it dies
   when the password changes, as Rails'.
8. **Email without SMTP is logged, not sent** (WARN, full body). `railwatch
   doctor` says so. Plain-text emails; Rails sends HTML templates.
9. **Not built:** the marketing
   home page (`/` redirects to the dashboard or sign-in), docs, Linear,
   usage page, MCP, `/api/v1`, account deletion's background job (deletion is
   inline). Issues/alerts (`recent_issues`, `open_issues`, `issues_open`) are 0
   or empty until p2-rollups' tables merge.
10. **A failed sign-up from an invitation** returns to the invitation page
    (keeps the invitation props). Rails redirects to `/sign_up`, which drops
    them and shows an empty form.
11. **Action Cable** is the actioncable-v1-json protocol served in process
    (`/cable`): welcome, 3 s pings, subscribe/confirm/reject, re-authorization
    before each message, as EnvironmentChannel. No Solid Cable; one process
    means the 2 s per-environment throttle is global (Rails: per worker). The
    message's `at` is whole seconds (Rails `Time#iso8601`), `counts` is the
    batch's per-type counts.
12. **`import-rails` never opens the Rails file:** it copies the file and its
    WAL to a snapshot beside the control DB and reads that (opening a WAL
    database, even read-only, writes its -shm). Tenant files are adopted by
    symlink, not copied. Tables the control DB lacks are reported, not
    imported (today: issues, issue_activities).
13. **Frontend build.** Plain Vite instead of rails-vite-plugin, assets under
    `/vite/`. One change to cloud's `frontend.config.ts`: the bare-import
    redirect applies to gem files only. Cloud's version also redirects bare
    imports from inside node_modules to the entrypoint, where Node's lookup
    walks above the checkout; a stray `node_modules` in a parent directory changed the
    built chart chunk depending on where the build ran. With the change the
    build is byte-identical across directories.
14. **Security headers on pages** are a fixed subset (X-Frame-Options,
    nosniff, Referrer-Policy, a CSP without script-src). Rails' report-only
    CSP with nonces is not reproduced.
15. **Running behind an app** (`base_path`, `trusted_proxy_secret_file`,
    `standby_relay`; docs/SELF-HOST.md) has no Rails counterpart. All three
    are off by default, and off, every page, prop and response is the same
    as before. With a base path the frontend is built with Vite's relative
    `base: "./"` (asset and font URLs resolve against their own file), and
    the entry script, icons and fonts in the document carry the prefix.

## Google sign-in (`web/google_oauth2.rs`, `web/sessions_oauth.rs`)

OmniAuth's google_oauth2 strategy (omniauth-google-oauth2 1.2.3,
omniauth-oauth2 1.9.0) and SessionsOauthController. Same routes
(`POST /auth/google_oauth2`, `GET /auth/:provider/callback`,
`GET /auth/failure`), the same redirect URI, session keys, failure redirect
(`/auth/failure?message=<key>&strategy=google_oauth2`), flash texts and
User.from_omniauth rules (by provider+uid; else link a *verified*-email
user; else create a verified user, terms accepted; Google's
`email_verified` required for any of it).

Proven by `crates/server/tests/google_sign_in.rs` (the binary against an
in-test mock Google) and by `bench/diff/google/run.sh`: the same 12 flows
through Rails (rw-baseline) and Rust against one mock Google, 9/12
identical; the 3 differences are items 1 and 6 below (both deliberate).

1. **The ID token is verified here; Rails takes the identity from
   userinfo.** A deliberate, stricter choice, not a Rails bug: Rails
   decodes the code exchange's ID token without checking its signature and
   reads the identity from the userinfo endpoint, fetched with the access
   token over TLS. OIDC Core 3.1.3.7 lets a client validate the issuer by
   TLS instead of the signature for an ID token it got directly from the
   token endpoint, so Rails is within the spec and is not changing. Here
   the identity is the ID token's, verified: RS256 only, signature against
   the JWKS (cached an hour; an unknown kid refetches at most once a
   minute; a stale set is served through an outage: the gem's
   `cached_jwks` rules), iss, aud, azp, exp/nbf (60 s leeway), sub, nonce.
   Measured on the mock: a token signed with a key not in the JWKS signs
   in on Rails (whose userinfo the mock serves) and is refused here. No
   userinfo or tokeninfo calls.
2. **PKCE (S256) and an OIDC nonce.** Rails' config sends neither
   (`pkce: false`, no nonce). Both are kept in the encrypted session next
   to `omniauth.state` and consumed by the callback.
3. **Failure keys are coarser.** A rejected token is always
   `message=invalid_credentials`; Rails puts the JWT error's text there
   (`Invalid audience. Expected [...]`). The page shown is the same
   ("Sign in with Google failed. Please try again.").
4. **Reauthentication works on both sides** (no longer a difference;
   Rails fixed 2026-10-16). The "Confirm with Google" button (a copy of the
   cloud's) posts to `/auth/google_oauth2?reauthenticate=<user|account>`:
   OmniAuth stores only the query string (`omniauth.params = request.GET`),
   and the old button's hidden form field never reached the callback, so
   Rails answered "You are already signed in" without confirming
   (`bench/diff/google/rails_reauth.rb` runs both shapes). The request
   phase here reads it from the query (a form field is accepted too).
   Failure: any `/auth/failure` reached by a signed-in user is a failed
   reauthentication, "Google could not confirm your identity. Please try
   again.", back to `/settings/members` for "account", else
   `/settings/profile`. Rails gets "account" from the failure's `origin`;
   here it is kept in the session for one request; same page either way.
   Provider users' destructive actions require that round-trip, as Rails
   (`destructive_authentication_valid?`): the password is not accepted.
   Test: `reauthentication_for_google_users`.
5. **Invite-only sign-up applies to Google too.** With `signup = invite`
   (the self-hosted default, item 4) Google signs existing users in and
   links verified emails, but creates a user only for an email with a
   pending invitation; anyone else is sent to `/sign_in` with "Sign-up is by
   invitation only. Ask an account owner to invite you." With
   `signup = open` it is Rails' behaviour. Creating the user does not accept
   the invitation (as Rails: the invitation link does that).
6. **A new Google user's "Signed in with Google" flash** is not shown: the
   redirect lands on `/dashboard`, which (with no account yet) forwards to
   `/accounts/new`, and the flash does not survive the second hop. Rails
   shows it there. Password sign-up loses its flash the same way here; it is
   the existing redirect chain, not sign-in.
7. **The request phase's CSRF check** is the form's `authenticity_token`
   (or `X-CSRF-Token`) against the session token, plus the same-origin check
   every POST gets; a failure goes to
   `/auth/failure?message=ActionController%3A%3AInvalidAuthenticityToken`,
   as omniauth-rails_csrf_protection. Without credentials,
   `POST /auth/google_oauth2` is a 404 (Rails: no route).
8. **`google_issuer`** (testing only) points the whole flow at another
   OpenID provider (`<issuer>/authorize`, `/token`, `/certs`, `iss` must
   equal it). `railwatch doctor` flags it.
# Phase 2: rollups, grouping, follow-ups, detectors, alerts, watchdog

Checked against railwatch-cloud @ 464f0fd in rw-baseline by:

- `crates/server/tests/tdigest_vs_ruby.rs`: the tdigest 0.2.1 port against
  vectors from the real gem (`bench/diff/tdigest_vectors.rb`): Ruby's MT19937,
  `Array#shuffle`, `as_small_bytes` and percentiles, including the compress that
  fires mid-push on sorted input, and `from_bytes`. Byte-identical.
- `bench/diff/rollup_diff.sh RAW.sqlite3 [seed]`: one raw tenant through
  Rails' RollupJob/ReleaseHealthRollupJob code and the port, both seeded
  `srand(seed + hour index)` before each hour. **rollups, release_health and
  their sqlite_sequence rows identical, row for row; 427/427 and 207/207
  digests byte-identical** (a mixed 2-hour tenant, and a busy hour of 180k
  rows).
- `bench/diff/grouping_diff.sh RAW.sqlite3 [chunk] [deploy_at]`
  (`RESOLVE_AND_REPEAT=1` adds resolve + regress): ExceptionGrouping vs the
  port on 1,500 exceptions over 40 groups with users, browser network errors
  around a deploy, nil files/lines, 300-char messages. **40/40 issues
  identical** (title, culprit, status, occurrences, affected_users, first/last
  seen, source, sample, number order, every activity), regressions included.
- `bench/diff/detectors/run.sh` (2026-10-15): 54 scripted timelines, each
  replayed through railwatch-cloud's own jobs (`rails_runner.rb`: a fresh
  primary file per scenario, `travel_to` at every step, an in-memory job
  adapter, DeliverAlertJob/SummarizeAlertBurstJob only on "deliver" steps)
  and through the ports (`rw-detectors`: `time::travel_to`, the scheduler's
  `scan_targets` + detector, `deliver_pass`), alerts delivered to one HTTPS
  capture webhook that fails on demand. Compared per scenario: every issue
  (title, status, counts, times to the microsecond, sample incl. the
  detection snapshot), issue_activities, alerts (rule, event, status,
  payload, attempts, error, burst/summary links, next_delivery_at), the
  integrations' last_error/last_delivered_at, every webhook delivery (body,
  signature, answer), and IssueDetectionPresenter's output per issue.
  **54/54 identical**: thresholds 11, anomalies 10, missed tasks 7, silent
  host 6, crash-free drop 6, quota 4, issue events (new/seen/resolve/regress/
  ignore/assign/burst/filters/deploy noise) 5, auto-resolve 2, delivery
  retries 3; 48 issues (24 with a snapshot, 25 representative records), 83
  alerts, 85 deliveries per side. No t-digest compress is involved (rollups
  are written with hand-built digests), so no RNG seeding is needed.
- `tests/rollup_incremental.rs`, `tests/background.rs`, unit tests: the
  incremental path equals a full recompute; alert filters, dedupe, burst
  summary; outbox lease, retry delay, private-address refusal; watchdog;
  exactly-once claims and the GC floor; thresholds off real rollups.

## Rails-vs-Rust timings (workstation, busy, numbers from one session)

| what | Rails | Rust |
|---|---|---|
| RollupJob, one busy hour (180k raw rows, 207 groups) | 1,415 ms | 295 ms (rw-rollup, cold) / 470 ms (warm, repeated) |
| Keeping that hour current while it fills (360 batches, flush every 10) | 36 full recomputes: 15.1 s (Rust's own full path; Rails would be ~4.8x that) | incremental: 3.9 s total |

## Deliberate differences

1. **Digest RNG.** tdigest's `compress!` shuffles with Ruby's process-global
   RNG, which other code also draws from (the first query of a Rails process
   takes 2 MT words), so production Rails digests depend on that history and
   no two Rails runs agree byte for byte either. The server seeds a Ruby MT
   per (record_type, group_hash, hour) instead, so a group's digest depends
   only on its rows. Equivalence runs use `--seed` (one stream per hour,
   as the Rails script sets it) and match byte for byte. Percentiles from
   either are valid t-digest readings of the same data.
2. **Incremental rollups** (the big lever). Committed row ids are folded into
   per-(type, group, hour) sets; every 5 s only the groups that changed are
   recomputed, with RollupJob's own aggregation over the group's complete
   rows (cold groups are loaded once from the group_hash index). Same rows,
   same order, same function: identical to a full recompute
   (tests/rollup_incremental.rs). Rails recomputes the whole hour per touched
   hour per minute. The full recompute stays as RollupCatchupJob (every
   5 min, current + previous hour) and for receipts committed before a
   restart. Above 128 MB of accumulated rows per tenant (~200k rows) it
   lets go of the hours it holds and recomputes them in full at each flush
   instead (same output; logged). Memory is bounded by that cap; before it,
   the cap was 3M rows (~2 GB).
3. **Reads on a read-only connection.** All background reads use a separate
   `SQLITE_OPEN_READ_ONLY` connection; under WAL they never take or wait for
   the writer's lock. Writes are RollupJob's (one IMMEDIATE transaction per
   type).
4. **No job queue.** One worker thread per process owns the per-tenant
   background work; a tokio scheduler runs recurring.yml's cadences
   (jobs/mod.rs has the table). Alert delivery is a loop every 5 s instead of
   DeliverAlertJob + a 1-minute reconciler; the outbox states, lease (5 min),
   attempts (5) and retry delays (1/5/15/60 min) are Rails'.
5. **Receipted usage** is counted once at commit (Phase 1, unchanged); the
   drain does not count it again. The drain does the grouping (claimed per
   group, exactly once) and clears the follow-ups; it recomputes hours only
   for receipts committed before this process started (those may have
   missed the in-memory accumulator). Rails' drain had a late-row bug: it
   skipped an hour for a receipt that committed after that hour's recompute
   read the rows, both within a pass (its `recomputed` set) and across
   passes (it compared `committed_at` with the recompute's start time), so
   the hour kept a short count (seen once on a real deployment: 1 counted,
   2 rows). Fixed in Rails 2026-10-16 with a receipt-id watermark. It cannot happen here: every recomputed receipt committed
   before the first pass, and later ones are counted by the accumulator
   (tests `a_late_row_for_an_old_rolled_up_hour_is_counted`,
   `the_drain_recomputes_a_pre_start_hour_once_and_counts_every_receipts_rows`;
   reasoning in jobs/drain.rs).
6. **Grouping filters rows with no group_hash on the direct path too, as
   Rails now does** (2026-10-16). Rails' GroupExceptionsJob used to pass
   them to `record_occurrence!`, which raised "Group hash can't be blank"
   and failed the job, so every later group in that batch was never
   grouped (the receipted path already filtered them). Rails now filters
   them before grouping too; not a difference.
7. **Control schema.** The 10 tables in `control_schema/issues.rs` use Rails'
   exact DDL from rw-baseline's production.sqlite3. Foreign keys are switched
   OFF on the control connection until p3-control adds users, applications
   and accounts (rusqlite's bundled SQLite enables them by default and every
   write would fail on the missing tables). Until applications exist an
   environment is its own application: issue numbers count per environment,
   keys are `APP-<n>`, alert rules match on `application_id = environment id`.
8. **Quota and auto-resolve are per account, as in Rails** (since the
   detectors diff, 2026-10-15): QuotaCheckJob's dates live on the account
   (`quota_alerted_80_at`/`_100_at`, reset when the month rolls over) and
   AutoResolveIssuesJob reads `accounts.auto_resolve_after_days` (nil = off;
   the application settings page edits it). `RAILWATCH_AUTO_RESOLVE_DAYS`
   is gone. Before, both were per environment, the quota dates in memory.
9. **title_pattern regexes** use Rust's linear-time regex engine
   (case-insensitive), so Ruby's 10 ms backtracking timeout has nothing to
   catch; patterns needing backtracking features (lookaround,
   backreferences) fail to compile and fall back to a substring match on the
   pattern's source, as Rails does for an unparsable pattern.
10. **Delivery.** Webhook body, `X-Railwatch-Signature: sha256=<hex HMAC>`,
    Slack Block Kit and email text follow Alerts::Delivery. Links use the
    server's configured `url` (`RAILWATCH_PUBLIC_URL` only where no server is
    configured, i.e. tests). Email alerts go through the same SMTP settings as
    sign-in mail (`smtp_*`); without SMTP they fail visibly. Integration
    secrets: see "Gaps" below. **Linear is not ported**: such alerts fail
    with a message saying so. OutboundHttp's policy is ported (HTTPS, no
    credentials/fragment, every resolved address public, connection pinned
    to the checked address, 1 MiB, 15 s); redirects are not followed.
11. **Scheduled-task schedules** are parsed by croner from cron (5/6 fields,
    trailing `UTC`) and fugit's common "every ..." forms. A schedule with a
    non-UTC zone or another natural-language form is skipped (fugit would
    parse it), so such a task is never flagged as missed.
12. **Detection snapshots are captured** (`detection/`, since 2026-10-15):
    IssueDetectionSnapshot.capture on a new/regressed threshold, anomaly or
    missed-task issue, carried forward on `:seen` for thresholds and
    anomalies (DetectionSnapshotting), and IssueDetectionPresenter renders
   them on the issue page and in MCP get_issue. Identical in the detectors
   diff (snapshot and presenter output). Missed tasks carry it forward too
   (a Rails bug fixed on both sides, see "Former Rails bugs" below).
13. **Not ported**: OptimizeTelemetryJob, BackupTelemetryJob,
    RestoreVerificationJob, OperationsStatusJob, PruneFailedExecutionsJob,
    WeeklyDigestJob, the Linear jobs, Solid Cable/Queue trims.

## Deliberate addition: the environment watchdog

The reason railwatch-rs runs apart from the monitored app: when the app goes
quiet, Railwatch still alerts. Rails' SilentHostCheckJob only covers
Kamal-declared servers and waits 10 minutes. Here:

- Every committed batch updates last-seen per **(environment, server)** in
  memory, from its rows' `server` column; no database read on the ingest
  path. Keyed per host, not per process, so restarts do not look like gaps
  (measured on real health streams by the research track: worst per-host gap
  10.6 s, none over 30 s).
- Every 10 s: a pair silent for more than `RAILWATCH_SILENCE_SECONDS`
  (default 60, minimum 30) fires one **`silent_environment`** alert through
  the normal alert rules (event name added to the rule events); data
  returning fires one **`environment_recovered`**. Paused environments are
  not alerted; pairs quiet for over a day stop being watched.
- State is persisted in the control table `environment_heartbeats`, so a
  restart neither forgets a silence nor alerts it twice.
- **Dead-man switch**: with `RAILWATCH_DEADMAN_URL`, it pings every 30 s, but
  only while the watchdog loop runs, a control-DB write succeeds, and (if
  anything committed in the last 10 min) a commit landed in the last 2 min.
  A wedged Railwatch stops pinging, the same as a dead one. If the alert
  outbox has held an undelivered alert for 10 minutes, or the delivery loop
  stalls, it pings `<url>/fail` (healthchecks.io convention).

Not done: deriving the threshold from each environment's own batch rate (the
brief's suggestion). The fixed 60 s default came from the research
measurement instead; a low-traffic environment with no health records would
need a longer `RAILWATCH_SILENCE_SECONDS`.

# Gaps: issues, alerts, integrations, install (gaps)

Checked by: unit tests in `web/integrations.rs`, `web/alerts.rs`,
`alerts/integration.rs`, `alerts/outbound_http.rs`, `tests/background.rs`; the
real React pages driven in headless Chromium (every Settings > Integrations
action, /alerts retry); `bench/alerts/silent_proof.sh` (HTTPS receiver,
signatures verified); `bench/restart/run.sh`; `deploy/install.sh` in a
systemd container; `bench/diff/run.sh` and `bench/diff/props/run.sh` after
all of it (identical; 130/130).

## Alerts and integrations

1. **Integration secrets are encrypted with XChaCha20-Poly1305**, not Active
   Record encryption. Same split as Rails (SENSITIVE_SETTING_KEYS to
   `encrypted_settings`, the rest to `settings`); the key is HMAC-SHA256
   (secret_key_base, "railwatch integration settings"); stored as
   `rwenc:v1:<base64url(nonce || ciphertext)>`. `import-rails` given Rails'
   `active_record_encryption` primary_key and key_derivation_salt
   (`--ar-primary-key`/`--ar-key-derivation-salt` or the
   `ACTIVE_RECORD_ENCRYPTION_*` env vars) decrypts each Rails-encrypted
   column (PBKDF2-HMAC-SHA256 x 65536, AES-256-GCM, zlib when `c`;
   active_record_encryption.rs) and re-encrypts it here, so nothing needs
   re-entering (bench/install/ar_import_proof.sh: three integrations Rails
   saved with its own keys read back identical). Without the keys a
   Rails-encrypted column (`{"p":..,"h":..}`) is copied as is, listed as
   UNREADABLE by the import and by `railwatch integrations`, and its alerts
   fail with that reason until the secret is re-entered. Plain-JSON
   `encrypted_settings` (older Rust builds) still reads.
2. **Linear is not offered** (`kinds` is email/slack/webhook,
   `linear_oauth_enabled` false, `webhook_health` null).
3. **The env alerts page also shows issue-less alerts whose payload names
   the environment** (`environment_id`: silent_environment,
   environment_recovered, silent_host, crash_free_drop, quota). Rails scopes
   that page through the alert's issue, so those appeared only account-wide;
   "your app went silent" belongs on that app's page.
4. **Retry schedule vs the reconcile window.** Rails' 1-hour
   RECONCILE_WINDOW bounds only its reconciler; an alert in its retry
   schedule has a job queued and is delivered at any age. With no job queue,
   the claim lets alerts with attempts > 0, or explicitly retried ones,
   through the window (otherwise attempt 5, due ~81 min after creation, was
   silently never made). Retry marks `delivery_enqueued_until = now` for
   this.
5. **Send test** creates the alert already holding a delivery lease, so the
   5-second delivery loop cannot deliver it a second time while the request
   delivers it synchronously (Rails suppresses its enqueue).
6. **Event labels**: the integrations page labels the two watchdog events
   ("App stopped reporting", "App reporting again"); a cloud-only page, so a
   frontend edit.
7. **`RAILWATCH_ALLOW_PRIVATE_ALERT_TARGETS=1`** (off by default) skips the
   public-address check, at validation and at connect; HTTPS, no
   credentials/fragment and the Slack host rule still apply. It re-opens
   SSRF to anyone who can edit an integration. **`RAILWATCH_ALERT_CA_FILE`**
   adds a PEM bundle to the trusted roots; an unreadable file fails the
   delivery with its path.
8. **`railwatch setup --alert-webhook/--alert-slack/--alert-email`** creates
   the integration and rules for new_issue, regressed_issue,
   silent_environment, environment_recovered (no Rails counterpart).

## Usage and quota

9. **Unenforced quota renders as "no event quota".** Self-hosted, quota is
   only enforced with `RAILWATCH_ENFORCE_QUOTA=1`; otherwise the dashboard's
   `usage.quota` and usage's `account.monthly_event_quota` are null, and the
   two cloud-only pages (dashboard, usage) say "no quota on self-hosted" /
   "no event quota" instead of "of 1000000.0M". Rails has no unlimited plan
   (its largest is 250M), so this is a frontend edit, not a ported prop.
10. **Usage's control props** (`account`, `environment_events`, `db_bytes`)
    are now sent; before, the page read the shared `account` prop, which has
    no quota. `environment_events` is this month's ledger row (Rails'
    `environment.events_this_month`, the cached counter; same number when
    the counter is current).

## Tenant files

11. **New tenant files get the two unused `queries` indexes as partial
    indexes** (`idx_queries_tenant_duration_cursor`,
    `idx_queries_duration_cursor`, `WHERE app_tenant IS NOT NULL`). No gem
    0.9.1 read uses them (research/indexes). Existing files are never
    altered. Rails opens such a file with 0 pending migrations and
    reads/writes it (cross_open.sh); the gem's queries plans are unchanged.
    Measured on a 538k-query tenant: 8 KB instead of 16.6 MB (3.1% of the
    540 MB file); ingest throughput unchanged within noise (23.8k/23.8k
    vs 26.4k/23.8k ev/s). `compare.py` accepts exactly these two
    sqlite_master rows. Not checked: a future gem migration that recreates
    either index.
12. **A size budget and a free-space floor** (`max_data_bytes`,
    `min_free_bytes`, off by default; storage/budget.rs). Rails bounds
    telemetry by retention only. With the budget on, raw rows can go before
    their retention ends (oldest day files first, logged), and under the
    floor, or over the budget with only today's files left, ingest answers
    503 + Retry-After, which Rails never does for disk space. Off, nothing
    changes.

## Process

12. **Socket activation.** With `LISTEN_FDS=1` or `2` (and `LISTEN_PID`
    this process, when set) `serve` takes fds 3.. as its listeners and
    ignores `RAILWATCH_BIND`; with two, the one on port 80 serves the
    https redirect. Another count is refused at startup.
    `railwatch.socket` (written by deploy/install.sh) holds the sockets.
14. **TLS is built in** (`tls_domain`, tls.rs): Rails cloud runs behind
    kamal-proxy, which terminates TLS. Here the binary terminates it and
    gets its certificate by ACME TLS-ALPN-01 (rustls-acme, cached in
    `<data_dir>/acme`), so a self-hosted install needs no proxy package;
    port 80 redirects (308). Plain HTTP behind a proxy is unchanged. Why
    not Caddy: docs/SELF-HOST.md, "HTTPS: why it is built in". Measured:
    issuance in a 3.6 s install and an in-process renewal against Pebble
    (bench/install/pebble_proof.sh); 0 of 641 posts dropped across 4
    upgrades over HTTPS (bench/install/upgrade_proof.sh).
13. **Ack guarantee** (unchanged, now documented in README): 200 means
    committed with `synchronous=NORMAL` in WAL mode. That survives a process
    crash, not power loss or a kernel panic; the gem's receipted path is the
    answer for apps that need more.

## Weekly digest (`jobs/weekly_digest.rs`, `web/digest_mailer.rs`)

WeeklyDigestJob + DigestMailer#weekly. Checked by `bench/diff/digest/run.sh`:
Rails renders every digest a run would send under `travel_to(NOW)`
(`capture_rails.rb`), Rust renders from an `import-rails` copy of the same
primary and tenant files (`digest-dump`), and subject, recipients, text and
HTML are compared byte for byte; only the server's base URL is normalised.
`setup_rails.rb` adds the edge cases (a member with the digest off, an
account with no recipients, an empty and a stale tenant, singular counts,
HTML-special characters, a title over 120 characters, a resolved issue).
Last runs (2026-10-16): **3/3 mails identical** (181 environment sections)
at a Monday after all data, the same with `SPLIT=1` (per-day layout), and
**2/2** (48 sections) at a later NOW whose window cuts the data (`last-diff.txt`).

1. **Delivery.** Rails enqueues one `deliver_later` per recipient; a
   failed send lands in Solid Queue's failed jobs, not retried. Here the
   run sends them in turn and logs each failure at ERROR with the
   recipient, not retried either. With
   `alert_delivery = off` or without SMTP nothing is sent and the run logs
   how many digests it withheld (Rails has no such switch; a shadow must
   not email people). From address: `smtp_from` (Rails: `SMTP_FROM_EMAIL`).
2. **Environment order.** Rails' `account.environments` has no ORDER BY;
   its plan yields application id, then environment id. The control DB has
   the same indexes created in another order, so SQLite picks other ones;
   the order is spelled out here (same result).
3. **Missed runs** are not caught up, as with Solid Queue's recurring
   tasks; the schedule is UTC, as Rails' (no `config.time_zone`).
4. **`railwatch digest send --dry-run [--account ID]`** is new: it prints
   what a run now would send.

## Issues (`web/issues.rs`, IssuesController + CommentsController)

Read side checked against rw-baseline by `bench/diff/issues/run.sh` (on a
props-diff work dir: Rails' own primary DB imported, same tenant file):
**36/36 cases identical** (9 index variants over 239 issues / 200 listed,
12 exception issues' show with occurrences, breadcrumbs, daily, activities,
12+12 merge_candidates with and without q); shared session props excluded.
Not covered by that diff: the write actions (update, bulk, merge/split,
comments; unit-tested only), show for performance/anomaly issues, resolved
or merged issues.

Ported: index (filters, counts, kindCounts, permitted filters echo,
members), show, update (resolve/ignore/reopen/assign/assign_me/priority/
merge/unmerge/split/title), bulk (200 ids, bulk_merge), merge_candidates
(JSON), comments. The lifecycle rules live in `grouping/issue.rs` next to
record_occurrence!. Differences from Rails:

14. **Detection is IssueDetectionPresenter's** (Phase 2 item 12): a
   snapshot (Rails- or Rust-captured) renders with live trend, deploy
   comparison and representatives; none, or one failing validation, renders
   `unavailable_json`. The issues diff's alerts list also now orders ties in
   `created_at` by id ascending, the order SQLite gives Rails'
   `order(created_at: :desc)` over the issue_id index (was id descending;
   found by the issues diff, 39/42 -> 42/42).
15. **Linear sync is not ported.** No LinearSyncJob/LinearCommentSyncJob on
   status, assignee or comment changes (enqueue_linear_changes,
   enqueue_linear_sync); `linear_issue_links` is not read.
16. **A missing tenant file renders as an empty tenant.** Show reads the
   telemetry half from an in-memory schema-only database and leaves the
   disk alone (what Rails' with_telemetry does with a missing file was not
   checked against rw-baseline).
   Split with no tenant file finds no rows: "Nothing to split".
17. **LIKE has an explicit `ESCAPE '\'`, as Rails now does** (fixed on
   both sides, 2026-10-16). Rails' raw `title LIKE ?` with
   sanitize_sql_like had no ESCAPE clause, which on SQLite leaves `\` as a
   literal, so a search containing `%` or `_` matched the escaped text.
   Both now escape the cloud controllers' LIKEs: issues index q, merge
   candidates, related_issues, API `issues?q=`, and the dashboard pages
   that mirror cloud controllers (deprecations `message`, outgoing requests
   `host`, LLM calls `model` and `attachment_types`;
   `text_filters_match_percent_and_underscore_literally`). **Still
   unescaped, as in the gem (unchanged):** the gem-owned FilterQuery's
   `source:` prefix and exceptions/logs `message`/`class_name` text
   (`dashboard/filter_query.rs`), and the tenants page's `app_tenant`
   (`dashboard/tenant.rs`); there a `%` or `_` still matches only a
   backslash sequence, like the gem's dashboard.
18. **Detection issues' comments fall back like exception issues', as
   Rails now does** (2026-10-16): render_detection_issue used to read
   `c.user.name`, which raised NoMethodError for a Linear comment (no
   user); both now fall back to author_name, then "Linear". Same keys
   (no `source`); not a difference.
19. **Invalid writes answer 422 without a page.** `update!` raising
   RecordInvalid (unknown priority, blank title) is a 422 "Unprocessable
   Content" body; Rails renders its error page. A bulk priority with an
   unknown value saves nothing (Rails raises on the first issue too).
20. **Unordered scopes are ordered.** `members` (account.users) is by
   membership id, related_issues and merged_issues by id; Rails leaves
   these to SQLite's plan (not compared against rw-baseline).

# Storage: per-day tenants (branch `storage`, docs/STORAGE.md)

Proven by: `SPLIT=1 bench/diff/props/run.sh` 130/130, `SPLIT=1
bench/diff/issues/run.sh` 42/42 (and 42/42 single-file; the earlier 40/42,
see STORAGE.md, was data drift, not storage),
`bench/storage/pages.sh` 122/122 single vs per-day, `tests/storage_crash.rs`,
`bench/storage/minio_restore.sh`.

1. **An environment is a directory, not one file**, for new environments:
   `meta.sqlite3` + `core/<day>.sqlite3` + `detail/<day>.sqlite3`, each a
   full gem-schema file holding its share of the rows. Rails reads any one
   file, not the set; `railwatch export` merges them into one Rails file.
   Existing single files are untouched until `railwatch split`.
2. **Ids are environment-wide**, continued through meta's sqlite_sequence,
   so ids match what one file would have assigned.
3. **Retention deletes whole day files.** Rails deletes rows older than
   `now - retention_days` (to the second) in 5,000-row batches. Here a day
   goes when all of it is past the cutoff, so up to one extra day of rows
   is kept; `raw_pruned_through` is the day boundary. Rails' 30-day
   retention of everything becomes: detail 7 days, core = retention_days
   (cap 90), rollups 13 months (unchanged).
4. **Exemplars.** Rails has no tiers, so nothing to keep. Here, after 7
   days a trace opens with its child rows only if its execution was among
   the 3 slowest of its (kind, group, hour) or errored; other executions
   older than 7 days show no queries, cache events, view renders, logs or
   spans. Their rollups are unaffected (computed at ingest).
5. **query_shapes are not pruned** when their queries go (Rails' NOT EXISTS
   is one file; across days it would scan every detail file). Shapes are
   deduplicated and small. Not measured on a long-lived tenant.
6. **people** are pruned by `last_seen_at` against the core cutoff (Rails:
   the row retention cutoff), same rule otherwise. `ingest_batches`,
   `processes`, `health_samples` go with their core day file instead of a
   DELETE by time.
7. **Per-day reads are rewritten, not just unioned.** INDEXED BY / NOT
   INDEXED go to hinted views (a view can't take them), and the statements
   that were slow over the union run inside each day file and are combined:
   cursor pages take each file's own top N (`idx_queries_slowest` only in
   files whose whole day is in the window), log text filters use each file's
   own FTS index, and COUNT / logs' level counts / queue stats / active
   people are computed per file and summed (AVG as SUM/COUNT, exact for the
   integer column). Same rows and order as one file:
   `bench/storage/slow_pages.sh` 383/383 cases byte-identical single vs per-day, and
   `per_day_pages_equal_the_single_file` (docs/PERF.md, "Slow dashboard
   pages").
8. **Profile links across days**: `link_profiles!` looks for an
   execution's profiles in the core files of the days the batch writes and
   the day either side of each. An execution whose profile arrives more
   than a day away (by occurred_at) is not linked. Rails, one file, always
   links.
9. **Partial commits are repaired.** A crash between day-file commits and
   meta's commit leaves rows that recovery deletes at the next writer open
   (logged as a warning with the row count). Rails has one file and no
   such state.
10. **Backups to S3** (off by default) are new: Rails cloud snapshots
    uncompressed `VACUUM INTO` files to its own bucket layout. Here zstd -3
    snapshots via the SQLite backup API, closed days uploaded once;
    today's and yesterday's day files are only as current as the last
    restore point before they close (2 days). `railwatch restore` is new.
11. **Usage `db_bytes`** is the sum over the environment's files (and their
    -wal), not one file's size.
12. **Bundled SQLite is built with SQLITE_MAX_ATTACHED=125** (default 10).
13. **A late row for a day whose detail file is already gone** (possible
    only between 7 days and the ingest floor) recreates that detail day,
    and the next prune deletes it without copying exemplars (the core day
    is already marked done). Not measured how often the gem sends rows that
    late; its buffer holds minutes, its export outbox up to 24 h.

# MCP server (`web/mcp.rs`, McpController + ApiTokenAuthenticated)

Proven by `bench/diff/mcp/run.sh` on a props-diff work dir: six API tokens
issued in Rails (full, read-only, api-only, mcp-without-read, revoked,
restricted to a second application) and seeded alerts, Rails' primary
imported into a fresh Rust data dir (same token digests, issue ids,
users), every case POSTed through the real McpController under
`travel_to(NOW)` and through `web::mcp::call` (`mcp-dump`) at the same NOW,
responses compared value for value with key order (a tool's text is parsed
first, so a difference points inside it). **154/154 identical** since the
detection port (was 152/154; 1 below). 84/84 identical on the
1.9M-row tenant (`perf.sh`). The write cases' `issue_activities`
and `comments` match row for row (kind, user, data). Cases: all 17 tools
(windows, filters, cursors, by-name/by-hash/by-id identifiers), the 3
prompts with and without optional arguments, resources list/read
(applications, both summary URI forms, docs incl. case-folded names),
initialize/tools/list (with and without write scope), and the error paths:
unknown app, environment, issue, route, profile, execution, query group,
tool, prompt, resource and method; bad resource, cursor, application_id,
window and limit; missing arguments; write without write scope; no, garbage,
lowercase-bearer and revoked tokens; tokens without mcp or read; malformed,
empty, form-encoded, string, array, null and number bodies, and non-object
params. Ported as-is: names, input schemas
and descriptions are Rails' own constants (dumped to `mcp_definitions.json`),
the prompt texts are Rails' heredocs, the docs are the cloud's `docs/gem`.

Not covered by that diff: profiles with more than a few frames (the props tenant's
are tiny; `hot_frames` is unit-tested against Ruby's semantics), and an
AmbiguousIdentifier (the baseline has one account; the SQL is Rails'
`limit(2)` shape, not exercised against Rails).

1. **Detection** is IssueDetectionPresenter's (issues #14); its
   representative records' `occurred_at` is written as JSON.generate writes
   a Time. The 2 get_issue cases that differed before the port now match:
   **154/154** (2026-10-15).
2. **No Linear sync on MCP writes** (issues #15): update_issue/add_comment
   enqueue no LinearSyncJob/LinearCommentSyncJob.
3. **An unexpected error is logged with `tracing`**, not `Rails.error.report`
   (the client still gets -32603 "Internal error"). Body and params
   handling now match fixed Rails (2026-10-16; both used to answer the HTML
   500 page for a JSON array or number body or a non-Hash `params` in
   prompts/get, and Rails ignored a form-encoded body): the raw body is
   parsed as JSON whatever the Content-Type; valid JSON that is not an
   object is 400 `{"jsonrpc":"2.0","id":null,"error":{"code":-32600,
   "message":"Invalid Request"}}`; `params` present, not null and not an
   object, for any method, is 200 -32602 "Invalid params"; malformed JSON
   is still 400 -32700. Test
   `the_body_is_json_whatever_its_type_and_must_be_an_object_with_object_params`;
   the MCP comparer lists the parse/params cases as EXPECTED vs the old
   baseline. A plain "Internal Server Error" 500 remains only for an
   unexpected failure outside a tool.
4. **The preauth rate limit is per process**, in memory (FixedWindow, as the
   other limits here); Rails' `rate_limit` uses Solid Cache, shared across
   Puma workers.
5. **Discovery URLs** (`endpoint`, `tokenUrl`) use the request's Host header
   with the scheme of `RAILWATCH_URL`; Rails' `mcp_url` uses the request's
   own protocol (behind a TLS proxy both say https).
6. **Body size**: a POST over 4 MiB is refused with 413 `{"error": "body too
   large"}` before parsing; Rails reads any size.

# Detectors diff (`bench/diff/detectors/`, 2026-10-15)

What `run.sh` found and fixed in the ports (each was a Rust difference,
Rails kept): scheduled tasks evaluated in sorted key order instead of
discovery order (issue numbers differed); the `tokens` threshold value was a
Float (title "(1500.0)", Rails "(1500)"); hand-rolled rounding instead of
`Float#round` (round_half_up); silent-host and crash-free messages used the
environment name instead of `display_name` ("Shop / production"); a NULL
`crash_free_threshold` defaulted to 99.0 (Rails: check off); the 24-hour
`last_seen_at` bound was exclusive for silent-host/release-health scans
(Rails' range is inclusive; the scheduled-task scan's `>` is exclusive, as
before); quota per environment in memory with an extra `environment_id`
(see Phase 2 item 8); auto-resolve from an env var; delivery errors stored
without Ruby's exception class ("RuntimeError: 500 from host").

Harness only (not differences): the Rails runner creates each scenario's
rows on the real clock (creating a tenant runs migrations, which refuse a
"now" before their timestamps), saves rules with `validate: false` (an
unparsable title_pattern stands for a rule saved before validation existed)
and stubs `OutboundHttp.ensure_public!` (the receiver is on the docker
bridge), which is what `RAILWATCH_ALLOW_PRIVATE_ALERT_TARGETS=1` does here.

## Former Rails bugs (fixed in Rails and here, 2026-10-16)

rw-baseline still runs the old Rails image, so these scenarios differ from
it; `compare.py` lists them as EXPECTED (not counted) until the baseline is
rebuilt from the fixed railwatch-cloud.

1. **A still-missed scheduled task keeps its detection.** CheckScheduledTasksJob
   used to write the snapshot only on a new/regressed outcome, while
   record_occurrence! replaces `sample` on every occurrence, so the next
   10-minute scan (`:seen`) dropped `sample["detection"]` and the issue page
   said "not captured" while the task was still missing. Both now wrap the
   capture in carry_detection, as thresholds and anomalies do (scenario
   `missed_task_still_missed_seen`; test
   `a_still_missed_scheduled_task_keeps_its_detection_snapshot`).
2. **QuotaCheckJob honours rule filters.** A quota rule fires only when some
   environment of its own application matches it (`rule.matches?` with that
   environment_id); the payload still has no `environment_id`. So a rule
   narrowed to an environment that is not its application's never fires
   (`quota_80_then_100`: the `environment_ids: [999]` rule used to fire
   twice); test
   `quota_alerts_fire_only_for_rules_whose_environment_filter_matches_their_application`.
3. **`related_issues` LIKE without ESCAPE** (issues #17) was what made the
   old 40/42: rw-baseline-jobs' detectors opened "Scheduled task
   digest_daily missed ..." issues in other bench environments, and old
   Rails' `title LIKE 'Scheduled task digest\_daily missed...:%'` matched
   none of them (the `\_` is literal on SQLite), while Rust's ESCAPE clause
   matched two. Rails now has the ESCAPE clause too, so on a rebuilt
   baseline that case matches; against the old image it can still differ
   when such issues share a prefix (today's 42/42 is because they do not).

# JSON API (`web/api.rs`, Api::BaseController + every Api::V1 controller)

Every /api/v1 endpoint is ported with Rails' paths (plus `(.:format)`, a
trailing slash and repeated slashes, as Journey routes them), verbs
(PATCH and PUT for issues#update), params, status codes, JSON shapes and
key order, and error bodies. It shares MCP's token code (`mcp::Token`:
lookup, `record_use!`, the accessible-account/application scopes), as
ApiTokenAuthenticated is one concern in Rails. Times are `as_json`
(MCP's tool texts are `Time#to_s`).

Proven by `bench/diff/api/run.sh` on a props-diff work dir: eight tokens
issued in Rails (full, read-only, MCP-only, no-read, revoked, expired,
restricted to a second application, restricted to the account), a second
member to assign, named deploys, a merged issue, a Linear comment and a
performance issue whose detection snapshot Rails' own DetectPerformanceIssuesJob
captured; Rails' primary imported into a fresh Rust data dir; every case
sent through Rails' whole middleware stack (Integration::Session) under
`travel_to(NOW)` and through `web::api::call` (`api-dump`) at the same NOW.
Compared: status, the body value for value with key order (an HTML error
page by its title), and the headers a client reads (content-type,
content-length, cache-control, etag, the security headers). **198/198
identical** single-file and **198/198** per-day (`SPLIT=1`), 2026-10-16
(before the Rails fixes below; the cases they change are listed as
EXPECTED in `compare.py` until rw-baseline is rebuilt);
the writes' issue activities (kind, user, data, created_at), comments,
issues (status, priority, assignee, resolved_at, merged_into_id,
occurrences, updated_at) and deploys (ref, name, url, previous_ref,
deployed_at, commits) match row for row. Cases: all 24 endpoints with their
windows, sorts, filters, `q` grammars, pages and cursors (chained to page 2
on each side's own cursor); 401 (none, garbage, lowercase `bearer`, two
words, revoked, expired), 403 (MCP-only, no read, write without write
scope, ahead of the 404 for an unknown environment), 404 (unknown and
another app's records, a token's restriction, `abc`/`1abc`/out-of-range
ids), 422 (series type and step, cursors forged, from another resource or
another query, bad status/priority, blank body, bad deploy url), 400
(malformed JSON, `params.require(:deploy)`), `page[]=1` (the 500 page
then; see below), 429 after 600 requests, unrouted paths and verbs (HTML and JSON 404
pages by format and Accept), form and array bodies, query-over-body
precedence, HEAD, If-None-Match.

Latency (`bench/diff/api/perf.sh`, 2026-10-16): a 9-day tenant of 1.66M
rows (rw-load --spread-days 9, Rails' own rollups via settle_rails.rb),
123 read cases timed through Rails' integration session vs `web::api::call`
(no socket either side), median of 3 warm runs, Rails.cache cleared per
run: **123/123 identical** in both layouts; summed **Rails 5.8 s, Rust
1.38 s single-file (4.2x), 2.90 s per-day (2.0x)**. By endpoint (median
case): control-DB endpoints (me, accounts, applications, environment,
deploys, releases, issues, issue) 11-400x; summary 21x; series 13x;
requests 9x; queries 7x (739 vs 103 ms: the 30d cursor page over 900k
queries); jobs 4x; execution 30x; request groups, users, tenants, logs
1.5-2x (the raw COUNT/GROUP BY over 30 days dominates both). These
timings predate the exceptions/logs fix (item 6 below): a request with only
`limit`/`cursor` or only `page`/`per_page` now skips the other pagination's
query (and the COUNT); not re-measured. Per-day is
slower than single-file on full-text log search (`logs.text`: Rails 196,
single 131, per-day 560 ms: FTS runs per day file) and on queries with a
filter (`queries.adapter`: 974 / 140 / 397 ms); `timings-large.md` has
every case.

Normalized by the comparer (and only these): `x-request-id`, `x-runtime`,
`strict-transport-security`, `vary` and the report-only CSP (item 1); ids
a write assigns; cursors (each server signs with its own key: compared for
presence, then followed on each side).

1. **Response headers are a subset.** The same security headers as Rails'
   controller responses (X-Frame-Options, X-XSS-Protection,
   X-Content-Type-Options, X-Permitted-Cross-Domain-Policies,
   Referrer-Policy, the enforced CSP baseline) and Rack::ETag's weak ETag
   (same digest) with Cache-Control, but no `x-request-id`, `x-runtime`,
   `vary: Accept` (Rails adds it when a non-browser Accept header is
   sent), `strict-transport-security` (Rails' `force_ssl`; terminate TLS
   in front or use built-in TLS) or report-only CSP with a per-request
   nonce (as pages, Phase 3 item 14).
2. **The rate limit is per process**, in memory (600/min per user per
   controller, Rails' key), as the MCP and ingest limits here; Rails' is in
   Solid Cache, shared across Puma workers.
3. **Bodies over 4 MiB** are refused with 413 `{"error":"body too
   large"}` before parsing; Rails reads any size.
4. **No Linear sync** on API writes (as MCP item 2, issues #15).
5. **An unexpected error** (a database failure) is logged with `tracing`
   and answered with Rails' public 500 page or `{"status":500,...}`, as
   Rails' exceptions app would render it; nothing goes to an error tracker.
6. **Each issue update step commits on its own**, as Rails' separate
   `update!` calls do: `{"priority":"urgent","assignee_id":<outsider>}`
   saves the priority and then answers 404 (diffed:
   `write.update.priority_then_bad_assignee`).

## Rails bugs fixed on both sides (2026-10-16)

These were Rails behaviour the port kept and the diff pinned; Rails fixes
them in the same change, so they are not differences. Against the old
rw-baseline image the affected cases print as EXPECTED (see
`bench/diff/api/last-diff.txt`). Tests in `web/api.rs`.

1. **API writes are attributed.** Every Api::V1 write runs as the token's
   user (`Current.api_user`, as MCP): the status, priority and assignee
   activities carry its user_id and the alerts they fire its name as
   `actor` (`writes_need_write_scope_and_leave_rails_activities`,
   `api_writes_are_attributed_to_the_token_user_in_alerts`).
2. **`issues?q=` LIKE has `ESCAPE '\'`** (with issues #17): `q=100%`
   finds only a title with a literal `100%`, `q=_` only titles with an
   underscore, `digest_daily` not `digestXdaily`
   (`issues_q_matches_percent_and_underscore_literally`).
3. **Non-scalar `page`, `per_page`, `limit`** (`page[]=1`) read as absent
   (1, 25, 50): a normal 200, not the 500 page; a string or a JSON number
   is read as before
   (`non_scalar_pagination_params_read_as_absent`).
4. **Re-posting an existing deploy keeps its `deployed_at`** (ref, name,
   url still update), so a retried hook no longer moves an old release to
   the top; a new deploy gets now (`deploys_upsert_by_name_and_validate_the_url`).
5. **Reopening a merged issue whose chain is broken** answers 422
   `{"errors":{"status":["<InvalidMerge message>"]}}` instead of the 500
   page (`reopening_a_merged_issue_with_a_broken_chain_is_a_422`).
6. **Exceptions and logs run only the pagination asked for**: only
   `cursor`/`limit` present answers `results`/`results_meta` alone; only
   `page`/`per_page`, `data`/`meta` alone; both families or neither, both
   as before. Presence is the key (`page[]=1` counts as asking for
   `page`) (`exceptions_and_logs_run_only_the_pagination_asked_for`).

# Security hardening (pre-open-source audits, 2026-10-16)

Each item is Rust-only (Rails does not have it) and was added after three
security audits. Regression tests: `crates/server/tests/security_*.rs`.

1. **NDJSON line cap.** A line over 4 MiB (`payload::MAX_LINE_BYTES`) refuses
   the batch with `400 {"error":"line too long"}`, like the other payload
   limits. Rails has no cap. Newlines are searched once per byte; the old
   rescan made a newline-free 32 MiB body cost 3.7 s of CPU (release), now
   it is refused in 2 ms and 28 MiB of near-cap lines parse in 42 ms.
2. **Deploy `timestamp` parsing is byte-safe and checked.** A multibyte
   timestamp used to panic the handler task (no response); a 19-digit year
   wrapped. Both now answer the usual 422 "Deployed at can't be blank".
3. **New issues are capped per environment and per account.** `_group` is client-chosen, so
   one ingest token could mint an issue (and an alert) per exception in the
   shared control database: 5,000 distinct hashes made 5,000 issues (+2.9 MB).
   Now at most `new_issues_per_batch` (200) new issues per grouping pass,
   `new_issues_per_hour` (1,000, trailing hour, per environment) and
   `new_issues_per_account_per_hour` (3,000, trailing hour, across all of an
   account's environments, so making environments does not multiply the
   cap) are created; a busy real deployment creates a handful a day. Past a cap, a group that
   has no issue yet is recorded onto one overflow issue per environment
   (group hash `railwatch:too-many-new-issues`, which `_group` validation
   can never produce; title "Too many new issues: ..."), its `occurrences`
   counting every diverted exception. The exceptions themselves are stored as
   usual with their own group hash; existing groups are never diverted; each
   capped pass logs a warning with the count. New-issue alerts are therefore
   bounded by the same caps, and the existing per-rule burst collapse (5 per
   minute, then one summary) limits what is delivered. Same 5,000-hash PoC
   now: 201 issues (+104 KiB).
4. **HTTP timeouts and a connection cap** (`serve.rs`, replacing
   `axum::serve`, which exposes neither). A request head must arrive within
   `http_header_timeout_seconds` (30; also the idle keep-alive limit), a
   body within `http_body_timeout_seconds` (600, from its first read: the
   32 MiB ceiling at ~450 kbit/s), and a connection that moves no byte
   either way for `http_idle_timeout_seconds` (300) while the server waits on
   it is closed. At most `max_connections` (4,096) are open; past that new
   ones wait in the kernel backlog. Before: a partial header held its
   connection forever (still open after 8 s in the test); after, it is closed
   at the limit (1.0 s with the test's 1 s setting). Socket-activated
   restarts are unchanged (bench/restart/run.sh restart and overlap: every
   generation inherited the socket, 2,399/2,399 posts accepted).
   One address (an IPv6 /64) may hold at most `max_connections_per_ip`
   (256) of them; a further connection from it is closed as soon as it is
   accepted. Without it one source could take all 4,096 slots and keep other
   clients, the gems included, in the backlog (`serve::tests::
   one_source_cannot_take_every_connection`: with 8 slots and the flood
   holding 20 connections, a second address was not served within 2 s; with
   4 per address it is, and the flood's 16 extra connections are closed).
   The cap uses the TCP peer, since X-Forwarded-For is not read until a
   request is, so with `trust_proxy` it is off: every connection is the
   proxy's, and per-client connection limits belong in the proxy.
5. **Back-redirects refuse `//` and `/\` paths.** `redirect_back_or_to`
   (`Ctx::back_or`) reflected the Referer's path; a path starting `//` or
   `/\` is protocol-relative to a browser, so it now falls back like a
   missing Referer (Rails' `url_from` would also refuse it). Control
   characters are refused as well. Every other redirect target is a fixed
   path.
6. **Failed sign-ins are also counted per email** (10 per 5 minutes per
   lower-cased email, on top of Rails' 30 attempts per 5 minutes per IP).
   A full email bucket never refuses: the password is still checked (the
   same bcrypt work), the right one signs in, and a wrong one gets the usual
   "That email or password is incorrect", so the response says nothing
   about the bucket and knowing someone's email does not let you lock them
   out. It logs a warning instead. Only the per-IP limit refuses outright,
   so guessing one account's password from many addresses is bounded by the
   per-IP limit times the addresses, as in Rails. `serve` and `doctor` warn
   when the url is https, the bind is loopback/private, there is no built-in
   TLS and `trust_proxy` is off (every client looks like the proxy).
7. **HSTS** (`Strict-Transport-Security: max-age=31536000`) on every
   response when the server terminates TLS itself, or when the url is https
   and `trust_proxy` is on (a proxy that forwards only https, as the
   installer sets up). Never on a plain-http url. Rails sends it from
   `force_ssl`; the API diff already ignores the header.
8. **AGPL-3.0 section 13 source pointers.** Every response carries
   `Link: <https://github.com/Rebulk/railwatch-rs>; rel="license source"`,
   every HTML page's head has `<meta name="source">` and
   `<link rel="license">`, and `GET /source` redirects to the repository.
   Not a visible link in the dashboard UI: that is the railwatch gem's
   frontend (a follow-up there). The diff harnesses call the router
   directly, below the layer that adds the header, so they are unaffected.
