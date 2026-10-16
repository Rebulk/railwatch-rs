#!/usr/bin/env bash
# Build railwatch-cloud @ 464f0fd from its production Dockerfile and run it
# locally the way production's web + job roles run (config/deploy.yml), minus
# kamal-proxy, litestream, SMTP, OAuth and self-monitoring.
#
#   bench/rails/up.sh            # build (cached) + start, wait for /up
#   SKIP_BUILD=1 bench/rails/up.sh
#
# Nothing in the railwatch-cloud repo is modified: the image is built from a
# detached worktree at /tmp/rw-baseline-wt (created if missing). No local
# tweaks are needed -- everything below is runtime env.
set -euo pipefail

REPO=${RAILWATCH_CLOUD_REPO:?set RAILWATCH_CLOUD_REPO to a railwatch-cloud checkout}
REV=464f0fd
WT=/tmp/rw-baseline-wt
IMAGE=railwatch-cloud-baseline:$REV
VOLUME=rw-baseline-storage
WEB=rw-baseline
JOBS=rw-baseline-jobs
PORT=${PORT:-8780}
HERE=$(cd "$(dirname "$0")" && pwd)
SECRETS=$HERE/.secrets.env   # throwaway keys, generated once, gitignored

if [ ! -e "$WT/.git" ]; then
  git -C "$REPO" worktree add --detach "$WT" "$REV"
fi
test "$(git -C "$WT" rev-parse --short=7 HEAD)" = "$REV" || { echo "worktree $WT is not at $REV" >&2; exit 1; }
test -z "$(git -C "$WT" status --porcelain)" || { echo "worktree $WT has local changes" >&2; exit 1; }

if [ -z "${SKIP_BUILD:-}" ]; then
  docker build -t "$IMAGE" "$WT"
fi

# Throwaway secrets. Production gets these from .kamal/secrets; we never had
# them and do not need them. RAILS_MASTER_KEY is deliberately absent: the app
# reads nothing from credentials except AR encryption keys, which ENV
# overrides (lib/railwatch_cloud/encryption_configuration.rb), and
# production.rb does not set require_master_key.
if [ ! -f "$SECRETS" ]; then
  umask 077
  {
    echo "SECRET_KEY_BASE=$(openssl rand -hex 64)"
    echo "ACTIVE_RECORD_ENCRYPTION_PRIMARY_KEY=$(openssl rand -hex 16)"
    echo "ACTIVE_RECORD_ENCRYPTION_DETERMINISTIC_KEY=$(openssl rand -hex 16)"
    echo "ACTIVE_RECORD_ENCRYPTION_KEY_DERIVATION_SALT=$(openssl rand -hex 16)"
  } > "$SECRETS"
fi

# Shared env for both roles (deploy.yml env.clear + secrets). Notes:
# - RAILWATCH_TOKEN unset => the gem's Configuration#enabled? is false, so
#   no self-monitoring reporter runs (config/initializers/railwatch.rb).
# - LITESTREAM_ENABLED unset and no litestream container. The image's
#   LD_PRELOAD=jemalloc and RAILS_ENV=production come from the Dockerfile.
# - RAILWATCH_ALLOWED_HOSTS: exact names only (no ports/wildcards); the Host
#   header's port is ignored by HostAuthorization.
# - assume_ssl=true in production.rb marks every request as HTTPS, so
#   force_ssl never redirects plain http; nothing to disable.
COMMON=(
  --env-file "$SECRETS"
  -e RAILS_MAX_THREADS=5
  -e RAILS_LOG_TO_STDOUT=true
  -e LITESTREAM_INSTALL_DIR=/usr/local/bin
  -e MAX_REQUEST_BODY=67108864
  -e RAILWATCH_ALLOWED_HOSTS=127.0.0.1,localhost
  -e WEB_CONCURRENCY=2
  -v "$VOLUME:/rails/storage"
)
# STORAGE=ram: storage on a tmpfs instead of the named volume, to measure the
# server rather than this workstation's (shared, busy) disk. Rust's matching
# switch is BENCH_DATA_ROOT=/dev/shm/... in bench/rust_run.sh.
if [ "${STORAGE:-disk}" = ram ]; then
  COMMON=(--env-file "$SECRETS" -e RAILS_MAX_THREADS=5 -e RAILS_LOG_TO_STDOUT=true
    -e LITESTREAM_INSTALL_DIR=/usr/local/bin -e MAX_REQUEST_BODY=67108864
    -e RAILWATCH_ALLOWED_HOSTS=127.0.0.1,localhost -e WEB_CONCURRENCY=2
    --mount "type=volume,dst=/rails/storage,volume-driver=local,volume-opt=type=tmpfs,volume-opt=device=tmpfs,volume-opt=o=size=8g,src=$VOLUME-ram")
fi

docker rm -f "$WEB" "$JOBS" >/dev/null 2>&1 || true
docker volume rm "$VOLUME-ram" >/dev/null 2>&1 || true  # tmpfs: always start empty
docker volume create "$VOLUME" >/dev/null

# Web role: the image's default CMD (./bin/thrust ./bin/rails server); the
# entrypoint runs db:prepare first. Thruster listens on 80 in the container.
docker run -d --name "$WEB" "${COMMON[@]}" -p "127.0.0.1:$PORT:80" "$IMAGE" >/dev/null

echo -n "waiting for http://127.0.0.1:$PORT/up "
for _ in $(seq 1 120); do
  if curl -fsS -o /dev/null "http://127.0.0.1:$PORT/up" 2>/dev/null; then echo " ok"; break; fi
  if [ "$(docker inspect -f '{{.State.Running}}' "$WEB")" != true ]; then
    echo; docker logs --tail 50 "$WEB"; exit 1
  fi
  echo -n .; sleep 1
done
curl -fsS -o /dev/null "http://127.0.0.1:$PORT/up" || { echo " timed out"; docker logs --tail 50 "$WEB"; exit 1; }

# Job role: same image and volume, cmd bin/jobs (deploy.yml servers.job).
# Started after web so db:prepare has created the queue database.
docker run -d --name "$JOBS" "${COMMON[@]}" "$IMAGE" bin/jobs >/dev/null
sleep 3
test "$(docker inspect -f '{{.State.Running}}' "$JOBS")" = true || { docker logs --tail 50 "$JOBS"; exit 1; }

echo "web:  $WEB  http://127.0.0.1:$PORT"
echo "jobs: $JOBS"
