# Emits t-digest vectors from the real tdigest 0.2.1 gem: inputs, srand seed,
# as_small_bytes (hex), percentiles, and merged_percentiles across digests.
require "json"
require "tdigest"
out = { "mt" => {}, "shuffle" => {}, "digests" => [], "absorb" => [], "merged" => [] }
srand(42); out["mt"]["42"] = 12.times.map { Random.rand(2**32) }
srand(123456789); out["mt"]["123456789"] = 5.times.map { Random.rand(2**32) }
srand(42); out["shuffle"]["42"] = (1..300).to_a.shuffle
gen = Random.new(99)
cases = []
cases << [5, 3, 9, 1, 7, 7, 2]
cases << [42]
cases << (1..20_000).to_a                      # sorted: forces the in-push compress!
cases << (1..6000).to_a.reverse
cases << Array.new(3000) { 7 } + Array.new(1000) { 9 }
cases << Array.new(50_000) { gen.rand(1..2_000_000) }
cases << Array.new(5000) { (gen.rand ** 3 * 10_000_000).to_i }   # long tail
cases << Array.new(200) { gen.rand(0..3) }
cases << [0, 0, 0, -5, 10**12, 3]
cases.each_with_index do |values, i|
  srand(1000 + i)
  d = TDigest::TDigest.new(0.01)
  values.each { |v| d.push(v) }
  d.compress!
  out["digests"] << { "seed" => 1000 + i, "values" => values, "bytes" => d.as_small_bytes.unpack1("H*"),
                      "p" => [0.5, 0.95, 0.99].map { |p| d.percentile(p).to_i }, "pf" => [0.5, 0.95, 0.99].map { |p| d.percentile(p).to_f } }
end
# absorb!: from_bytes (float means) + push more + compress!
srand(7)
base = TDigest::TDigest.new(0.01); (1..3000).each { |v| base.push(gen.rand(1..90_000)) }; base.compress!
bytes = base.as_small_bytes
more = Array.new(800) { gen.rand(1..120_000) }
srand(8)
d = TDigest::TDigest.from_bytes(bytes); more.each { |v| d.push(v) }; d.compress!
out["absorb"] << { "seed" => 8, "base" => bytes.unpack1("H*"), "values" => more, "bytes" => d.as_small_bytes.unpack1("H*"),
                   "p" => [0.5, 0.95, 0.99].map { |p| d.percentile(p).to_i } }
# merged_percentiles over stored digests (rollup rows)
rows = out["digests"].first(8).map { |h| Struct.new(:digest).new([h["bytes"]].pack("H*")) }
out["merged"] << { "digests" => rows.map { |r| r.digest.unpack1("H*") },
                   "p" => Railwatch::Telemetry::Rollup.merged_percentiles(rows, [0.5, 0.95, 0.99]) }
File.write("/tmp/tdigest_vectors.json", JSON.generate(out))
puts "ok #{out["digests"].size} digests"
