# Runs Ingest::ExceptionGrouping (GroupExceptionsJob's code) over a tenant's
# exceptions, in id-ordered chunks as batches would bring them, and prints
# the resulting issues and activities as JSON for compare_grouping.py.
#   SLUG=... CHUNK=50 DEPLOY_AT="2026-10-13 16:20:00" bin/rails runner grouping_rails.rb
require "json"
env = Environment.find_by!(slug: ENV.fetch("SLUG"))
if ENV["DEPLOY_AT"].present?
  env.deploys.create!(deploy: "d-#{Time.now.to_i}", deployed_at: Time.zone.parse(ENV["DEPLOY_AT"] + " UTC"))
end
ids = env.with_telemetry { Telemetry::Exception.order(:id).pluck(:id) }
# GroupExceptionsJob's body, minus rows with no group_hash: on the direct
# path Rails passes them to record_occurrence!, which raises (Group hash
# can't be blank) and fails the job, leaving every later group of that batch
# ungrouped. The receipted path (Ingest::Followups) filters them first, and
# so does the Rust port on both paths; filtering here compares the rest.
ids.each_slice(Integer(ENV.fetch("CHUNK", "50"))) do |slice|
  rows = env.with_telemetry { Telemetry::Exception.where(id: slice).to_a }.select { |row| row.group_hash.present? }
  Ingest::ExceptionGrouping.new(env, rows).call
end
# RESOLVE_AND_REPEAT=1: resolve every issue (half of them "in" deploy
# abc123), then group the same exceptions again: regressions, and the
# resolved_in_deploy rule, exercised.
if ENV["RESOLVE_AND_REPEAT"] == "1"
  env.issues.order(:id).each_with_index { |i, n| i.resolve!(deploy: n.even? ? "abc123" : nil) }
  ids.each_slice(Integer(ENV.fetch("CHUNK", "50"))) do |slice|
    rows = env.with_telemetry { Telemetry::Exception.where(id: slice).to_a }.select { |row| row.group_hash.present? }
    Ingest::ExceptionGrouping.new(env, rows).call
  end
end
issues = env.issues.order(:id).map do |i|
  { group_hash: i.group_hash, kind: i.kind, title: i.title, culprit: i.culprit, status: i.status, occurrences: i.occurrences,
    affected_users: i.affected_users, first_seen_at: i.first_seen_at.utc.iso8601(6), last_seen_at: i.last_seen_at.utc.iso8601(6),
    source: i.source, sample: i.sample, number_rank: i.number,
    activities: i.activities.order(:id).map { |a| [a.kind, a.data] } }
end
min = issues.map { |i| i[:number_rank] }.min || 0
issues.each { |i| i[:number_rank] -= min }
puts "RWJSON #{JSON.generate(issues)}"
