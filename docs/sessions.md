# Browser sessions and journeys

A session journey is what one person did in their browser, in order, across
every tab they had open: the pages they visited, the requests those pages
made, the errors the browser threw and the exceptions the server raised. It
answers "what was this user doing when it broke?", even when they had two
tabs side by side.

## The session model

Railwatch follows the standard web analytics model (GA4, Amplitude,
Mixpanel), with PostHog's per-tab id inside it:

- **One session per browser, shared by every tab.** The browser client
  keeps the session id in `localStorage`, so a new tab joins the session
  already running. It is mirrored in the `railwatch_session` cookie, so a
  full page load carries it too.
- **It ends after 30 minutes with no activity in any tab**, or 24 hours
  after it started, whichever comes first. The next activity starts a new
  session. Activity is a page load, an Inertia visit, a beacon, or a tab
  becoming visible again. A tab that comes back after the session expired
  joins the new one.
- **`tab_id` tells the tabs apart.** Each tab has its own id in
  `sessionStorage`. It survives a reload of that tab, but a new tab gets a
  new one. (A browser's "duplicate tab" copies `sessionStorage`, so a
  duplicated tab starts with its original's id. PostHog's `window_id` has
  the same caveat.)

The browser client (railwatch gem 1.5 and later) sends both ids:

| Record | Where | Fields |
|---|---|---|
| request | `X-Railwatch-Session` and `X-Railwatch-Tab` headers on the requests it makes; the cookie on a full page load (no tab) | `session_id`, `tab_id` in the request's `detail` |
| visit | the beacon | `session_id`, `tab_id` columns on `visits` |
| browser error | the beacon | `context.browser.session` and `context.browser.tab` (from a gem without `tab`, the journey puts it in the lane of the tab that last visited the page it names) |

Ids are 1-64 characters of `[A-Za-z0-9_-]`. A request whose app has no
browser client (an API client, a job, a script) has no session, and
neither do requests and visits recorded before the gem sent these fields.
They are never in a journey. The gem's `user:<id>` session key for
release health is not a browser session and is not stored as one.

The journey is not the release-health `sessions` table. That table holds one
row per session per flush, with counts. The journey is read from the
requests and visits themselves.

## Reading a journey

**Dashboard.** Monitoring > Sessions lists the window's sessions (filter by
user, or to the ones with errors). A session's page shows its tabs as
lanes, `t1`, `t2`, ... in the order they were first seen, with one row per
event in time order. Consecutive alike events in one tab (the same request
over and over) fold into one row, `×40 over 20m`, which opens into its
events. A request's page links to its session ("View session") when the
request carried one.

**MCP.**

- `list_sessions`: recent sessions with first and last activity, requests,
  visits, tabs, users and errors. Filter by `user` (a session any of whose
  records names that user, counted whole: a visit or browser error the
  beacon could not attribute is still in it) or `has_errors`.
- `session_journey`: one session by `session_id`, by `execution_id` (the
  session that request belonged to), or by `user` (their newest session in
  the window; `user_sessions` lists the others). The result is a summary
  (started, duration, tabs, counts, users, and with the `security` scope
  the client addresses and user agents), then the timeline.

The timeline is grouped by default, losslessly, the way `search_requests`
groups ([security-agent.md](security-agent.md), "Grouped results").
Consecutive steps of one tab that are alike are one run. A step is a page
visit together with its own request (the next request in that tab, within
5 seconds, to the visit's path: what an Inertia visit or reload records),
or any other event alone. Steps are alike when every event in them is: for
a request, the same status, route, outcome, halted callback and exception
class; for a visit, the same component; for an error, the same class and
message. A tab reloading its page every 2 seconds is then one run, `type:
"page"`, its `count` every visit and request in it (`requests` of them
requests, the visits folded in `visits`), and the page shows "GET /three
×15 over 30s + 15 page visits". A run carries its exact count, first and last time, cadence,
duration p50/max and the values that varied. A run of requests has
`expand`: the `search_requests` arguments (`session:<id> tab:<id>` and the
run's times) that list exactly its requests. A run only ends when its own
tab does something else, so a tab polling in the background stays one entry
while the other tab's clicks interleave with it. `shape: "rows"` lists
every event. `max_tokens` keeps the newest entries, where a session usually
went wrong, and counts the older ones in `timeline_left_out`.

```json
{"tool": "session_journey", "arguments": {"execution_id": "<a failing request's id>"}}
```

The `session:` and `tab:` filter keys work wherever the request filter
grammar does: `search_requests`, `query_requests`, `search_telemetry` (the
records of that session's requests), `/api/v1`, and the dashboard's
Requests filter bar.

## Errors

One definition everywhere (the list, the page, the MCP summary): a
session's errors are its 5xx requests, its failed page visits and its
browser errors. An exception a request raised is part of that request's
error: it is listed in the timeline and counted apart (`exceptions` in the
MCP summary), never added to the total, so a 500 that raised is one error.
A browser error is beaconed after the fact, so the journey takes the
session's browser errors from up to 30 minutes (the idle timeout) either
side of its requests and visits, as the list does over its window.

## Storage and speed

A request's `session_id` and `tab_id` stay in its `detail` JSON. A partial
expression index, `idx_executions_sessions`, holds just the requests that
carry a session, keyed by session id and time. Visits get two columns and
`idx_visits_sessions`. Reading one session is an index seek in each day
file, not a scan of the day (docs/PERF.md, "Session journeys"). Existing
day files get the indexes from the same background build as the request
index (docs/DIFFERENCES.md #15). A day file without them answers the same,
only slower.

A journey reads at most the newest 20,000 requests and 20,000 visits of a
session, and says so when there were more.
