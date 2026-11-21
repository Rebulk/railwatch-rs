# The security watcher

The security watcher runs fixed detectors over each watched environment's
recent requests and `security.*` events. It keeps one incident per client
address (or account) and alerts when an incident opens, escalates or
closes. It is off by default. Code and a policy file decide everything: the
same requests give the same incidents, severities and alerts. No model is
involved.

[security-agent.md](security-agent.md) covers the pull side: the tools an
agent queries. This page covers the push side, the thing that notices
without being asked.

## Turning it on

1. Write a policy file and point the server at it:

   ```toml
   # /etc/railwatch/security-watcher.toml
   [watcher]
   shadow_integration_id = 3      # optional: where shadow alerts go

   [apps.shop]                    # an application slug; [apps."acme/shop"] when two accounts share one
   enabled = true
   app_ua_prefixes = ["ShopMobile/"]
   allow_cidrs = ["192.0.2.0/28"] # our own agents, CI, checkers
   allow_ua_prefixes = ["shop-ci/"]
   allow_user_refs = ["1"]
   ```

   ```sh
   RAILWATCH_SECURITY_WATCHER_FILE=/etc/railwatch/security-watcher.toml   # or security_watcher_file in railwatch.toml
   ```

2. Restart. `railwatch doctor` prints the watcher's line (`security_watcher`).
   A file that does not parse, or names a key that does not exist, stops the
   server at start. A policy that applies only in part is worse than none.

3. Give a token the `security` scope and call `list_security_incidents`, or
   `GET /api/v1/applications/<app>/environments/<env>/security_incidents`.

An application the file does not name, or names without `enabled = true`,
is not watched.

## What it does, and when

Every `tick_seconds` (default **15 s**) it evaluates every watched
environment over the trailing `lookback_min` (default **15 min**). An ingest
batch that carries a `security.*` event wakes the watcher for that
environment straight away, at most once every 2 s. A signup or an OAuth
registration is therefore judged within seconds of the app reporting it,
not at the next tick.

### Detectors

