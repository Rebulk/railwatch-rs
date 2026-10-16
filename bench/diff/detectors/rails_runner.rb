# Replays bench/diff/detectors scenarios through railwatch-cloud's own jobs
# and models, inside rw-baseline:
#
#   RUN=... HOOK=https://172.17.0.1:PORT SSL_CERT_FILE=/tmp/det/ca.pem \
#     bin/rails runner /tmp/det/rails_runner.rb
#
# Each scenario gets a fresh primary database (/tmp/det/out/<name>.sqlite3,
# db/schema.rb loaded; rw-baseline's own production.sqlite3 is never
# touched) and its own tenant file, deleted at the end. Every step runs
# under travel_to(step time). Jobs go to an in-memory adapter instead of
# Solid Queue: scan steps run the jobs they enqueue at once, except alert
# delivery and burst summaries, which only a "deliver" step runs (after
# ReconcileAlertDeliveriesJob), when they are due. The receiver's address is
# private, so OutboundHttp's public-address check is switched off here,
# which is what RAILWATCH_ALLOW_PRIVATE_ALERT_TARGETS does on the Rust side.
require "json"
require "active_support/testing/time_helpers"
include ActiveSupport::Testing::TimeHelpers

RUN = ENV.fetch("RUN")
HOOK = ENV.fetch("HOOK")
SCENARIOS = JSON.parse(File.read("/tmp/det/scenarios.json"))
ONLY = ENV["ONLY"].to_s.split(",")
Dir.mkdir("/tmp/det/out") unless Dir.exist?("/tmp/det/out")

Alerts::OutboundHttp.define_singleton_method(:ensure_public!) { |_address| nil }
ActiveModel::SecurePassword.min_cost = true

class CaptureAdapter
  attr_reader :jobs

  def initialize = @jobs = []
  def enqueue(job) = @jobs << [job, nil]
  def enqueue_at(job, timestamp) = @jobs << [job, Time.at(timestamp)]
  def enqueue_after_transaction_commit? = true
  def stopping? = false

  DELIVERY = [DeliverAlertJob, SummarizeAlertBurstJob].freeze

  # Runs queued jobs (and whatever they enqueue) until none is runnable.
  def drain(delivery:)
    loop do
      runnable = @jobs.select { |job, at| DELIVERY.include?(job.class) == delivery && (at.nil? || at <= Time.current) }
      break if runnable.empty?
      @jobs -= runnable
      runnable.each { |job, _| ActiveJob::Base.execute(job.serialize) }
    end
  end
end

def schema!(path)
  File.delete(path) if File.exist?(path)
  ActiveRecord::Base.establish_connection(adapter: "sqlite3", database: path)
  ActiveRecord::Schema.verbose = false
  load Rails.root.join("db/schema.rb").to_s
end

def sql_literal_tenant(env, stmts)
  env.with_telemetry { stmts.each { |s| Railwatch::TelemetryRecord.connection.execute(s) } }
end

def run_scan(name, adapter)
  case name
  when "performance_scan" then PerformanceScanJob.perform_now
  when "anomaly_scan" then AnomalyScanJob.perform_now
  when "silent_host_check" then SilentHostCheckJob.perform_now
  when "scheduled_task_check" then ScheduledTaskScanJob.perform_now
  when "release_health_check" then ReleaseHealthCheckJob.perform_now
  when "quota_check" then QuotaCheckJob.perform_now
  when "auto_resolve_issues" then AutoResolveIssuesJob.perform_now
  else raise "unknown scan #{name}"
  end
  adapter.drain(delivery: false)
end

