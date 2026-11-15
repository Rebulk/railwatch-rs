# Sourced by the other scripts. Starts/stops either server inside rvr.slice
# (host.sh) with identical limits, and samples the slice.
#
# Rails: the railwatch-cloud production image (RAILS_IMAGE, built from the
# revision production last ran, 71020d3), the two roles production runs --
# web (Thruster + Puma, 2 workers x 5 threads) and job (bin/jobs, Solid Queue)
# -- as two containers in the slice, sharing one volume, the same env as
# bench/rails/up.sh. Rust: the railwatch binary from target/release in a
# debian:13 container (bind-mounted, nothing else in it). Both use host
# networking (no docker-proxy hop on either side) and both containers are
# pinned to SERVER_CPUS inside the slice, which also caps memory at 3 GiB
# for whatever runs in it (web + jobs together for Rails).

RVR_HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
RVR_ROOT=$(cd "$RVR_HERE/../.." && pwd)
RAILS_IMAGE=${RAILS_IMAGE:-railwatch-cloud-baseline:71020d3}
RUST_BIN=${RUST_BIN:-$RVR_ROOT/target/release/railwatch}
LOAD_BIN=${LOAD_BIN:-$RVR_ROOT/target/release/rw-load}
SERVER_CPUS=${SERVER_CPUS:-6-7}
LOAD_CPUS=${LOAD_CPUS:-2-5}
MNT=${BENCH_MNT:-/mnt/rvr}
# RVR_SLICE / RVR_SLICE_PATH: the systemd slice the servers run in and its
# cgroup directory (bench/scale/ uses bench-server.slice inside bench.slice).
RVR_SLICE=${RVR_SLICE:-rvr.slice}
SLICE=${RVR_SLICE_PATH:-/sys/fs/cgroup/$RVR_SLICE}
RAILS_PORT=8781   # Thruster's HTTP_PORT, host network
RUST_PORT=8795
SECRETS=$RVR_HERE/.secrets.env

rvr_secrets() {
  [ -f "$SECRETS" ] && return
  ( umask 077
    { echo "SECRET_KEY_BASE=$(openssl rand -hex 64)"
      echo "ACTIVE_RECORD_ENCRYPTION_PRIMARY_KEY=$(openssl rand -hex 16)"
      echo "ACTIVE_RECORD_ENCRYPTION_DETERMINISTIC_KEY=$(openssl rand -hex 16)"
      echo "ACTIVE_RECORD_ENCRYPTION_KEY_DERIVATION_SALT=$(openssl rand -hex 16)"
      echo "RAILWATCH_SECRET_KEY_BASE=$(openssl rand -hex 64)"; } > "$SECRETS" )
}

rvr_stop() { docker rm -f rvr-rust rvr-rails rvr-rails-jobs >/dev/null 2>&1 || true; }

# rvr_wait URL: until it answers (max 180 s)
rvr_wait() {
  for _ in $(seq 1 900); do curl -fsS -o /dev/null "$1" 2>/dev/null && return 0; sleep 0.2; done
  echo "timed out waiting for $1" >&2; return 1
}

# rvr_start_rails DATA_DIR: fresh web + jobs on DATA_DIR (bind mount at /rails/storage)
rvr_start_rails() {
  local data=$1; rvr_secrets; mkdir -p "$data"
  local common=(--cgroup-parent "$RVR_SLICE" --cpuset-cpus "$SERVER_CPUS" --network host --user 1000:1000
    --env-file "$SECRETS" -e RAILS_MAX_THREADS=5 -e WEB_CONCURRENCY=2 -e RAILS_LOG_TO_STDOUT=true
    -e LITESTREAM_INSTALL_DIR=/usr/local/bin -e MAX_REQUEST_BODY=67108864
    -e RAILWATCH_ALLOWED_HOSTS=127.0.0.1,localhost -e HTTP_PORT=$RAILS_PORT -e TARGET_PORT=3781
    -v "$data:/rails/storage")
  [ "${RAILS_ENV_LIMIT:-off}" = off ] && common+=(-v "$RVR_HERE/rails/zz_bench_no_environment_limit.rb:/rails/config/initializers/zz_bench_no_environment_limit.rb:ro")
  [ -n "${RAILS_EXTRA_MOUNTS:-}" ] && eval "common+=($RAILS_EXTRA_MOUNTS)"
  # RAILS_EXTRA_ENV: an extra --env-file (non-secret settings, e.g. a tuned
  # image's Puma/writer knobs) for both roles
  [ -n "${RAILS_EXTRA_ENV:-}" ] && common+=(--env-file "$RAILS_EXTRA_ENV")
  docker run -d --name rvr-rails "${common[@]}" "$RAILS_IMAGE" >/dev/null
  rvr_wait "http://127.0.0.1:$RAILS_PORT/up"
  docker run -d --name rvr-rails-jobs "${common[@]}" "$RAILS_IMAGE" bin/jobs >/dev/null
  sleep 3
  [ "$(docker inspect -f '{{.State.Running}}' rvr-rails-jobs)" = true ] || { docker logs --tail 30 rvr-rails-jobs; return 1; }
}