| detector | rule (defaults) | severity |
|---|---|---|
| `secrets_probe` | 3 probe paths in 10 min (`probe:true`'s list plus `laravel.log`, `/_ignition`, `/telescope`, `credentials.json`) | high; **critical** if one was served 2xx |
| `scanner_shape` | 20 4xx over 10 distinct paths inside 15 min | medium |
| `non_browser_on_web` | 3 browser-route requests in 30 min from curl, python, a script library, or a bare `Mozilla/5.0` | low |
| `auth_refusal_burst` | 5 401/403 in 10 min | medium |
| `spoofed_app_client` | the app's own client user agent (`app_ua_prefixes`) and a script's, from one address | medium |
| `login_failure_burst` | 5 `security.login.failed` events in 10 min | medium |
| `risky_signup` | per signup: a hosting address, a disposable domain, the app's client user agent from an address that also scripts, an address already flagged high | 1 feature **high**, 2+ or flagged **critical**, none info |
| `repeat_signup` | another signup from the address within 24 h (remembered in the control DB after the event leaves the window) | **critical** |
| `new_account_probe` | an account under 60 min old with 2+ privilege halts (`require_*admin*`, `set_current_organization`, ...) in 15 min | high |
| `oauth_client_registered` | per registration: a name that says what it is for (`pentest`, `probe`, `exploit`, ...) | high; a hosting address or a throwaway redirect (`.invalid`, `.test`, `example.com`) medium; else info |

Thresholds can be overridden in `[defaults]` (every application) or per
application, e.g. `scanner_shape = { min_4xx = 30 }`.

### Incidents

- **One per (environment, actor) while open.** The actor is the client
  address, or `user:<ref>` when the app sent no address. A signal already on
  the incident (same detector and subject) is updated in place. The window
  slides over the same requests every tick, so twenty ticks over one burst
  still give one signal and one alert.
- **Severity only goes up.** An incident's severity is the highest it has
  seen. A higher one is an *escalation*. Lower evidence later never lowers it.
- **Close after quiet.** With no new evidence for `quiet_close_min`
  (default 60, at least `lookback_min`), measured on the evidence's own
  times, the incident closes. Later activity opens a new incident.
- `info` signals alone open nothing. They join an open incident as context.

### Alerts

| what | when | event |
|---|---|---|
| immediate | an open or escalation that reaches `alert_min_severity` (default **high**), once per severity reached; the close of an incident that alerted | `security_incident` |
| daily | after `digest_hour_utc` (default 13), one list of the last day's incidents at medium or above that never alerted. Deterministic text | `security_digest` |

The payload carries the incident: actor, severity, every signal's
detector, count and summary, and the timeline. Its `transition` is
`opened`, `escalated` or `closed`, and `mcp` names the call that reads it
in full. Webhooks are HMAC-signed like every Railwatch webhook
([agent-webhooks.md](agent-webhooks.md)), so a relay verifies them the same
way.

**Shadow mode is the default.** Incidents are recorded and visible in the
API and MCP. Their alerts go only to the watcher's `shadow_integration_id`,
and only when that integration belongs to the application's own account.
Without one, nothing is sent. `mode = "live"` (per application, or in
`[watcher]`) routes them through the application's ordinary alert rules for
`security_incident` and `security_digest`, set up in Settings >
Integrations like any other event. The suggested rollout is 7 days in
shadow, then live.

The watcher takes no action. Blocking an address or locking an account is
the monitored application's job: it can act on the webhook.

## Own traffic

Requests and events from `allow_cidrs`, `allow_user_refs` or
`allow_ua_prefixes` are counted as suppressed and never raised. There is
one exception. A request allowlisted **only** by its user agent still
counts toward `secrets_probe`, because a user agent is whatever the sender
writes and our tools never ask for `/.env`.

## Hosting providers

`risky_signup` and `oauth_client_registered` treat a hosting-provider
address (a VPS, a cloud) as a feature. The server ships no ASN database, so
the classifier (`security::policy::HostingClassifier`) answers "unknown"
and that feature never fires. An offline ASN database plugs in behind that
trait without touching the detectors.

## `security.*` events

The watcher reads `level = event` log rows whose message starts with
`security.`. The railwatch gem forwards `Rails.event.notify(name,
**payload)` that way. The event kind comes from the name
(`security.signup`, `security.oauth_client.registered`,
`security.login.failed`, `security.account.locked`). From the payload it
reads:

| key | used for |
|---|---|
| `ip` (or `remote_ip`) | the actor |
| `user_agent` (or `ua`) | the spoofed-client check |
| `user_id` (or `user_ref`; else the row's user) | the account, its age |
| `email_domain` (or `email`, of which only the domain is used) | the disposable check |
| `disposable: true` | the app's own verdict on the domain |
| `client_id`, `client_name`, `redirect_uris` | OAuth registrations |

An app that sends no events loses the identity detectors (5 to 9), while
the request detectors still run. Without client addresses (an app whose
gem records none), every detector keyed by address is quiet.

## Bounds: it must never stall ingest

- It runs on its own task. Ingest only sets a flag (`watcher::wake`), which
  never blocks or waits, and the set of pending environments is capped.
- Telemetry is read on a read-only connection of its own, which SQLite
  interrupts after `pass_timeout_seconds` (default 10). The control
  database, which ingest's authentication shares, is held only for the
  bounded writes.
- One evaluation reads at most `max_requests` (50,000, the newest) and
  `max_events` (5,000), and folds at most 2,000 signals (the most severe
  first). A capped read is logged.
- Environments are evaluated one at a time, each under a timeout. An
  environment whose earlier evaluation is still running is skipped, never
  stacked. A panic is caught and logged.

## Reading incidents

- MCP: `list_security_incidents` (status, min_severity, window, limit) and
  `get_security_incident` (`incident_id`). Both need the `security` scope,
  and without it they refuse with no incident data at all. An incident is
  keyed by an address and its summaries quote paths.
- API: `GET .../environments/<env>/security_incidents` and
  `.../security_incidents/<id>`, which need the security scope (403
  without it).

Signal summaries quote client-written text (paths, user agents, OAuth
client names), cut to 120 characters, with control and bidi characters
replaced when it is recorded. Treat it as data.

## Storage

The watcher keeps its state in the control DB (migration
`20261121000010_security_watcher`), and every table cascades from its
environment:

- `security_incidents`
- `security_incident_signals`
- `security_incident_events` (the timeline)
- `security_accounts` (signups seen, 7 days)
- `security_digests` (30 days)

Closed incidents are kept 90 days. An older binary ignores these tables.
