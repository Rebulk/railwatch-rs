# AI assistants and MCP

Railwatch Cloud is an [MCP](https://modelcontextprotocol.io) server. Point
Claude Code, Claude Desktop, Cursor, VS Code, or Zed at it and the assistant
sitting in your editor can read the same production data your dashboards
show: issues and their stack traces, slow routes, the queries and N+1s
behind them, stored query plans, stack profiles, logs, deploys, and
crash-free rates per release. It can also write — resolve an issue, set a
priority, leave a comment — and every write is signed with your user and the
agent's name in the issue's activity feed.

MCP is a Railwatch Cloud feature. The embedded dashboard at `/railwatch`
does not serve it; an embedded install gets it by
[exporting to the cloud](embedded.md#three-ways-to-run-it).

The endpoint is `<your ingest host>/mcp`. For the hosted platform that's
`https://railwatch.rebulk.com/mcp`; if you self-host, it is your own host (see
[`self-hosting.md`](self-hosting.md)). The gem knows which one you're on:

```sh
bin/rails railwatch:mcp
```

prints every block below with your platform's host already filled in.

## 1. Get a token

MCP tokens are **per person**, not per application: sign in to the platform,
go to **Settings → Profile → "API & MCP token"**, and press *Generate token*.
The token starts with `rwp_` and is shown once. It can reach every account
your user belongs to, and nothing else.

This is a different token from the `rw_...` ingest token the gem uses. The
ingest token writes telemetry for one environment; the MCP token reads it
back as you.

That page also renders the blocks below with the real token substituted in,
each with a copy button — so the fastest path is: generate, copy, paste.

## 2. Connect a client

Everything below uses `https://railwatch.rebulk.com/mcp`; substitute your own
host if you self-host, and `rwp_your_token_here` for the token.

### Claude Code

```sh
claude mcp add railwatch --transport http https://railwatch.rebulk.com/mcp \
  --header "Authorization: Bearer rwp_your_token_here"
```

### Claude Desktop

Claude Desktop speaks stdio, so it needs the `mcp-remote` bridge. In
`claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "railwatch": {
      "command": "npx",
      "args": [
        "-y",
        "mcp-remote",
        "https://railwatch.rebulk.com/mcp",
        "--header",
        "Authorization: Bearer rwp_your_token_here"
      ]
    }
  }
}
```

### Cursor

`.cursor/mcp.json` in the project, or `~/.cursor/mcp.json` for every project:

```json
{
  "mcpServers": {
    "railwatch": {
      "url": "https://railwatch.rebulk.com/mcp",
      "headers": { "Authorization": "Bearer rwp_your_token_here" }
    }
  }
}
```

### VS Code

`.vscode/mcp.json` in the workspace:

```json
{
  "servers": {
    "railwatch": {
      "type": "http",
      "url": "https://railwatch.rebulk.com/mcp",
      "headers": { "Authorization": "Bearer rwp_your_token_here" }
    }
  }
}
```

### Zed

`settings.json`, also through the `mcp-remote` bridge:

```json
{
  "context_servers": {
    "railwatch": {
      "source": "custom",
      "command": "npx",
      "args": [
        "-y",
        "mcp-remote",
        "https://railwatch.rebulk.com/mcp",
        "--header",
        "Authorization: Bearer rwp_your_token_here"
      ]
    }
  }
}
```

### Check it without a client

```sh
curl -sS https://railwatch.rebulk.com/mcp \
  -H "Authorization: Bearer rwp_your_token_here" \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'
```

A JSON body listing tools means the token works. A `401` means it doesn't.

`GET https://railwatch.rebulk.com/mcp` and
`GET https://railwatch.rebulk.com/.well-known/mcp.json` describe the server —
name, version, protocol version, transport, tool and prompt names, and how
to authenticate — with no token at all, for clients that probe a host before
they're configured.

## 3. Tools

Every tool returns JSON as the text body of a single content block. **All
durations are milliseconds.** Every `window` argument takes `1h`, `6h`,
`24h`, `7d`, or `30d`, and defaults to `24h`.

`application_slug` and `environment` are optional when the token can see
only one application, or the application has only one environment (an
embedded server always has one of each). Records carry a `url` to their
dashboard page. A result is never larger than about 60 KB: one that would be
comes back as `{"truncated": {...}, "result": ...}`, with what was left out
and a `hint` on how to ask for less.

| Tool | Arguments | Returns |
|---|---|---|
| `list_applications` | — | applications, their account, and their environments. Start here — everything else takes the `application_slug` and `environment` this returns (plus `account_slug`/`application_id` when two accounts share a slug). |
| `list_issues` | `application_slug?`, `environment?`, `status?` (`open`/`resolved`/`ignored`, default `open`), `sort?` (`recent`/`impact`), `limit?` | issues with key, title, kind, status, priority, occurrence and affected-user counts, culprit, first/last seen. `sort: impact` ranks by occurrences divided by (1 + days since last seen). |
| `get_issue` | `key` or `issue_id`, `frames?` (`app`/`all`) | one issue. For an exception, the sample occurrence with its stack frames (framework runs collapsed to a count unless `frames: all`), cause, locals, parsed context, browser breadcrumbs, and the execution it happened inside; `last_occurrence` is the newest, or a reference when it is the same one. For a performance or anomaly issue, the metric, limit or baseline, representative records, trend, and deploy comparison. |
| `investigate_issue` | `key` or `issue_id` | the root-cause bundle in one call: the exception with its in-app frames and code; `lead_up`, the summary of the request or job it was raised in and the queries, calls and log lines just before the raise; `started_in`, the deploy its earliest retained occurrence ran (else the last deploy before `first_seen_at`) with the deploy before it, the commit range and the deploy's commits; `trend` (per day, per deploy, last 7 days against the 7 before); `who` (users, tenants); `similar_issues` (same class or culprit file, resolved ones with their last comment); and `markdown`, the same as a brief. |
| `update_issue` | `key`, `status?`, `priority?`, `assignee_email?`, `agent?` | the updated issue. Writes an activity entry attributed to your user and `agent`. |
| `add_comment` | `key`, `body`, `agent?` | the created comment, attributed the same way. |
| `list_slow_routes` | `application_slug`, `environment`, `window?`, `sort?` (`total`/`avg`/`p95`) | the 20 routes that cost the most request time (`sort: total`, the default), each with `group_hash`, count, errors, `total_ms`, `avg_ms`, `p95_ms`. |
| `get_route` | `application_slug`, `environment`, `route` or `group_hash`, `window?` | one route's count/errors, p50/p95/p99/max, its slowest individual requests, the slowest queries those requests ran, and each N+1 with a concrete Active Record fix. |
| `explain_route` | `application_slug`, `environment`, `route` or `group_hash`, `window?` | why one route is slow, from the timelines of up to 20 of its requests (its slowest and newest): `answer`, a paragraph naming the causes; `time_ms_per_request` (SQL, transactions, views, external, untracked); `queries_per_request`; the N+1s with a fix and how often each ran per request; `top_queries` by total time with stored plans; the hottest profile frames, or how to turn the profiler on. |
| `compare` | `application_slug`, `environment`, `route?`/`job?`/`group_hash?`, `kind?`, `before?`, `after?`, `since?`, `window?` | before and after for a route, a job class or the whole environment: by default the latest deploy against the one before it; `since` (a time or the `execution_id` of the last request before a change) for development, where a code reload doesn't change the deploy. A `verdict` first, then each side's count, errors, p50/p95/avg, queries and db ms per request, the deltas, `worst_regression_pct` (gate CI on it), `low_samples`, N+1s gained and lost, and the query shapes that appeared, disappeared or changed. |
| `search_requests` | `application_slug`, `environment`, `q?`, `window?`, `limit?` | individual requests. `q` is the dashboard's filter grammar: `method:GET`, `route:/checkout`, `status:500` or `status:5xx`, `deploy:…`, `tenant:…`, `user:…`, `min_ms:250`; bare words match the route name. |
| `get_execution` | `application_slug`, `environment`, `execution_id`, `timeline_offset?`, `timeline_limit?`, `timeline_type?` | one execution, led by a `summary`: time split into SQL, transactions, views, external calls and untracked time; the repeated query shapes with count, total ms and source line; the N+1s with a fix; the slowest records; exceptions and warnings; and notes on what to do next. Then one page of the child timeline (25 rows; `timeline_page` says how to get the rest). |
| `last_execution` | `application_slug?`, `environment?`, `path?`, `kind?` (`any`/`request`/`job`), `timeline_*?` | the newest execution, optionally matching a path, route or job name, in `get_execution`'s shape. Load a page, call this, see the N+1. |
| `explain_query` | `application_slug`, `environment`, `group_hash`, `window?` | the stored query plan for a query group, with the SQL and the sample's duration. `explain` is null unless the app sets `RAILWATCH_CAPTURE_QUERY_EXPLAIN`. `sql` is the normalized shape unless the app also sets `RAILWATCH_CAPTURE_SQL_VALUES`. |
| `get_profile` | `application_slug`, `environment`, `profile_id?`, `execution_id?`, `limit?` | the hottest frames of a stack profile — self and total samples, each with a percentage. |
| `search_logs` | `application_slug`, `environment`, `q`, `level?`, `limit?` | matching log lines, each with the `execution_id` to expand with `get_execution`. |
| `search_telemetry` | `application_slug`, `environment`, `resource` (`jobs`/`exceptions`/`logs`/`queries`), `q?`, `window?`, `cursor?`, `limit?` | raw rows of that resource with cursor pagination (`meta.next_cursor`, `meta.has_more`). `q` takes `after:`, `before:`, `user:`, `tenant:`, `deploy:`, plus per-resource keys such as `class:`, `queue:`, `level:`. |
| `list_jobs` | `application_slug`, `environment`, `window?`, `sort?` (`count`/`errors`/`p95`/`avg`/`max`), `limit?` | every job class with runs, failures and failure rate, avg/p95/max ms, queue and average queue latency, plus per-queue totals. |
| `get_job` | `application_slug`, `environment`, `job` or `group_hash`, `window?` | one job class against the window before it, the exceptions its failures raised, its recent failed runs and slowest runs with their `execution_id`s. |
| `list_llm_calls` | `application_slug`, `environment`, `window?`, `group?` (`model`/`none`), `q?`, `limit?` | LLM calls by model (calls, errors, tokens, cost, truncations, avg/p95 latency, and the tools called), or the recent calls themselves with the execution that made each one. |
| `list_tenants` | `application_slug`, `environment`, `window?`, `q?` | your app's own tenants (whatever it passes to `Railwatch.context(tenant:)`) with request, error, job, exception, and user counts. |
| `recent_deploys` | `application_slug`, `environment` | the 20 most recent deploys with ref, name, time, and link. |
| `release_health` | `application_slug`, `environment`, `window?` | crash-free session rate, crash-free user rate, and adoption per release. |
| `web_vitals` | `application_slug`, `environment`, `window?`, `component?`, `sort?` (`count`/`p95`/…/`lcp`/`cls`/`inp`/`ttfb`), `dir?`, `limit?` | what the Visits page shows: p75 LCP, INP, TTFB (ms) and CLS (a score), each rated `good`/`needs-improvement`/`poor` with its sample count and the web.dev `thresholds`; every Inertia component with visit count, errors, and visit duration p50/p95/p99/max plus its own vitals; and the vitals of each release deployed in the window, to see whether a deploy made pages slower. Vitals come from initial page loads only. `ttfb_phases` (overall, per component, per release) splits TTFB into p75 `redirect_ms`, `connect_ms` and `server_ms`, with `redirected_at_least` (a floor: browsers hide cross-origin redirects, whose time is still in `redirect_ms`). |
| `list_alerts` | `application_slug?`, `event?`, `status?`, `limit?` | fired alerts: which rule, which issue, which integration, and whether delivery succeeded. |

## 4. Prompts

Three canned workflows. A prompt is a plan, not an answer: it tells the
assistant which tools to call in which order and what to do with each
result, so its first turn is spent working rather than asking you which tool
exists.

| Prompt | Arguments | What it does |
|---|---|---|
| `triage_issue` | `key` | Calls `investigate_issue` (frames, the lead-up, the deploy it started in and its commits, similar issues), reads the in-app frames and the suspect commits, then reports what breaks, for whom, how often, and the smallest fix. It will comment and set a priority; it is told not to resolve. |
| `slow_route` | `application_slug`, `environment`, `route`, `window?` | Calls `explain_route` (time split, N+1s with fixes, costliest queries with plans, profile frames), then reports the indexes and `includes` to add, ordered by expected saving, quoting measured milliseconds. |
| `daily_summary` | `application_slug`, `environment`, `window?` | New issues, spiking issues, deploys, release-health movement, the worst routes, and what already alerted — leading with the one thing worth acting on. |

In Claude Code these appear as slash commands once the server is connected.

## 5. Resources

The server also publishes documents an assistant can read without a tool
call:

| URI | Contents |
|---|---|
| `railwatch://applications` | Every application and environment the token can see. |
| `railwatch://applications/<slug>/environments/<name>/summary` | Request and job volume, p95, errors, open issue count, last-seen time, and deploys, over the last 24 hours, with the previous 24 hours alongside for comparison. |
| `railwatch://docs/<name>` | Railwatch's own documentation — `readme`, `getting-started`, `configuration`, `records`, `testing`, `replacing-sentry`, `troubleshooting`, `faq`, and the rest. An assistant that doesn't know an option can look it up instead of guessing. |

## 6. What an assistant can and can't do

- **Scope.** A token reaches exactly the accounts its user belongs to.
  Revoke it by regenerating: Settings → Profile → *Regenerate token*
  invalidates the old one immediately.
- **Writes.** Only three things write: `update_issue`, `add_comment`, and
  the activity entries they create. There is no tool that deletes anything,
  changes billing, or touches your Rails app.
- **Attribution.** Every write records your user *and* the `agent` name the
  client sent, so "who resolved this" has an honest answer in the UI.
- **Payloads.** Tools return what the dashboards show, which is what the gem
  shipped. If you don't want request parameters or job arguments leaving
  your app, they never arrive here in the first place — see the redaction
  and opt-in capture settings in
  [`configuration.md`](configuration.md).

## 7. Agents working on your app

Separately from the MCP server, the gem ships two files for coding agents
working *in a Rails app that uses Railwatch*:

- [`../llms.txt`](../llms.txt) — the [llmstxt.org](https://llmstxt.org)
  index: one paragraph on what Railwatch is, then every document with a
  one-line description.
- [`../AGENTS.md`](../AGENTS.md) — how to install it, the facade methods,
  the spec matchers, `railwatch:doctor`, and this MCP hookup, in under 120
  lines.

Copy either into your own app's repo to give its agent the same context.

## See also

- [`getting-started.md`](getting-started.md) — install, the cloud token,
  first request.
- [`self-hosting.md`](self-hosting.md) — pointing the gem, and this endpoint,
  at your own platform.
- [`troubleshooting.md`](troubleshooting.md) — when something isn't
  reporting.
