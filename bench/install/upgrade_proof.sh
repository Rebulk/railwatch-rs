#!/usr/bin/env bash
# Zero dropped ingest across `railwatch upgrade`, under load, over HTTPS:
#
#   bench/install/upgrade_proof.sh dist/v0.2.0-rc.1 dist/v0.2.0-rc.2 [rate=40] [swaps=4]
#
# Same rig as pebble_proof.sh (Pebble + DNS + release server + a fresh
# systemd VPS). Installs OLD with --domain (built-in TLS, Let's Encrypt via
# Pebble), then runs rw-load against https://DOMAIN/ingest at RATE batches/s
# of 500 records (open loop, from the VPS itself so the network is not the
# variable) while `railwatch upgrade` swaps OLD -> NEW -> OLD ... SWAPS times
# (RAILWATCH_RELEASE_BASE points upgrade at the local release server).
#
# Zero drops means: every post rw-load sent got a 200 (no connection error,
# no 5xx), and the server's ingest_batches row count equals the 200s.
set -euo pipefail
OLD=$(cd "${1:?usage: upgrade_proof.sh OLD_DIST NEW_DIST [rate] [swaps]}" && pwd)
NEW=$(cd "${2:?}" && pwd)
RATE=${3:-40} SWAPS=${4:-4}
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
LOAD=${RW_LOAD:-$ROOT/dist/tools/rw-load}   # static (musl) rw-load: runs in the VPS
[ -x "$LOAD" ] || { echo "build a static rw-load first (docs/SELF-HOST.md, 'Proofs')"; exit 1; }
OLD_V=$(basename "$OLD") NEW_V=$(basename "$NEW")
W=$(mktemp -d /tmp/rwi-upgrade.XXXXXX)
# The release server serves both versions as GitHub does: /download/<v>/...
mkdir -p "$W/releases/download"
ln -s "$OLD" "$W/releases/download/$OLD_V"; ln -s "$NEW" "$W/releases/download/$NEW_V"
cp -rL "$W/releases/download/." "$W/releases-real" 2> /dev/null || true
rm -rf "$W/releases"; mkdir -p "$W/releases"; mv "$W/releases-real" "$W/releases/download"

# Reuse pebble_proof's network setup by sourcing its first half would couple
# the scripts; the rig is small enough to repeat.
NET=rwi-pebble
PEBBLE=ghcr.io/letsencrypt/pebble@sha256:ddf230642b1a584f519f32e347de1b05a6e4c1f6c35c1863b33effeab5f78199
CHALL=ghcr.io/letsencrypt/pebble-challtestsrv:latest
DOMAIN=monitor.example.test
cleanup() { docker rm -f rwi-pebble rwi-chall rwi-releases rwi-vps > /dev/null 2>&1 || true; docker network rm $NET > /dev/null 2>&1 || true; }
trap cleanup EXIT
cleanup
docker network create --subnet 10.77.0.0/24 $NET > /dev/null
id=$(docker create $PEBBLE); docker cp "$id:/test" "$W/pebble-test" > /dev/null; docker rm "$id" > /dev/null
python3 -I -c '
import json, sys
c = json.load(open(sys.argv[1])); c["pebble"]["tlsPort"] = 443; c["pebble"]["httpPort"] = 80
print(json.dumps(c))' "$W/pebble-test/config/pebble-config.json" > "$W/pebble-config.json"
docker run -d --name rwi-chall --network $NET --ip 10.77.0.53 $CHALL -defaultIPv4 10.77.0.10 -defaultIPv6 "" -http01 "" -https01 "" -tlsalpn01 "" -doh "" > /dev/null
docker run -d --name rwi-pebble --network $NET --ip 10.77.0.20 -e PEBBLE_VA_NOSLEEP=1 -e PEBBLE_WFE_NONCEREJECT=0 \
  -v "$W/pebble-config.json:/test/config/pebble-config.json:ro" $PEBBLE -config /test/config/pebble-config.json -dnsserver 10.77.0.53:8053 > /dev/null
docker run -d --name rwi-releases --network $NET --ip 10.77.0.30 -v "$W/releases:/srv:ro" -w /srv python:3-alpine python -m http.server 8000 > /dev/null
docker run -d --name rwi-vps --hostname vps --network $NET --ip 10.77.0.10 --privileged --cgroupns=host \
  -v /sys/fs/cgroup:/sys/fs/cgroup:rw --tmpfs /run --tmpfs /run/lock --add-host $DOMAIN:10.77.0.10 --add-host pebble:10.77.0.20 \
  rwi-vps:ubuntu24 > /dev/null
