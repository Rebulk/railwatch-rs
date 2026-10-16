#!/usr/bin/env bash
# Cross-open: Rails (rw-baseline) reads a tenant file the Rust server wrote.
#
#   bench/diff/cross_open.sh RUST_TENANT.sqlite3
#
# Creates a scratch environment `xopen-<run>` in the bench application,
# replaces its freshly created tenant file with the Rust-written one (copied
# into the rw-baseline volume under that slug), then runs, inside Rails:
# pending-migration check, row counts per telemetry model, and a few records
# loaded through the models (JSON columns decoded, times parsed, the
# execution presenter's associations). env-0001 is not touched.
set -euo pipefail
SRC=${1:?usage: cross_open.sh RUST_TENANT.sqlite3}
RUN=$(date +%s)
NAME=xopen-$RUN
SLUG=$(docker exec -i -e NAME="$NAME" rw-baseline bin/rails runner - <<'RUBY' | sed -n 's/^SLUG //p'
account = Account.find_by!(slug: "bench")
app = account.applications.find_by!(slug: "bench")
env = app.environments.create!(name: ENV.fetch("NAME"))
puts "SLUG #{env.slug}"
RUBY
)
DEST=/rails/storage/telemetry/$SLUG.sqlite3
TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT
sqlite3 "$SRC" ".backup $TMP/tenant.sqlite3"
docker exec rw-baseline rm -f "$DEST" "$DEST-wal" "$DEST-shm"
docker cp "$TMP/tenant.sqlite3" "rw-baseline:$DEST" >/dev/null
docker exec -u 0 rw-baseline chown rails:rails "$DEST"
echo "copied $(basename "$SRC") -> $DEST"

docker exec -i -e SLUG="$SLUG" rw-baseline bin/rails runner - <<'RUBY' 2>&1 | grep -v '^\s*from ' | grep -E '^XO |Error|error'
env = Environment.find_by!(slug: ENV.fetch("SLUG"))
env.with_telemetry do
  conn = TelemetryRecord.connection
  pending = conn.pool.migration_context.open.pending_migrations
  puts "XO pending_migrations=#{pending.size}"
  puts "XO environment=#{conn.select_value("SELECT value FROM ar_internal_metadata WHERE key='environment'")}"
  Telemetry.constants.map { |c| Telemetry.const_get(c) }
    .select { |k| k.is_a?(Class) && k < TelemetryRecord && !k.abstract_class? && conn.table_exists?(k.table_name) }
    .sort_by(&:table_name).each { |k| puts "XO count #{k.table_name}=#{k.count}" }
  check = ->(label, &blk) { puts "XO #{label} #{blk.call}" rescue puts("XO FAIL #{label}: #{$!.class}: #{$!.message[0, 200]}") }
  check.("execution") { e = Telemetry::Execution.order(:id).first; "#{e.kind} #{e.name.inspect} occurred_at=#{e.occurred_at.iso8601(6)} stages=#{e.stages.class}/#{e.stages.size} detail_keys=#{e.detail.keys.first(4)}" }
  check.("query_with_shape") { q = Telemetry::Query.with_sql.where.not(group_hash: nil).where(id: Telemetry::QueryShape.select(:group_hash).then { |s| Telemetry::Query.where(group_hash: s).select(:id) }).first; "sql=#{q.sql.to_s[0, 50].inspect}" }
  check.("exception") { ex = Telemetry::Exception.first; "#{ex.class_name} frames=#{ex.frames.class}/#{ex.frames.size} handled=#{ex.handled.inspect} fingerprint=#{ex.fingerprint.class}" }
  check.("log_fts") { l = Telemetry::Log.first; word = l.message.split.first.to_s.gsub(/\W/, ""); "search(#{word.inspect}) hits=#{Telemetry::Log.fts(word).count}" }
  check.("profile") { p = Telemetry::Profile.first; p ? "collapsed_bytes=#{p.collapsed.bytesize}" : "none in file" }
  check.("attachment") { a = Telemetry::Attachment.first; a ? "body_bytes=#{a.body.bytesize}" : "none in file" }
  check.("health") { h = Telemetry::HealthSample.first; "detail=#{h.detail.class} keys=#{h.detail.keys.first(3)}" }
  check.("ingest_batch") { b = Telemetry::IngestBatch.recent.first; "received_at=#{b.received_at.iso8601(6)} counts=#{b.counts_by_type.size} rejections=#{b.rejections.size}" }
  check.("receipt") { r = Telemetry::IngestReceipt.first; r ? "pending=#{r.followups_pending?} committed_at=#{r.committed_at.iso8601(6)} ack=#{r.ack.keys}" : "none" }
  check.("rails_write") { Telemetry::Person.touch_all([[{"id" => "xopen"}, Time.current]]); "people=#{Telemetry::Person.count}" }
end
RUBY
echo "scratch environment: $SLUG"
