# Runs inside rw-baseline. Issues the API diff's tokens for the props-diff
# user (an owner of the bench account) and writes {name => raw} to
# /tmp/api-tokens.json. The Rust side gets the same rows (digests) through
# import-rails of a backup taken after this runs.
#
#   full          api mcp read write
#   read_only     api read             (writes: 403)
#   mcp_only      mcp read write       (no api: 403)
#   no_read       api write            (no read: 403)
#   revoked       all scopes, revoked  (401)
#   expired       all scopes, expires_at in the past (401)
#   other_app     all scopes, restricted to a second application
#   account_only  api read, restricted to the bench account
#
# Plus what the diff reads that a props work dir does not have: a second
# application, an assignable second member, a merged issue, deploys with
# names and urls, and a comment by each source. Idempotent by RUN.
#
# ENV: SLUG (the scratch environment), NOW (epoch; the expired token's expiry
# and the fixtures' times sit before it).
require "active_support/testing/time_helpers"
include ActiveSupport::Testing::TimeHelpers
env = Environment.find_by!(slug: ENV.fetch("SLUG"))
account = env.application.account
user = User.find_by!(email: "props-diff@example.com")
now = Time.at(ENV.fetch("NOW").to_i).utc
run = Time.now.to_i
other = account.applications.find_or_create_by!(slug: "mcp-diff-other") { |a| a.name = "mcp diff other" }
member = User.find_or_create_by!(email: "api-diff-member@example.com") do |u|
  u.name = "API Diff Member"
  u.password = SecureRandom.hex(16)
  u.verified = true
  u.terms_accepted_at = Time.current
end
account.memberships.find_or_create_by!(user: member) { |m| m.role = "member" }

tokens = {}
{
  "full" => {scopes: %w[api mcp read write]},
  "read_only" => {scopes: %w[api read]},
  "mcp_only" => {scopes: %w[mcp read write]},
  "no_read" => {scopes: %w[api write]},
  "revoked" => {scopes: %w[api mcp read write]},
  "expired" => {scopes: %w[api mcp read write]},
  "other_app" => {scopes: %w[api mcp read write], application: other},
  "account_only" => {scopes: %w[api read], account: account}
}.each do |name, opts|
  token, raw = ApiToken.issue!(user: user, name: "api-diff-#{name}-#{run}", **opts)
  token.revoke! if name == "revoked"
  token.update_columns(expires_at: now - 3600) if name == "expired"
  tokens[name] = raw
end

# Deploys: named, with urls and refs, in the window, fixed times (the
# write cases re-post one of them and add a new one).
[["api-diff-a", "aaaa111", "Release A", "https://ci.example.com/a", now - 2 * 3600],
 ["api-diff-b", "bbbb222", nil, nil, now - 26 * 3600]].each do |name, ref, label, url, at|
  d = env.deploys.find_or_initialize_by(deploy: name)
  d.update!(ref: ref, name: label, url: url, deployed_at: at)
end

# A merged issue (status open on it would unmerge) and a Linear comment.
issues = env.issues.where(kind: "exception").order(:id).to_a
src, target = issues[-1], issues[-2]
if src && target && !src.merged? && target.open?
  Current.set(api_user: user) { src.merge_into!(target) }
end
target&.comments&.find_or_create_by!(source: "linear", author_name: "Linear Bot", external_id: "api-diff-#{target.id}") { |c| c.body = "Synced from Linear" }

# A performance issue with a detection snapshot: a p95 threshold on the
# busiest route (low enough to breach) and Rails' own detector at NOW, so
# the issue page's detection block (trend, deploys, representative records)
# is compared too. The threshold is removed again so later runs and the
# jobs container do not keep firing it.
unless env.issues.where(kind: "performance").any? { |i| i.sample.is_a?(Hash) && i.sample["detection"].is_a?(Hash) }
  route = env.with_telemetry { Railwatch::Telemetry::Rollup.for_type("request").group(:name).order(Arel.sql("SUM(count) DESC")).limit(1).pluck(:name).first }
  if route
    threshold = env.thresholds.create!(target_kind: "requests", target: route, metric: "p95", limit: 1, window_minutes: 1_440)
    travel_to(now - 120) { DetectPerformanceIssuesJob.perform_now(env) }
    threshold.destroy
  end
end

# Paused, so rw-baseline-jobs' scans (Environment.active) cannot open or
# touch issues between the snapshot and the capture.
env.update_columns(paused_at: now - 60) unless env.paused?

File.write("/tmp/api-tokens.json", JSON.generate(tokens))
puts "TOKENS #{tokens.size} MEMBER #{member.id}"
