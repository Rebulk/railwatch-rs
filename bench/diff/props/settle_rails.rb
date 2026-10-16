# Runs inside rw-baseline after ingest: computes, synchronously, everything
# the jobs container would do eventually for the scratch tenant, so the
# props capture never races a queue:
# - RollupJob and ReleaseHealthRollupJob for every hour that has rows;
# - Ingest::ExceptionGrouping for every exception (issues in the primary DB).
# Also sets expected_servers so processes#silent_servers has something to say.
# ENV: SLUG
env = Environment.find_by!(slug: ENV.fetch("SLUG"))
hours = env.with_telemetry do
  Telemetry::Execution.connection.select_values(
    "SELECT DISTINCT strftime('%Y-%m-%d %H:00:00', occurred_at) FROM (SELECT occurred_at FROM executions UNION ALL SELECT occurred_at FROM queries " \
    "UNION ALL SELECT occurred_at FROM sessions UNION ALL SELECT occurred_at FROM cache_events UNION ALL SELECT occurred_at FROM llm_calls " \
    "UNION ALL SELECT occurred_at FROM visits UNION ALL SELECT occurred_at FROM spans)"
  )
end
hours.each do |h|
  bucket = Time.zone.parse(h)
  RollupJob.perform_now(env, bucket)
  ReleaseHealthRollupJob.perform_now(env, bucket)
end
ids = env.with_telemetry { Telemetry::Exception.order(:id).pluck(:id) }
ids.each_slice(500) { |slice| GroupExceptionsJob.perform_now(env, slice) }
env.update!(expected_servers: %w[web-1 web-2 worker-1 web-9])
counts = env.with_telemetry { [Telemetry::Rollup.count, Telemetry::ReleaseHealth.count] }
puts "SETTLED hours=#{hours.size} rollups=#{counts[0]} release_health=#{counts[1]} issues=#{env.issues.count}"
