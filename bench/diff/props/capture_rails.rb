# Runs inside rw-baseline (bin/rails runner). For each case in
# /tmp/props-cases.json, GETs the page as an Inertia XHR through
# ActionDispatch::Integration::Session, signed in as a scratch user, with the
# clock frozen at NOW, and writes {name => props} to /tmp/props-rails.json.
# Also dumps the environment's control rows (deploys, issues,
# expected_servers, thresholds, anomaly rules) to /tmp/props-control.json so
# the Rust side sees the same ones.
#
# ENV: SLUG (the scratch environment), NOW (epoch float).
require "active_support/testing/time_helpers"
include ActiveSupport::Testing::TimeHelpers

env = Environment.find_by!(slug: ENV.fetch("SLUG"))
app = env.application
account = app.account
now = Time.at(ENV.fetch("NOW").to_f).utc

# A scratch member of the bench account, signed in through a real Session row.
user = User.find_or_create_by!(email: "props-diff@example.com") do |u|
  u.name = "Props Diff"
  u.password = SecureRandom.hex(16)
end
account.memberships.find_or_create_by!(user: user) { |m| m.role = "owner" }

cases = JSON.parse(File.read("/tmp/props-cases.json"))
out = {}
timings = {}
travel_to(now) do
  session_row = user.sessions.create!
  s = ActionDispatch::Integration::Session.new(Rails.application)
  s.host! "localhost"
  jar = ActionDispatch::Request.new(Rails.application.env_config.merge("HTTP_HOST" => "localhost")).cookie_jar
  jar.signed[:session_token] = session_row.id
  cookie = "session_token=#{CGI.escape(jar[:session_token])}"
  version = InertiaRails.configuration.version
  version = version.call if version.respond_to?(:call)
  headers = {"X-Inertia" => "true", "X-Inertia-Version" => version.to_s, "Cookie" => cookie, "Accept" => "text/html, application/xhtml+xml",
             "User-Agent" => "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0 Safari/537.36"}
  base = "/apps/#{app.id}/envs/#{env.id}"
  warm = ENV.fetch("WARM", "1").to_i
  cases.each do |c|
    path = c["path"].sub("{base}", base)
    # Warm runs: Rails.cache (grouped/summary_with_delta) would serve a
    # cached answer on the timed run; clear it so every run computes, as the
    # Rust side does.
    samples = []
    status = nil
    (warm + 1).times do
      Rails.cache.clear
      t0 = Process.clock_gettime(Process::CLOCK_MONOTONIC)
      s.get path, params: c["params"] || {}, headers: headers
      samples << (Process.clock_gettime(Process::CLOCK_MONOTONIC) - t0) * 1000.0
      status = s.response.status
    end
    if status == 200
      page = JSON.parse(s.response.body)
      out[c["name"]] = {"component" => page["component"], "props" => page["props"]}
    else
      out[c["name"]] = {"status" => status, "body" => s.response.body.to_s.first(300)}
    end
    timings[c["name"]] = samples.drop(1).sort
  end
end

File.write("/tmp/props-rails.json", JSON.generate(out))
File.write("/tmp/props-rails-timings.json", JSON.generate(timings))
control = {
  deploys: env.deploys.map { |d| {id: d.id, deploy: d.deploy, ref: d.ref, name: d.name, url: d.url, server: d.server,
    deployed_at: (d.deployed_at.to_r * 1_000_000).to_i, previous_ref: d.previous_ref, commits: d.commits, detail: d.detail} },
  issues: env.issues.includes(:application).map { |i| {id: i.id, key: i.key, title: i.title, kind: i.kind, status: i.status, priority: i.priority,
    occurrences: i.occurrences, affected_users: i.affected_users, culprit: i.culprit, group_hash: i.group_hash,
    first_seen_at: (i.first_seen_at.to_r * 1_000_000).to_i, last_seen_at: (i.last_seen_at.to_r * 1_000_000).to_i,
    resolved_in_deploy: i.resolved_in_deploy, sample: i.sample} },
  expected_servers: env.expected_servers,
  thresholds: env.thresholds.order(:target_kind, :target).map { |t| t.slice(:id, :target_kind, :target, :metric, :limit, :window_minutes).merge(description: t.description) }.as_json,
  anomaly_rules: env.anomaly_rules.order(:target_kind, :target).map { |r| r.slice(:id, :target_kind, :target, :metric, :deviation, :window_minutes, :baseline_days, :enabled, :last_fired_at).merge(description: r.description) }.as_json
}
File.write("/tmp/props-control.json", JSON.generate(control))
puts "CAPTURED #{out.size} cases (#{out.count { |_, v| v['status'] }} non-200)"
