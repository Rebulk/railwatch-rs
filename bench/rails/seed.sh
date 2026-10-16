#!/usr/bin/env bash
# Create N ingest environments in the running rw-baseline container and write
# their raw tokens to bench/rails/tokens-N.txt (one per line).
#
#   bench/rails/seed.sh 1
#   bench/rails/seed.sh 50
#
# One account "bench" on the business plan (250 M events/month quota, the
# highest plan; Account::PLANS), with monthly_event_quota raised further to
# 10^12 so no benchmark can reach Account#quota_exhausted? (402). One
# application "bench"; environments are named env-0001.. and get slugs
# bench-bench-env-0001.. (Environment#assign_slug). Tokens are minted by
# Environment#assign_token (rw_ + 44 base58, SHA-256 digest stored) and are
# only readable right after create, so re-running mints a fresh set: existing
# bench environments are kept and their tokens rotated.
#
# Rate limits are NOT touched. See README.md ("Rate limits").
set -euo pipefail
N=${1:?usage: seed.sh N}
HERE=$(cd "$(dirname "$0")" && pwd)
OUT=$HERE/tokens-$N.txt

docker exec -i -e N="$N" rw-baseline bin/rails runner - <<'RUBY' > "$OUT.raw"
n = Integer(ENV.fetch("N"))
account = Account.find_or_initialize_by(slug: "bench")
account.update!(name: "bench", plan: "business", monthly_event_quota: 1_000_000_000_000)
app = account.applications.find_or_create_by!(slug: "bench") { |a| a.name = "bench" }
tokens = (1..n).map do |i|
  name = format("env-%04d", i)
  env = app.environments.find_by(name: name)
  if env
    env.rotate_token!
  else
    env = app.environments.create!(name: name)
    env.token
  end
end
missing = (1..n).map { |i| app.environments.find_by!(name: format("env-%04d", i)) }.reject(&:telemetry_exists?)
abort "telemetry tenant missing for #{missing.map(&:slug).join(", ")}" if missing.any?
puts tokens.map { |t| "TOKEN #{t}" }
RUBY
# Rails prints an image_processing warning to stdout on boot; keep only ours.
sed -n 's/^TOKEN //p' "$OUT.raw" > "$OUT.tmp"; rm -f "$OUT.raw"

test "$(wc -l < "$OUT.tmp")" -eq "$N" || { echo "expected $N tokens, got $(wc -l < "$OUT.tmp")" >&2; cat "$OUT.tmp" >&2; exit 1; }
grep -qvE '^rw_[1-9A-HJ-NP-Za-km-z]{44}$' "$OUT.tmp" && { echo "malformed token line" >&2; exit 1; }
mv "$OUT.tmp" "$OUT"
echo "$N tokens -> $OUT"