done = 0
SCENARIOS.each_with_index do |sc, index|
  next if ONLY.any? && !ONLY.include?(sc["name"])
  adapter = CaptureAdapter.new
  ActiveJob::Base.queue_adapter = adapter
  path = "/tmp/det/out/#{sc["name"]}.sqlite3"
  env = nil
  # Setup on the real clock: creating the tenant runs its migrations, which
  # Rails refuses when "now" is before a migration's timestamp. Setup rows
  # are not compared.
  begin
    schema!(path)
    ada = User.create!(name: "Ada", email: "ada@example.com", password: "correct horse battery")
    grace = User.create!(name: "Grace", email: "grace@example.com", password: "correct horse battery")
    account = Account.create!(name: "det#{RUN}s#{index}", plan: "free")
    account.update_columns(sc["account"].slice("monthly_event_quota", "additional_events_cap", "auto_resolve_after_days"))
    Membership.create!(account: account, user: ada, role: "owner")
    Membership.create!(account: account, user: grace, role: "member")
    app = account.applications.create!(name: "Shop", issue_prefix: "DET")
    env = app.environments.create!(name: "production")
    env.update_columns(sc["env"].transform_values { |v| v })
    integrations = sc["integrations"].map do |i|
      integration = Integration.new(account: account, kind: "webhook", name: i["name"], enabled: i["enabled"],
        settings: {"url" => "#{HOOK}/rails/#{sc["name"]}/#{i["path"]}", "secret" => "det-secret"})
      integration.save!(validate: false)
      integration
    end
    # validate: false: an unparsable title_pattern stands for a rule saved
    # before patterns were validated (AlertRule#title_matches? handles it).
    sc["rules"].each { |r| app.alert_rules.new(event: r["event"], integration: integrations[r["integration"]], filters: r["filters"]).save!(validate: false) }
    sc["thresholds"].each { |t| env.thresholds.create!(t) }
    sc["anomaly_rules"].each { |a| env.anomaly_rules.create!(a) }
  end
  sc["steps"].each do |step|
    travel_to(Time.iso8601(step["at"])) do
      env.reload
      case step["do"]
      when "tenant_sql" then sql_literal_tenant(env, step["sql"])
      when "deploy" then env.deploys.create!(deploy: step["deploy"], ref: step["ref"], deployed_at: Time.zone.parse(step["deployed_at"] + " UTC"))
      when "usage"
        IngestUsageMonth.where(environment_id: env.id, month: Date.parse(step["month"])).delete_all
        IngestUsageMonth.create!(environment: env, month: Date.parse(step["month"]), events: step["events"])
      when "env" then env.update_columns(step.except("at", "do"))
      when "account" then env.account.update_columns(step.except("at", "do"))
      when "scan" then run_scan(step["scan"], adapter)
      when "group"
        rows = env.with_telemetry { Telemetry::Exception.where(id: step["ids"]).to_a }
        Ingest::ExceptionGrouping.new(env, rows).call
        adapter.drain(delivery: false)
      when "issue"
        issue = env.issues.find_by!(number: step["number"])
        case step["action"]
        when "resolve" then issue.resolve!(deploy: step["deploy"])
        when "ignore" then issue.ignore!
        when "reopen" then issue.reopen!
        when "assign" then issue.update!(assignee: step["value"] && User.find(step["value"]))
        when "priority" then issue.update!(priority: step["value"])
        else raise "unknown action #{step["action"]}"
        end
        adapter.drain(delivery: false)
      when "deliver"
        ReconcileAlertDeliveriesJob.perform_now
        adapter.drain(delivery: true)
      else raise "unknown step #{step["do"]}"
      end
    end
  end
  # IssueDetectionPresenter over every issue at the last step's time, as
  # issues#show / MCP render it.
  travel_to(Time.iso8601(sc["steps"].last["at"])) do
    presented = env.issues.order(:id).map { |i| [i.number, IssueDetectionPresenter.new(i).as_json] }.to_h
    File.write("/tmp/det/out/#{sc["name"]}.presented.json", presented.to_json) # as_json Times, as issues#show renders
  end
  env.destroy_telemetry_database
  ActiveRecord::Base.connection.disconnect!
  done += 1
end
puts "RAILS DONE #{done}"