for _ in $(seq 1 50); do docker exec rwi-vps systemctl is-system-running 2> /dev/null | grep -qE 'running|degraded' && break; sleep 0.2; done
docker cp "$W/pebble-test/certs/pebble.minica.pem" rwi-vps:/root/pebble.minica.pem
docker cp "$LOAD" rwi-vps:/usr/local/bin/rw-load
for _ in $(seq 1 50); do docker exec rwi-vps curl -fsS -o /dev/null --cacert /root/pebble.minica.pem https://pebble:14000/dir 2> /dev/null && break; sleep 0.2; done

echo "== install $OLD_V"
docker exec rwi-vps sh -c "echo 'correct horse battery staple' > /root/pw; curl -fsSL http://10.77.0.30:8000/download/$OLD_V/install.sh | sh -s -- \
  --domain $DOMAIN --email ops@example.test --password-file /root/pw --release-url http://10.77.0.30:8000/download/$OLD_V --allow-http-release-url \
  --acme-directory https://pebble:14000/dir --acme-ca-file /root/pebble.minica.pem" > "$W/install.log" 2>&1 || { cat "$W/install.log"; exit 1; }
grep -E 'done in|answers' "$W/install.log"
TOKEN=$(grep -o 'RAILWATCH_TOKEN=rw_[A-Za-z0-9]*' "$W/install.log" | cut -d= -f2)
docker exec rwi-vps sh -c "curl -fsS --cacert /root/pebble.minica.pem https://pebble:15000/roots/0 > /root/acme-root.pem"
# The gem's rate limits would 429 a load test; they are not what is measured.
docker exec rwi-vps sh -c "printf 'RAILWATCH_RATE_LIMIT_ENV=0\nRAILWATCH_RATE_LIMIT_IP=0\n' >> /etc/railwatch/railwatch.env && systemctl restart railwatch"
sleep 2

DURATION=$(( (SWAPS + 1) * 12 + 6 ))
echo "== load: $RATE batches/s x 500 records for ${DURATION} s over https while upgrading $SWAPS times"
docker exec -d rwi-vps sh -c "rw-load --url https://$DOMAIN --cacert /root/acme-root.pem --token $TOKEN --concurrency 32 --rate $RATE --duration $DURATION > /root/load.out 2>&1; touch /root/load.done"
sleep 6
for i in $(seq 1 "$SWAPS"); do
  if [ $((i % 2)) = 1 ]; then v=$NEW_V; else v=$OLD_V; fi
  t0=$(date +%s%N)
  docker exec -e RAILWATCH_RELEASE_BASE=http://10.77.0.30:8000 -e RAILWATCH_ALLOW_HTTP_RELEASE=1 rwi-vps railwatch upgrade --version "$v" > "$W/upgrade-$i.log" 2>&1 \
    || { cat "$W/upgrade-$i.log"; exit 1; }
  ms=$(( ($(date +%s%N) - t0) / 1000000 ))
  pid=$(docker exec rwi-vps systemctl show -p MainPID --value railwatch.service)
  echo "  swap $i -> $(docker exec rwi-vps railwatch version) in ${ms} ms (pid $pid): $(grep -E '^railwatch v|restart|running' "$W/upgrade-$i.log" | tr '\n' ' ' | cut -c1-110)"
  sleep 10
done
until docker exec rwi-vps test -f /root/load.done; do sleep 1; done
docker exec rwi-vps cat /root/load.out | grep -E 'posts in|batches/s|^ +(status|error|[0-9]{3}) ' | sed 's/^/  /'
oks=$(docker exec rwi-vps cat /root/load.out | awk '$1 == "200" {print $2}')
errors=$(docker exec rwi-vps cat /root/load.out | awk '$1 != "200" && $1 ~ /^(error|[0-9][0-9][0-9])$/ {n += $2} END {print n + 0}')
docker exec rwi-vps systemctl stop railwatch.service railwatch.socket
docker exec rwi-vps sh -c 'command -v sqlite3 > /dev/null || { apt-get update -qq && apt-get install -y -qq sqlite3; } > /dev/null 2>&1'
rows=$(docker exec rwi-vps sh -c 'n=0; for f in /var/lib/railwatch/telemetry/*/core/*.sqlite3 /var/lib/railwatch/telemetry/*.sqlite3; do
  [ -f "$f" ] || continue; c=$(sqlite3 "$f" "select count(*) from ingest_batches"); n=$((n + c)); done; echo $n')
echo
echo "upgrades: $SWAPS ($OLD_V <-> $NEW_V) under load over https"
echo "client: ${oks:-0} posts answered 200, $errors not (connection errors, 4xx, 5xx)"
echo "server: $rows ingest_batches rows"
[ "$errors" = 0 ] && [ "$rows" = "${oks:-x}" ] && echo PASS || { echo FAIL; exit 1; }
