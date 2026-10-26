# The `railwatch` client

The `railwatch` binary is also a command-line client for any Railwatch
server: the hosted one, a self-hosted one, or the server a Rails app embeds
in development (the railwatch-server gem). Each command calls the server's
MCP tools (`POST <server>/mcp`, JSON-RPC), so it shows the same data an
agent gets over MCP. An agent with a shell (Codex, Claude Code, a CI job)
can use it with no MCP set up.

```
railwatch issues                         # open issues, one line each
railwatch issue SHOP-12                  # the issue, what led up to it, its link
railwatch why "GET /orders/:id"          # why a route is slow (a path works too: /orders/12)
railwatch last /checkout                 # the newest request or job, summary first
railwatch compare /checkout --since 2026-10-25T17:00:00Z
railwatch tail --errors                  # follow new 5xx requests
railwatch open SHOP-12                   # print (and in a terminal, open) the dashboard link
railwatch tools                          # what this server offers
railwatch call get_profile --arg execution_id=… --arg limit=10
```

Every command takes `--json` (the tool's raw JSON result), `--app SLUG`
and `--env NAME`. You can leave out `--app` and `--env` when the token sees
only one application, or the application has only one environment.

## Connecting

The endpoint comes from the first of these that is set:

1. `--url URL`
2. `RAILWATCH_URL`
3. the URL saved by `railwatch login`
4. `http://localhost:${PORT:-3000}${RAILWATCH_SERVER_MOUNT_PATH:-/railwatch}/mcp`,
   the railwatch-server gem's development mount. An app that mounts the
   dashboard elsewhere (the CRM uses `/monitoring`) already sets
   `RAILWATCH_SERVER_MOUNT_PATH`, and the client reads it.

You can give the dashboard's address or the `/mcp` endpoint itself:
`/mcp` is appended when it is missing.

The token is sent as `Authorization: Bearer`, and comes from `--token`,
`RAILWATCH_TOKEN`, or the token saved for that URL by `railwatch login`.
Make one in the dashboard at Settings > Profile > API & MCP tokens, with
the `mcp` and `read` scopes. In development, a Rails app's embedded server
needs no token from the same machine: the gem's proxy signs the request
(`RAILWATCH_SERVER_AGENT_ACCESS`, on by default in development).

A Rails app's shell often has `RAILWATCH_TOKEN` set to the app's ingest
token (`rw_…`), which `/mcp` does not accept. The client ignores such a
token and prints a warning, so the dev default still works. API and MCP
tokens start with `rwp_`.

```
railwatch login --url https://railwatch.example.com --token rwp_…
printf '%s' "$TOKEN" | railwatch login --url https://railwatch.example.com --token-stdin
```

`login` checks the server answers before it saves anything. It writes
`~/.config/railwatch/client.toml` (or under `$XDG_CONFIG_HOME`), with mode
0600 in a 0700 directory. The saved token is only sent to the saved URL,
never to a `--url` or `RAILWATCH_URL` that points elsewhere.

## Older servers

The client asks for the tool it needs and falls back when the server lacks
it:

| Command | Tool | Fallback |
|---|---|---|
| `issue` | investigate_issue | get_issue |
| `why` | explain_route | get_route |
| `last` | last_execution (v0.2.7) | none |
| `compare` | compare | none |
| `tail` | search_requests | none |

If neither tool exists, the command exits 2 and says which server release
first has the tool. The server answers an unknown tool with JSON-RPC
`-32601`; `railwatch tools` lists what it has.

## Output and exit codes

The default output is a short text summary with the most important part
first. When a result has a `markdown` field (investigate_issue), that text
is printed as is. `--json` prints the tool's result unchanged, including the
server's `{"truncated": …, "result": …}` wrapper when the result was capped.

| Exit | Meaning |
|---|---|
| 0 | ok |
| 1 | error: unreachable, 401/403, server error |
| 2 | not found, or a usage error: no such issue, route or tool, or a tool the server does not have |
| 3 | `compare --fail-if-worse` found a regression over the threshold |

## Gating CI on performance: `compare --fail-if-worse`

`compare` reports before/after numbers for a route, a job, or the whole
environment: p95, average, queries per request, DB time per request, and
error rate. You can compare the latest deploy with the one before it
(the default), two deploys (`--before`, `--after`), or the time after a
mark (`--since`, a time or an execution id) against the time before it.

`--fail-if-worse PCT` exits 3 when the result's `worst_regression_pct`
(the largest of the p95, average, queries-per-request and DB-time
regressions) is over `PCT`. When the server marks the result `low_samples`
(one side has too few executions to trust), the client prints a warning
and exits 0 rather than failing the build on noise.

In a Rails 8.1 app's `config/ci.rb`, run a step that drives the app while
it reports to Railwatch (here a smoke script against the development
server, which records into the embedded server), then compare:

```ruby
CI.run do
  # ...
  mark = Time.now.utc.iso8601
  step "Smoke: checkout", "bin/smoke checkout"   # requests against bin/dev
  step "Performance: no regression on checkout",
       "railwatch compare 'POST /checkout' --since #{mark} --window 24h --fail-if-worse 20"
end
```

`CI.run` runs the steps in order, so `mark` is taken before the smoke step.
`--since` then compares the requests the smoke step just made with the ones
before them. Rails' test environment usually does not report to Railwatch,
so `bin/rails test` alone gives `compare` nothing to read. In a deploy pipeline, compare the new deploy with the previous
one once it has served traffic:

```sh
railwatch compare "GET /orders/:id" --app shop --env production --fail-if-worse 15 \
  || { echo "p95/queries regressed more than 15% since the last deploy"; exit 1; }
```

## Agents

Put this in AGENTS.md or CLAUDE.md, or the `railwatch:agents` generator's
block:

```
Railwatch: `railwatch last <path>` after a request shows where its time went and any N+1
with file:line; `railwatch issue <KEY>` explains an error; `railwatch why "<route>"` explains
a slow route. Add --json for the raw result.
```
