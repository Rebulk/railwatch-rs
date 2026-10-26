# Agent-ready webhooks

A webhook integration (Settings > Integrations > webhook) POSTs every alert
its rules send as JSON. For issue alerts, the body includes what an agent
needs to start work without another lookup:

- the issue, with its key and canonical key, kind, culprit, first and last seen, and deploy;
- the dashboard link;
- the MCP endpoint, plus the call that investigates the issue;
- a capped summary: the occurrence's frames, collapsed the way `get_issue` shows them, and the execution summary `get_execution` leads with.

The second half of this page is a working recipe: a new issue starts a
GitHub Action that runs Claude Code headless with the Railwatch MCP. The
agent investigates, opens a PR, and comments the PR link back on the issue.

## The request

```
POST <your url>
Content-Type: application/json
User-Agent: Railwatch-Alerts/1.0
X-Railwatch-Event-Id: 745194b4-af57-474d-ad2e-6ba8ce3345b0
X-Railwatch-Timestamp: 1792039095
X-Railwatch-Signature: sha256=<hex HMAC-SHA256(secret, body)>
X-Railwatch-Signature-256: sha256=<hex HMAC-SHA256(secret, timestamp + "." + body)>
```

```json
{
  "alert_id": "745194b4-af57-474d-ad2e-6ba8ce3345b0",
  "event": "new_issue",
  "issue": {
    "id": 1, "number": 12, "key": "SHOP-12", "canonical_key": "acme/1/SHOP-12",
    "title": "ZeroDivisionError: divided by 0", "status": "open", "priority": "normal",
    "kind": "exception", "culprit": "app/models/order.rb:31",
    "occurrences": 4, "affected_users": 2,
    "first_seen_at": "2026-10-25T11:00:00Z", "last_seen_at": "2026-10-25T12:00:00Z",
    "deploy": "abc1234",
    "application": "shop", "application_id": 1, "environment": "production"
  },
  "issue_key": "SHOP-12",
  "url": "https://railwatch.example.com/issues/1",
  "payload": {"environment": "production"},
  "sent_at": "2026-10-25T12:00:05Z",
  "mcp": {
    "endpoint": "https://railwatch.example.com/mcp",
    "tool": "investigate_issue",
    "arguments": {"key": "acme/1/SHOP-12"},
    "fallback_tool": "get_issue"
  },
  "summary": {
    "occurrence": {
      "class_name": "ZeroDivisionError", "message": "divided by 0",
      "occurred_at": "2026-10-25T12:00:00.000Z", "deploy": "abc1234",
      "execution_id": "0f6c…", "execution_source": "request",
      "frames": [
        {"file": "app/models/order.rb", "line": 31, "function": "total", "in_app": true,
         "code": {"31": "line_items.sum(&:price) / count"}},
        {"omitted_frames": 40, "gems": ["actionpack", "activesupport"]}
      ]
    },
    "execution": {"time_ms": {"duration": 812.0, "sql": 24.1, "untracked": 760.2}, "n_plus_ones": ["…"], "notes": ["…"]}
  }
}
```

- **`alert_id`** (and `X-Railwatch-Event-Id`) is the alert's delivery key.
  It stays the same on every retry of that alert, so dedupe on it. Delivery
  is at least once: a receiver that took longer than 15 s to answer is sent
  the alert again.
- **Every non-2xx answer is retried** after 1, 5, 15 and 60 minutes. After
  5 attempts the alert is marked failed. Answer 2xx for events you ignore.
- **`summary`** holds only what the server could read. `occurrence` is there
  for exception issues whose occurrences are still within retention.
  `execution` is the summary of the request or job the issue happened in.
  Both MCP results are capped together at 16 KB of JSON; past that, the
  result comes back as `{"truncated": {"hint": …}, "result": …}`, which is
  the same wrapper the MCP tools use. The `mcp` call has everything.
- **`mcp.tool`** is `investigate_issue`. A server older than that tool
  answers it with JSON-RPC `-32601`; call `fallback_tool` (`get_issue`)
  instead.
- `issue` and `summary` are left out for alerts with no issue (threshold,
  silent host, quota, and so on). Those still carry `event`, `payload` and
  `url`.
- **Send test** (on the integration) delivers a `new_issue` with
  `"payload": {"test": true, …}` and no issue.

## Verifying the signature

Set a signing secret on the integration. Without one, nothing is signed.