# rvr_seed_rails N OUT: N environments in the running Rails -> tokens file
rvr_seed_rails() {
  local n=$1 out=$2
  docker exec -i -e N="$n" rvr-rails bin/rails runner - <<'RUBY' 2>/dev/null | sed -n 's/^TOKEN //p' > "$out"
n = Integer(ENV.fetch("N"))
account = Account.find_or_initialize_by(slug: "bench")
account.update!(name: "bench", plan: "business", monthly_event_quota: 1_000_000_000_000)
app = account.applications.find_or_create_by!(slug: "bench") { |a| a.name = "bench" }
tokens = (1..n).map do |i|
  name = format("env-%04d", i)
  env = app.environments.find_by(name: name)
  env ? env.rotate_token! : app.environments.create!(name: name).token
end
missing = (1..n).map { |i| app.environments.find_by!(name: format("env-%04d", i)) }.reject(&:telemetry_exists?)
abort "telemetry tenant missing for #{missing.map(&:slug).join(", ")}" if missing.any?
puts tokens.map { |t| "TOKEN #{t}" }
RUBY
  [ "$(wc -l < "$out")" -eq "$n" ] || { echo "rails seed: expected $n tokens" >&2; return 1; }
}

# rvr_start_rust DATA_DIR N TOKENS_OUT: N environments, then serve
rvr_start_rust() {
  local data=$1 n=$2 out=$3; rvr_secrets; mkdir -p "$data"
  : > "$out"
  for i in $(seq 1 "$n"); do RAILWATCH_DATA_DIR=$data "$RUST_BIN" env create "e$i" | grep -o 'rw_[A-Za-z0-9]*' >> "$out"; done
  local skb; skb=$(sed -n 's/^RAILWATCH_SECRET_KEY_BASE=//p' "$SECRETS")
  docker run -d --name rvr-rust --cgroup-parent "$RVR_SLICE" --cpuset-cpus "$SERVER_CPUS" --network host --user 1000:1000 \
    -v "$RUST_BIN:/usr/local/bin/railwatch:ro" -v "$data:/data" -e RAILWATCH_DATA_DIR=/data -e PORT=$RUST_PORT \
    -e RAILWATCH_SECRET_KEY_BASE="$skb" -e RAILWATCH_TRUST_PROXY=1 -e RAILWATCH_RATE_LIMIT_ENV="${RUST_ENV_LIMIT:-0}" \
    ${RUST_EXTRA_ENV:-} debian:13 railwatch serve >/dev/null
  rvr_wait "http://127.0.0.1:$RUST_PORT/up"
}

# Slice counters. CPU in microseconds (user+system, every process in the
# slice: Rails web + jobs, or the one Rust process); memory in bytes.
rvr_cpu_us() { awk '/^usage_usec/{print $2}' "$SLICE/cpu.stat"; }
rvr_anon() { awk '/^anon /{print $2}' "$SLICE/memory.stat"; }
rvr_mem_current() { cat "$SLICE/memory.current"; }
# bytes written / write IOs to the bench disk by the slice (io.stat, dm device)
rvr_io() {
  local dev; dev=$(sudo dmsetup info -c --noheadings -o major,minor rvrdisk | tr -d ' ')
  awk -v d="$dev" '$1==d { for (i=2;i<=NF;i++) { split($i,a,"="); v[a[1]]=a[2] } print v["wbytes"]+0, v["wios"]+0 }' "$SLICE/io.stat" | head -1
}
rvr_load() { cut -d' ' -f1-3 /proc/loadavg; }
rvr_iopsi() { awk '/^full/{print $2}' /proc/pressure/io | cut -d= -f2; }
