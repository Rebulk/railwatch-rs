# frozen_string_literal: true

# THE COUNTERFACTUAL, benchmark-only: "what if the Rails server had
# coalesced concurrent batches the way railwatch-rs does?"
#
# Mounted read-only into the unchanged railwatch-cloud production image as an
# initializer (bench/rails-vs-rust/counterfactual.sh). It changes one thing,
# Ingest::Batch#write! on the direct (non-receipted) path, which is the path
# the gem and rw-load use:
#
#   before: every request opens its own BEGIN IMMEDIATE ... COMMIT on the
#           tenant file, so N concurrent senders to one app queue on SQLite's
#           write lock (busy handler, 1 ms sleeps) and commit N times;
#   after:  requests for the same environment in the same Puma worker join a
#           queue. One of them (the leader) takes up to MAX_BATCHES queued
#           batches, writes them in ONE transaction with a SAVEPOINT per
#           batch (a failing batch rolls back alone), commits once, and hands
#           each request its own result. Leadership passes to the next queued
#           request after every group, so no request is held hostage. This is
#           the shape of railwatch-rs's per-tenant writer (tenants.rs), minus
#           the dedicated thread: a leader/follower group commit.
#
# Parsing, mapping and validation still run on each request's own thread
# before it queues; the usage ledger, job enqueues and the live broadcast
# still run per request after its commit, unchanged. The receipted path is
# untouched (super).
#
# Also part of the fix: PRAGMA temp_store = MEMORY on the tenant connection,
# because a SAVEPOINT makes SQLite keep statement journals, and in a
# container those land in /var/tmp (railwatch-rs found this cost ~35% of its
# CPU and made the same change, tenants.rs open_tenant).
#
# Limits, stated plainly: production runs 2 Puma workers, so up to two
# leaders (one per process) still contend for the same file; coalescing is
# per process. Ruby's GVL is released during SQLite calls (sqlite3 2.x) but
# not during the mapping, so 5 threads per worker are still at most ~1 core
# of Ruby each.
module BenchGroupCommit
  MAX_BATCHES = 64
  State = Struct.new(:mutex, :pending, :leader)
  Slot = Struct.new(:batch, :queue)
  STATES = Concurrent::Map.new
  STATS = Concurrent::Map.new # groups, batches: logged every 20 groups

  def write!
    return super if @receipt

    map_all
    log_truncations
    state = STATES.compute_if_absent(@environment.id) { State.new(Mutex.new, [], false) }
    slot = Slot.new(self, Thread::Queue.new)
    lead = state.mutex.synchronize do
      state.pending << slot
      state.leader ? false : (state.leader = true)
    end
    message = lead ? [:lead] : slot.queue.pop
    while message.first == :lead
      group = state.mutex.synchronize { state.pending.shift(MAX_BATCHES) }
      BenchGroupCommit.commit_group(@environment, group)
      state.mutex.synchronize do
        if state.pending.empty?
          state.leader = false
        else
          state.pending.first.queue << [:lead]
        end
      end
      message = slot.queue.pop
    end
    _, accepted, error = message
    raise error if error

    @environment.count_events!(accepted, at: @received_at)
    enqueue_followups
    broadcast_live
    Ingest::Batch::Result.new(accepted: accepted, rejected: @rejections.size, rejections: @rejections, committed_at: nil)
  end

  # The body of the original write!'s transaction, for one batch.
  def write_in_group
    accepted = 0
    @exception_ids = Ingest::Writer.new(@rows_by_class).write!
    link_profiles!
    accepted += @rows_by_class.values.sum(&:size)
    Telemetry::Person.touch_all(@people.map { |rec| [rec, Time.at(rec["timestamp"].to_f).utc] })
    accepted += @people.size
    Telemetry::IngestBatch.create!(received_at: @received_at, accepted: accepted, rejected: @rejections.size,
                                   dropped_by_client: @dropped_by_client, backpressure_factor: @backpressure_factor,
                                   bytes: @bytes, gem_version: @gem_version,
                                   counts_by_type: @counts, rejections: @rejections.first(20))
    accepted
  end

  def self.commit_group(environment, group)
    results = {}
    begin
      environment.with_telemetry do
        TelemetryRecord.connection.execute("PRAGMA temp_store = MEMORY")
        TelemetryRecord.transaction do
          group.each do |slot|
            TelemetryRecord.transaction(requires_new: true) { results[slot] = [slot.batch.send(:write_in_group), nil] }
          rescue StandardError => e
            results[slot] = [nil, e]
          end
        end
      end
      groups = STATS.compute(:groups) { |v| (v || 0) + 1 }
      batches = STATS.compute(:batches) { |v| (v || 0) + group.size }
      # Cumulative per process; counterfactual.sh reads the last line of each pid.
      if (groups % 20).zero?
        $stdout.puts "BENCH_GROUP_COMMIT pid=#{Process.pid} groups=#{groups} batches=#{batches}"
        $stdout.flush
      end
    rescue StandardError => e
      group.each { |slot| results[slot] = [nil, e] } # the outer COMMIT failed: nothing landed
    end
    group.each { |slot| slot.queue << [:done, *results.fetch(slot)] }
  end
end

Rails.application.config.to_prepare do
  Ingest::Batch.prepend(BenchGroupCommit) unless Ingest::Batch < BenchGroupCommit
end
