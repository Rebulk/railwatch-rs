# Runs inside rw-baseline. Issues the MCP diff's API tokens for the
# props-diff user (an owner of the bench account) and writes
# {name => raw} to /tmp/mcp-tokens.json. The Rust side gets the same rows
# (digests) through import-rails of a backup taken after this runs.
#
#   full        api mcp read write
#   read_only   mcp read
#   api_only    api read write        (no mcp: 403)
#   mcp_no_read mcp write             (no read: 403)
#   revoked     all scopes, revoked   (401)
#   other_app   all scopes, restricted to a second application
#
# ENV: SLUG (the scratch environment)
env = Environment.find_by!(slug: ENV.fetch("SLUG"))
account = env.application.account
user = User.find_by!(email: "props-diff@example.com")
other = account.applications.find_or_create_by!(slug: "mcp-diff-other") { |a| a.name = "mcp diff other" }
run = Time.now.to_i
tokens = {}
{
  "full" => {scopes: %w[api mcp read write]},
  "read_only" => {scopes: %w[mcp read]},
  "api_only" => {scopes: %w[api read write]},
  "mcp_no_read" => {scopes: %w[mcp write]},
  "revoked" => {scopes: %w[api mcp read write]},
  "other_app" => {scopes: %w[api mcp read write], application: other}
}.each do |name, opts|
  token, raw = ApiToken.issue!(user: user, name: "mcp-diff-#{name}-#{run}", **opts)
  token.revoke! if name == "revoked"
  tokens[name] = raw
end
# list_alerts reads these: rows inserted directly (no delivery job, fixed
# times), so both sides read the same ones. Two rules on events the write
# cases never fire, a rule-less "Send test" alert, statuses Rails' STATUSES
# has beyond the schema's usual three, and one alert on another app's rule.
integration = Integration.insert_all!([{account_id: account.id, kind: "webhook", name: "mcp-diff-#{run}", enabled: true,
  created_at: Time.current, updated_at: Time.current}], returning: %w[id]).first["id"]
rule = ->(app, event) {
  AlertRule.insert_all!([{application_id: app.id, integration_id: integration, event: event, filters: {},
    created_at: Time.current, updated_at: Time.current}], returning: %w[id]).first["id"]
}
new_issue, threshold, other_rule = rule[env.application, "new_issue"], rule[env.application, "threshold"], rule[other, "new_issue"]
issues = env.issues.order(:id).limit(3).to_a
base = Time.utc(2026, 10, 1, 12)
rows = [
  [new_issue, issues[0], "new_issue", "sent", base, base + 2, nil],
  [new_issue, issues[1], "new_issue", "failed", base + 60, nil, "HTTP 500 from hook"],
  [threshold, issues[2], "threshold", "collapsed", base + 120, nil, nil],
  [nil, nil, "new_issue", "sent", base + 180, base + 181, nil],
  [other_rule, nil, "new_issue", "skipped", base + 240, nil, nil],
  [threshold, issues[0], "threshold", "pending", base + 300, nil, nil]
]
Alert.insert_all!(rows.map { |r, i, event, status, at, sent, error|
  {alert_rule_id: r, integration_id: integration, issue_id: i&.id, event: event, status: status, payload: {},
   created_at: at, updated_at: at, sent_at: sent, error: error}
})
File.write("/tmp/mcp-tokens.json", JSON.generate(tokens))
puts "TOKENS #{tokens.size}"
