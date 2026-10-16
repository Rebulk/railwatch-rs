# Drives the real railwatch 0.9.0 gem's transport against a server:
#
#   RAILWATCH_TOKEN=rw_... RAILWATCH_INGEST_URL=http://127.0.0.1:8790 \
#     ruby bench/diff/gem_transport.rb [fixtures.json]
#
# Uses Railwatch::Transport::Http exactly as the gem's reporter does
# (#deliver for an ordinary batch, #deliver_encoded with receipt headers
# for a stored one, #ping), on records from the gem's own wire fixtures.
# Exits non-zero unless every call reports success the way the gem judges it.
gem "railwatch", "0.9.0"
require "json"
require "securerandom"
require "digest"
require "railwatch"

abort "railwatch 0.9.0 expected, got #{Railwatch::VERSION}" unless Railwatch::VERSION == "0.9.0"

config = Railwatch::Configuration.new
config.token = ENV.fetch("RAILWATCH_TOKEN")
config.ingest_url = ENV.fetch("RAILWATCH_INGEST_URL")

fixtures = JSON.parse(File.read(ARGV[0] || File.expand_path("../../crates/server/tests/fixtures/wire_fixtures.json", __dir__)))
records = fixtures.values.map do |rec|
  rec.merge("timestamp" => Time.now.to_f, "execution_id" => rec["execution_id"] && SecureRandom.uuid)
end

transport = Railwatch::Transport::Http.new(config)
failures = []

ok = transport.ping
puts "ping: #{ok}"
failures << "ping" unless ok

result = transport.deliver(records)
puts "deliver: ok=#{result.ok} status=#{result.status} accepted=#{result.accepted} rejected=#{result.rejected} disposition=#{result.disposition.inspect} error=#{result.error.inspect}"
failures << "deliver" unless result.ok && result.disposition == :stored && result.accepted == records.size

# The durable path: stored bytes, a v7 delivery id, receipt headers.
encoded = Railwatch::Transport::WireEncoder.new(batch_bytes: config.batch_bytes).encode(records)
ms = (Time.now.to_f * 1000).to_i
hex = format("%012x", ms) + "7" + SecureRandom.hex(2)[0, 3] + "8" + SecureRandom.hex(8)[0, 15]
v7 = [hex[0, 8], hex[8, 4], hex[12, 4], hex[16, 4], hex[20, 12]].join("-")
headers = { "X-Railwatch-Producer-Id" => SecureRandom.uuid, "X-Railwatch-Body-SHA256" => Digest::SHA256.hexdigest(encoded.body),
            "X-Railwatch-Policy" => "everything-v1" }
2.times do |i|
  r = transport.deliver_encoded(body: encoded.body, expected_count: encoded.sent, batch_id: v7, headers: headers)
  puts "deliver_encoded ##{i + 1}: ok=#{r.ok} status=#{r.status} accepted=#{r.accepted} ack_disposition=#{r.ack_disposition.inspect} error=#{r.error.inspect}"
  want = i.zero? ? "committed" : "already_committed"
  failures << "deliver_encoded ##{i + 1}" unless r.ok && r.ack_disposition == want && r.accepted == records.size
end

bad = Railwatch::Configuration.new
bad.token = "rw_wrong"
bad.ingest_url = config.ingest_url
r = Railwatch::Transport::Http.new(bad).deliver(records.first(1))
puts "bad token: ok=#{r.ok} status=#{r.status} retryable=#{r.retryable?}"
failures << "bad token" unless r.status == 401 && !r.retryable?

if failures.empty?
  puts "GEM TRANSPORT OK"
else
  abort "GEM TRANSPORT FAILED: #{failures.join(", ")}"
end