`X-Railwatch-Signature-256` is HMAC-SHA256 over the timestamp header, a
dot, and the raw body. Check it against the bytes you received, before any
JSON parsing, and reject timestamps more than five minutes from now, so a
captured request cannot be replayed later. `X-Railwatch-Signature` (over
the body alone) is the older header, kept for existing receivers. It does
not protect against replay.

```ruby
# Rails
def railwatch_webhook_valid?(request, secret)
  ts = request.headers["X-Railwatch-Timestamp"].to_s
  return false unless ts.match?(/\A\d+\z/) && (Time.now.to_i - ts.to_i).abs <= 300
  want = "sha256=" + OpenSSL::HMAC.hexdigest("SHA256", secret, "#{ts}.#{request.raw_post}")
  ActiveSupport::SecurityUtils.secure_compare(want, request.headers["X-Railwatch-Signature-256"].to_s)
end
```

```python
# Python
def valid(headers, raw: bytes, secret: bytes) -> bool:
    ts = headers.get("X-Railwatch-Timestamp", "")
    if not ts.isdigit() or abs(time.time() - int(ts)) > 300:
        return False
    want = "sha256=" + hmac.new(secret, ts.encode() + b"." + raw, hashlib.sha256).hexdigest()
    return hmac.compare_digest(want, headers.get("X-Railwatch-Signature-256", ""))
```

```sh
# By hand, on a captured request
printf '%s.%s' "$TIMESTAMP" "$(cat body.json)" | openssl dgst -sha256 -hmac "$SECRET"
```

## Recipe: a new issue -> Claude Code opens a fix PR

```
Railwatch ──webhook──▶ relay.py ──repository_dispatch──▶ GitHub Action
                                                        └─ claude -p, with the Railwatch MCP
                                                           ├─ investigate_issue
                                                           ├─ fix + test + gh pr create
                                                           └─ add_comment (PR link) on the issue
```

GitHub's dispatch API needs a GitHub token and its own body shape, so the
webhook cannot point at it directly. A small relay that you run verifies
the signature and makes the dispatch call.
[`examples/agent-webhooks/relay.py`](../examples/agent-webhooks/relay.py)
is one: Python standard library only, with signature and timestamp checks,
and dedupe on the event id. Any serverless function that does the same
works too.

1. **The workflow.** Copy
   [`examples/agent-webhooks/railwatch-agent.yml`](../examples/agent-webhooks/railwatch-agent.yml)
   to `.github/workflows/` in the app's repository. Add the secrets
   `ANTHROPIC_API_KEY` and `RAILWATCH_TOKEN`, and the variable
   `RAILWATCH_URL`. The token is a Railwatch API & MCP token made at
   Settings > Profile, with the scopes `mcp`, `read` and `write` (`write`
   is for `add_comment`). Name it for the workflow, so its comments say
   where they came from. `workflow_dispatch` lets you try the workflow by
   hand with any issue key.
2. **The relay.** Run `relay.py` behind HTTPS with
   `RAILWATCH_WEBHOOK_SECRET`, `GITHUB_REPOSITORY=owner/repo`, and
   `GITHUB_TOKEN` (a fine-grained token on that one repository, with
   Contents: read and write; repository_dispatch needs nothing more).
3. **The integration.** In Railwatch, go to Settings > Integrations > Add,
   choose kind webhook, set the URL to the relay's HTTPS address, and
   enter the same signing secret. Then add an alert rule: event `new_issue`
   (add `regressed_issue` too, if you like), for the application, with
   that integration.
4. **Check it.** Press *Send test* on the integration. The relay answers
   204 and skips it, because test alerts carry no issue. Then run the
   workflow by hand (Actions > Railwatch agent > Run workflow) with a real
   issue key. You should see a PR and a comment on the issue in Railwatch.

What the workflow allows the agent: the Railwatch read tools and
`add_comment` (no `update_issue`, so it cannot resolve or reassign issues),
file edits, `git`, `gh pr create`, and the app's tests. Its spend is capped
with `--max-budget-usd`. A `concurrency` group per issue key, plus a check
for an existing `railwatch/<key>` branch, keep a redelivered alert from
opening a second PR.

The same body works for any other receiver: a T3 or Claude scheduled task
behind a webhook URL, a Slack bot, or a Codex run. Each one starts from
`mcp.endpoint` and `mcp.arguments`. The receiver's MCP client needs a token
of its own.
