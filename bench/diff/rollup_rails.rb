# Runs RollupJob's and ReleaseHealthRollupJob's own code on one tenant, for
# the given hours, and prints per-hour timings.
#   SLUG=... SEED=42 HOURS="1760000000 1760003600" bin/rails runner rollup_rails.rb
#
# tdigest's compress! shuffles with Ruby's global RNG, so both sides seed it
# the same way: srand(SEED + hour index) right before each hour's recompute
# (rw-rollup --seed does the same). Other code draws from that RNG too: the
# first query of the process takes two MT words (lazy setup in the AR/sqlite3
# stack). So one untimed warm-up pass runs first; recomputing is idempotent,
# and the second, timed pass is what is compared.
# RollupJob#recompute is called directly (perform_now adds job bookkeeping).
env = Environment.find_by!(slug: ENV.fetch("SLUG"))
hours = ENV.fetch("HOURS").split.map { |h| Time.at(Integer(h)).utc }
seed = Integer(ENV.fetch("SEED"))
job = RollupJob.new
rh = ReleaseHealthRollupJob.new
run = lambda do |timed|
  total = Process.clock_gettime(Process::CLOCK_MONOTONIC)
  hours.each_with_index do |bucket, i|
    t = r = nil
    env.with_telemetry do
      srand(seed + i)
      t = Process.clock_gettime(Process::CLOCK_MONOTONIC)
      Railwatch.ignore { job.send(:recompute, env, bucket, bucket...(bucket + 1.hour)) }
      r = Process.clock_gettime(Process::CLOCK_MONOTONIC)
    end
    rh.perform(env, bucket)
    next unless timed
    puts "RW hour #{bucket.to_i} rollup_ms=#{((r - t) * 1000).round(1)} release_health_ms=#{((Process.clock_gettime(Process::CLOCK_MONOTONIC) - r) * 1000).round(1)}"
  end
  puts "RW total_ms=#{((Process.clock_gettime(Process::CLOCK_MONOTONIC) - total) * 1000).round(1)} rollups=#{env.with_telemetry { Telemetry::Rollup.count }}" if timed
end
run.call(false)
run.call(true)
