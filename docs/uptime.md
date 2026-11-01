# Uptime monitors and status pages

Railwatch can check that an app answers from the outside: it requests a URL
on an interval, records whether the answer was right and how long it took,
alerts when the app stops answering, and can publish a status page.

It complements the [watchdog](DIFFERENCES.md#deliberate-addition-the-environment-watchdog),
which alerts when an app stops *sending* telemetry. The watchdog notices a
dead process; a monitor notices an app that is up but no longer reachable
or no longer answering correctly (DNS, TLS, the proxy, a 502).

## Setup

1. Open an environment, then **Monitoring > Uptime**.
2. An account owner or admin adds a monitor:

   | field | default | |
   |---|---|---|
   | name | | shown in alerts and on the page |
   | URL | | `http://` or `https://`, no credentials |
   | method | GET | or HEAD |
   | expected status | any 2xx or 3xx | or one exact status |
   | body contains | none | the response body must contain this text (GET only; the first 1 MiB is read) |
   | timeout | 10 s | 1 to 30 s, and shorter than the interval |
   | every | 60 s | 30 s to 1 hour |

   An environment can have up to 20 monitors. Members see the page but
   cannot change it.
3. In **Settings > Integrations**, add an alert rule for **Monitor down**
   and **Monitor up again** on the application, like any other event.

The first check runs within a few seconds of saving.

## How checks work

- Each check is one request from this server, with the user agent
  `Railwatch-Uptime/<version> (+<source URL>)`, so the app's logs and
  firewalls can recognise it.
- **Redirects are not followed.** A 3xx is an answer, and it passes unless
  you set an exact expected status. Point the monitor at the final URL.
- **Address rules.** As with alert webhooks, every address the host resolves
  to must be public. Private, loopback, link-local, carrier-grade NAT and
  cloud metadata addresses (such as `169.254.169.254`) are refused when the
  monitor is saved (for an IP in the URL) and again at every check (for a
  name). The connection is pinned to the address that was checked, so DNS
  rebinding cannot reach an internal address in between. A refused check is
  recorded as a failure with cause `blocked`.
- **Self-hosted on the same network as the app?** Set
  `RAILWATCH_ALLOW_PRIVATE_MONITOR_TARGETS=1` to allow private and loopback
  addresses. Anyone who can add a monitor can then make this server request
  anything it can reach, including admin ports on localhost, so only set it
  where every account admin is trusted with that. It is separate from
  `RAILWATCH_ALLOW_PRIVATE_ALERT_TARGETS`.
- **Scheduling.** A monitor is checked one interval after its last check,
  plus up to a tenth of the interval of random jitter, so monitors added
  together drift apart. At most 8 checks run at once; any others wait for a
  free slot. After a restart, a monitor that is overdue is checked within a
  few seconds.
- Each check records its status code, latency and, when it fails, a cause:

  | cause | meaning |
  |---|---|
  | `dns` | the host name did not resolve |
  | `connect` | the connection was refused, reset or unreachable |
  | `tls` | the TLS handshake or certificate failed |
  | `timeout` | no complete answer within the timeout |
  | `status` | the status was not the expected one |
  | `keyword` | the body did not contain the text |
  | `blocked` | the address is not allowed (above) |

## Incidents and alerts

- A monitor is **down after 2 failed checks in a row**, so one dropped packet
  does not page anyone. It is **up again after 1 successful check**.
- Going down opens an incident that starts at the **first** failed check,
  and fires one `monitor_down` alert. The payload has `monitor`, `url`,
  `environment`, `environment_id`, `cause`, `detail` (for example
  `status 503, expected 2xx or 3xx`), `status_code`, `since` and a
  one-line `message`.
- Coming back closes the incident and fires one `monitor_recovered` alert
  with `down_for_seconds` and `since`.
- Further failures while down send nothing. State lives in the control
  database, so a restart neither forgets an outage nor alerts it again.
- Pausing a monitor closes any open incident without an alert. Resuming it
  starts again from "waiting for first check".
- Alerts go through the normal rules, so environment filters, every
  integration (Slack, email, webhook) and the alert history work as for any
  other event. Webhooks get the payload as `payload`
  ([agent-webhooks.md](agent-webhooks.md)).

## Storage and retention

Monitors, checks, daily rollups, incidents and status pages are tables in the
control database (`control.sqlite3`), not in the per-environment telemetry
files:

- Checks are written for every environment at once, by one loop. The
  telemetry files have one writer per environment and commit through the
  ingest path; putting checks there would make the probe loop wait on
  ingest, and ingest wait on probes.
- The data is small and bounded by construction: at most 20 monitors per
  environment, one check every 30 s at most, raw checks for 7 days (about
  20,000 rows, under 2 MB, per monitor at the shortest interval), and one row
  per monitor per day for 90 days.
- It is in the control database's backup ([SELF-HOST.md](SELF-HOST.md#backups)),
  and `MAX_DATA_BYTES` cannot delete it: the budget only removes old
  telemetry day files, never the control database, so uptime history
  outlives a disk-pressure trim, as alerts and issues do.

Retention, run hourly and at start: raw checks are kept for the 7 whole days
before today; daily rollups (checks, failures, uptime, p50/p95 latency of
successful checks) and closed incidents for 90 days. 24-hour uptime comes from
the raw checks, 7/30/90-day uptime from the rollups. Uptime is the share of
checks that passed, rounded down, so 99.9996% shows as 99.999% rather than
100%. A day without checks has no uptime rather than 100%.

## Public status page

Off by default, one per environment. On the Uptime page, an admin sets a
title, an address and **Public**, and marks each monitor to show with
**Show on the status page** (optionally with a public name, and **Show its
URL there**).

- It is served at **`/status/<slug>`**, with JSON at **`/status/<slug>.json`**,
  without signing in. Leave the address blank for an unguessable slug (20
  random characters), or choose one (3 to 64 lowercase letters, digits and
  hyphens). The route is outside the dashboard's session: no sign-in, no
  cookies. With `base_path` set it is under that path like every page, so
  a server mounted behind an app's own login (the railwatch-server gem) is
  only public if that app forwards `/status/` without its login.
- It shows the overall status (all operational, some down, all down), each
  public monitor with a 90-day bar and its 90-day uptime, and the last 10
  incidents of public monitors with their durations.
- It shows **only** what is marked public: the public name (never the
  internal name), the URL only when allowed, statuses, uptime and incident
  times. No ids, internal URLs, causes, error text or anything about users.
  The HTML and JSON are rendered from the same value, so the HTML cannot show
  more than the JSON.
- An unknown address and a page that is off are the same 404.
- It is one server-rendered page with inline CSS and no JavaScript, readable
  on a phone (the bars show the last 30 days on narrow screens). Responses
  are `Cache-Control: public, max-age=30`, so a CDN or proxy in front of it
  absorbs a crowd during an outage. The JSON allows any origin, for a status
  widget on another site.

### Custom domains (not built)

A status page on its own host, such as `status.example.com`, is not built
yet. The built-in HTTPS takes one certificate for one `tls_domain`, and
routing does not look at the host, so serving a second domain would need
certificates for several names and a host-to-page map. Until then, put a
proxy or CDN in front of `https://<railwatch>/status/<slug>` for that name,
or link to the Railwatch URL.

## Limits

- **One location.** Probes come from the Railwatch server itself. They show
  that the app is down from where Railwatch runs, but not a regional outage,
  a CDN problem in one country, or a network split between Railwatch and the
  app that users do not see. There is no multi-region confirmation.
- **If Railwatch is down, so is the page.** The status page and the alerts
  run on the same server. If that server is down, nothing is checked, no
  alert is sent and the status page is unreachable. For Railwatch's own
  status, run a second Railwatch instance somewhere else that monitors the
  first, and use `RAILWATCH_DEADMAN_URL` with an outside service.
- Checks are HTTP requests only: no TCP, ping, DNS record or certificate
  expiry checks, no request bodies or custom headers, no manual incidents or
  maintenance windows on the status page.
- Latency is measured on this server, including its DNS lookup and
  connection, so it is the time from here, not from your users.
- Clocks: days are UTC days.

## Agents and API

- MCP `uptime` (read scope): every monitor with status and since when, the
  last check, uptime over 24h/7d/30d/90d, yesterday's and today's
  p50/p95, recent incidents, the status page URL when it is on, and a
  dashboard link. `monitor` narrows to one; `incidents` sets how many (up to
  50). `list_alerts` takes `event: monitor_down` / `monitor_recovered`.
- `GET /api/v1/applications/<app>/environments/<env>/uptime`: the same data
  with each monitor's settings and its 90 days.
