# Fixtures the bench account lacks, so the diff covers every branch of
# WeeklyDigestJob and both views. Idempotent (find_or_create_by!).
#   - a member with weekly_digest off (not a recipient);
#   - account "Digest <Edge> & Co" with one recipient and three
#     environments: one whose tenant has request and job_attempt rollups in
#     the window (copied from a bench tenant's), one whose tenant exists but
#     is empty (skipped), one with rollups only before the window (skipped);
#     its live environment has exactly one new issue and one deploy
#     (singular words), a resolved issue, HTML-special characters and a
#     130-character title (title.first(120));
#   - account "Quiet" whose only member has the digest off (no recipients).
now = Time.at(Integer(ENV.fetch("NOW"))).utc
src = Environment.find_by!(slug: ENV.fetch("SRC_SLUG"))
pw = "correct horse battery staple"
user = ->(email, name, digest) {
  User.find_or_create_by!(email: email) { |u| u.name = name; u.password = pw; u.verified = true; u.weekly_digest = digest }
}
bench = Account.order(:id).first
off = user.("digest-off@example.com", "Off", false)
Membership.find_or_create_by!(account: bench, user: off)

edge = Account.find_by(name: "Digest <Edge> & Co") || Account.create!(name: "Digest <Edge> & Co")
Membership.find_or_create_by!(account: edge, user: user.("o'brien@example.com", %q(Siobhán "Sam" O'Brien), true)) { |m| m.role = "owner" }
Membership.find_or_create_by!(account: edge, user: off)
app = edge.applications.find_by(slug: "shop") || edge.applications.create!(name: "Shop & <Cart>", slug: "shop", issue_prefix: "SHOP")
live, empty, stale = %w[production staging old].map { |n| app.environments.find_by(name: n) || app.environments.create!(name: n) }
rows = src.with_telemetry { Railwatch::Telemetry::Rollup.where(record_type: %w[request job_attempt]).map(&:attributes) }
live.with_telemetry do
  Railwatch::Telemetry::Rollup.delete_all
  Railwatch::Telemetry::Rollup.insert_all!(rows.map { |r| r.except("id") })
end
empty.with_telemetry { Railwatch::Telemetry::Rollup.delete_all }
stale.with_telemetry do
  Railwatch::Telemetry::Rollup.delete_all
  Railwatch::Telemetry::Rollup.insert_all!(rows.first(5).each_with_index.map { |r, i| r.except("id").merge("bucket" => (now - 30.days).beginning_of_hour - i.hours) })
end
issue = ->(hash, attrs) {
  Issue.find_by(environment: live, group_hash: hash) ||
    Issue.create!({environment: live, application: app, group_hash: hash, kind: "exception", sample: {}}.merge(attrs))
}
issue.("a" * 32, title: %q(NoMethodError: undefined method `<=>' for "a" & 'b'), occurrences: 1_234_567, first_seen_at: now - 2.days, last_seen_at: now - 1.hour)
issue.("b" * 32, title: "RuntimeError: " + "x" * 116 + "TRUNCATED", occurrences: 999, first_seen_at: now - 30.days, last_seen_at: now - 1.day)
issue.("c" * 32, title: "Resolved: gone", occurrences: 5_000_000, status: "resolved", resolved_at: now - 1.day, first_seen_at: now - 40.days, last_seen_at: now - 3.days)
issue.("d" * 32, title: "Second", occurrences: 1000, first_seen_at: now - 20.days, last_seen_at: now - 1.day)
issue.("e" * 32, title: "Fourth (not shown)", occurrences: 3, first_seen_at: now - 20.days, last_seen_at: now - 1.day)
live.deploys.find_or_create_by!(deploy: "digest-v1") { |d| d.deployed_at = now - 3.days }
live.deploys.find_or_create_by!(deploy: "digest-v0") { |d| d.deployed_at = now - 9.days }

quiet = Account.find_by(name: "Quiet") || Account.create!(name: "Quiet")
Membership.find_or_create_by!(account: quiet, user: user.("quiet@example.com", "Quiet", false))
qapp = quiet.applications.find_by(slug: "q") || quiet.applications.create!(name: "Q", slug: "q", issue_prefix: "Q")
qenv = qapp.environments.find_by(name: "production") || qapp.environments.create!(name: "production")
qenv.with_telemetry do
  Railwatch::Telemetry::Rollup.delete_all
  Railwatch::Telemetry::Rollup.insert_all!(rows.first(5).map { |r| r.except("id") })
end
[live, empty, stale, qenv].each { |e| e.with_telemetry { Railwatch::TelemetryRecord.connection.execute("PRAGMA wal_checkpoint(TRUNCATE)") } }
puts "SETUP edge=#{edge.id} quiet=#{quiet.id}"
