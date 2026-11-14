# frozen_string_literal: true

# Benchmark diagnostic only (mechanism.sh), never for real data: every
# SQLite connection runs with PRAGMA synchronous = OFF, the Rails twin of
# railwatch-rs's RAILWATCH_SQLITE_SYNC=off. If throughput does not move,
# fsync is not the bottleneck.
module BenchSyncOff
  def configure_connection
    super
    raw_execute("PRAGMA synchronous = OFF", "SCHEMA")
  end
end
ActiveSupport.on_load(:active_record_sqlite3adapter) { prepend BenchSyncOff }
