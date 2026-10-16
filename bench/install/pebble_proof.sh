#!/usr/bin/env bash
# End-to-end proof of the one-command install with real ACME, no production:
#
#   bench/install/pebble_proof.sh dist/v0.2.0-rc.1 [image=rwi-vps:ubuntu24]
#
# On one docker network:
#   pebble         Let's Encrypt's ACME test server (ghcr.io/letsencrypt/pebble),
#                  validating against challtestsrv's DNS; 2-minute certificates
#                  (profile validity overridden) so renewal happens during the run
#   challtestsrv   DNS: monitor.example.test -> the VPS container
#   releases       python http.server serving the bin/release output, standing
#                  in for github.com/.../releases/download/<v>/
#   minio          S3-compatible backup target (bucket rw-backups)
#   vps            a fresh systemd container (Ubuntu 24.04 cloud-image packages)
#
# In the vps: `curl .../install.sh | sh -s -- --domain monitor.example.test
# --email ... --password-file ... --release-url ... --acme-directory pebble
# --acme-ca-file pebble.minica.pem`, timed. Then, from a client container
# that trusts Pebble's issuing root (fetched from Pebble's management API):
# https://monitor.example.test/up verifies, http:// redirects, an ingest
# post with the printed token is accepted; the serial changes after renewal
# with the service never restarted; `railwatch doctor` reports the cert.
set -euo pipefail
DIST=$(cd "${1:?usage: pebble_proof.sh DIST_DIR [image]}" && pwd)
IMAGE=${2:-rwi-vps:ubuntu24}
HERE=$(cd "$(dirname "$0")" && pwd)
NET=rwi-pebble
PEBBLE=ghcr.io/letsencrypt/pebble@sha256:ddf230642b1a584f519f32e347de1b05a6e4c1f6c35c1863b33effeab5f78199
CHALL=ghcr.io/letsencrypt/pebble-challtestsrv:latest
DOMAIN=monitor.example.test
W=$(mktemp -d /tmp/rwi-pebble.XXXXXX)
cleanup() { docker rm -f rwi-pebble rwi-chall rwi-releases rwi-vps rwi-client rwi-minio > /dev/null 2>&1 || true; docker network rm $NET > /dev/null 2>&1 || true; }
trap cleanup EXIT
cleanup
docker network create --subnet 10.77.0.0/24 $NET > /dev/null

# Pebble's own HTTPS (the ACME directory) is signed by this minica root.
id=$(docker create $PEBBLE); docker cp "$id:/test" "$W/pebble-test" > /dev/null; docker rm "$id" > /dev/null
python3 -I - "$W/pebble-test/config/pebble-config.json" > "$W/pebble-config.json" <<'EOF'
import json, sys
c = json.load(open(sys.argv[1]))
# 120 s certificates: rustls-acme renews at a third of validity left (80 s).
# Every profile: with none requested (rustls-acme asks for none), Pebble
# picks one of its profiles at random (a Go map), "shortlived" is 6 days.
for p in c["pebble"]["profiles"].values():
    p["validityPeriod"] = 120
c["pebble"]["tlsPort"] = 443   # TLS-ALPN-01 is validated on 443, as Let's Encrypt does
c["pebble"]["httpPort"] = 80
print(json.dumps(c))
EOF

docker run -d --name rwi-chall --network $NET --ip 10.77.0.53 $CHALL -defaultIPv4 10.77.0.10 -defaultIPv6 "" -http01 "" -https01 "" -tlsalpn01 "" -doh "" > /dev/null
docker run -d --name rwi-pebble --network $NET --ip 10.77.0.20 -e PEBBLE_VA_NOSLEEP=1 -e PEBBLE_WFE_NONCEREJECT=0 \
  -v "$W/pebble-config.json:/test/config/pebble-config.json:ro" $PEBBLE -config /test/config/pebble-config.json -dnsserver 10.77.0.53:8053 > /dev/null
docker run -d --name rwi-releases --network $NET --ip 10.77.0.30 -v "$DIST:/srv:ro" -w /srv python:3-alpine python -m http.server 8000 > /dev/null 2>&1 \
  || docker run -d --name rwi-releases --network $NET --ip 10.77.0.30 -v "$DIST:/srv:ro" -w /srv rwi-vps:ubuntu24 python3 -m http.server 8000 > /dev/null
docker run -d --name rwi-minio --network $NET --ip 10.77.0.40 -e MINIO_ROOT_USER=rwbackup -e MINIO_ROOT_PASSWORD=rwbackup-secret \
  pgsty/minio:latest server /data > /dev/null
until docker exec rwi-minio sh -c 'mc alias set l http://127.0.0.1:9000 rwbackup rwbackup-secret > /dev/null 2>&1 && mc mb -q --ignore-existing l/rw-backups'; do sleep 0.3; done
# The VPS: resolves the domain like the internet would (to itself), reaches
# pebble by name. --privileged + host cgroups: systemd as PID 1.
docker run -d --name rwi-vps --hostname vps --network $NET --ip 10.77.0.10 --privileged --cgroupns=host \
  -v /sys/fs/cgroup:/sys/fs/cgroup:rw --tmpfs /run --tmpfs /run/lock --add-host $DOMAIN:10.77.0.10 --add-host pebble:10.77.0.20 "$IMAGE" > /dev/null
for _ in $(seq 1 50); do docker exec rwi-vps systemctl is-system-running 2> /dev/null | grep -qE 'running|degraded' && break; sleep 0.2; done
docker cp "$W/pebble-test/certs/pebble.minica.pem" rwi-vps:/root/pebble.minica.pem
for _ in $(seq 1 50); do docker exec rwi-vps curl -fsS -o /dev/null --cacert /root/pebble.minica.pem https://pebble:14000/dir 2> /dev/null && break; sleep 0.2; done

echo "== install (fresh VPS, one command)"
t0=$(date +%s.%N)
docker exec rwi-vps sh -c "echo 'correct horse battery staple' > /root/pw; curl -fsSL http://10.77.0.30:8000/install.sh | sh -s -- \
  --domain $DOMAIN --email ops@example.test --password-file /root/pw --app Shop --env production \
  --release-url http://10.77.0.30:8000 --allow-http-release-url --acme-directory https://pebble:14000/dir --acme-ca-file /root/pebble.minica.pem \
  --backup-s3-endpoint http://10.77.0.40:9000 --backup-s3-bucket rw-backups --backup-s3-access-key-id rwbackup \
  --backup-s3-secret-access-key rwbackup-secret --backup-s3-prefix monitor/" \
  2>&1 | tee "$W/install.log" | sed 's/^/  | /'
t1=$(date +%s.%N)
INSTALL_S=$(python3 -c "print(f'{$t1 - $t0:.1f}')")
TOKEN=$(grep -o 'RAILWATCH_TOKEN=rw_[A-Za-z0-9]*' "$W/install.log" | cut -d= -f2)
[ -n "$TOKEN" ] || { echo "FAIL: no token printed"; exit 1; }

# A client that trusts Pebble's issuing root (it changes per Pebble start).
docker exec rwi-vps curl -fsS --cacert /root/pebble.minica.pem https://pebble:15000/roots/0 > "$W/issuing-root.pem"
docker run -d --name rwi-client --network $NET --add-host $DOMAIN:10.77.0.10 -v "$W/issuing-root.pem:/root.pem:ro" "$IMAGE" sleep infinity > /dev/null 2>&1 \
  || true
docker exec rwi-client true 2> /dev/null || { docker rm -f rwi-client > /dev/null; docker run -d --name rwi-client --network $NET --add-host $DOMAIN:10.77.0.10 -v "$W/issuing-root.pem:/root.pem:ro" --entrypoint sleep "$IMAGE" infinity > /dev/null; }
# curl verifies the chain against the ACME CA's root (fails otherwise);
# openssl prints what is being served.
cert() { docker exec rwi-client sh -c "curl -fsS -o /dev/null --cacert /root.pem https://$DOMAIN/up && echo verify=ok && echo | timeout 5 openssl s_client -connect $DOMAIN:443 -servername $DOMAIN 2>/dev/null | openssl x509 -noout -serial -enddate -issuer" ; }

echo "== verified HTTPS from a client trusting the ACME CA"
docker exec rwi-client sh -c 'command -v openssl > /dev/null || (apt-get update -qq && apt-get install -y -qq openssl > /dev/null 2>&1)' > /dev/null 2>&1 || true
first=$(cert); echo "$first" | sed 's/^/  /'
echo "== http -> https redirect"
docker exec rwi-client curl -s -o /dev/null -w '  %{http_code} -> %{redirect_url}\n' "http://$DOMAIN/settings?x=1"
echo "== ingest with the printed token over verified TLS"
# One request record from the gem's wire fixtures, stamped now, gzipped
# NDJSON as the gem posts it.
python3 -I -c '
import json, sys, time, uuid
r = json.load(open(sys.argv[1]))["request"]
r["timestamp"] = time.time(); r["execution_id"] = str(uuid.uuid4())
print(json.dumps(r))' "$HERE/../../crates/server/tests/fixtures/wire_fixtures.json" | gzip -n > "$W/batch.ndjson.gz"
docker cp "$W/batch.ndjson.gz" rwi-client:/batch.ndjson.gz
docker exec rwi-client curl -fsS --cacert /root.pem -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/x-ndjson' \
  -H 'Content-Encoding: gzip' --data-binary @/batch.ndjson.gz "https://$DOMAIN/ingest" -w '  %{http_code}\n' | sed 's/^/  /'
pid_before=$(docker exec rwi-vps systemctl show -p MainPID --value railwatch.service)

echo "== renewal: wait for a new certificate (120 s validity, renews at 80 s)"
serial1=$(echo "$first" | sed -n 's/^serial=//p')
renewed=""
for i in $(seq 1 30); do
  sleep 5
  now=$(cert 2> /dev/null || true)
  serial=$(echo "$now" | sed -n 's/^serial=//p')
  if [ -n "$serial" ] && [ "$serial" != "$serial1" ] && echo "$now" | grep -q verify=ok; then renewed="after $((i * 5)) s"; echo "$now" | sed 's/^/  /'; break; fi
done
pid_after=$(docker exec rwi-vps systemctl show -p MainPID --value railwatch.service)
docker exec rwi-vps journalctl -u railwatch --no-pager -o cat | grep -iE 'acme|certificate' | sed 's/\x1b\[[0-9;]*m//g' | tail -8 | sed 's/^/  log: /'
echo "== backups: one pass now, then what is in the bucket"
docker exec rwi-vps railwatch backup 2>&1 | sed 's/^/  /'
docker exec rwi-minio mc ls -r l/rw-backups | sed 's/^/  /'
echo "== re-run of the same command (idempotent: nothing changes, nothing restarts needlessly)"
docker exec rwi-vps sh -c "curl -fsSL http://10.77.0.30:8000/install.sh | sh -s -- --domain $DOMAIN --email ops@example.test \
  --release-url http://10.77.0.30:8000 --allow-http-release-url --acme-directory https://pebble:14000/dir --acme-ca-file /root/pebble.minica.pem" 2>&1 \
  | grep -E 'already installed|unchanged|kept|Upgraded|done in|answers' | sed 's/^/  /'
docker exec rwi-vps sh -c 'ls /var/lib/railwatch/acme | sed "s/^/  acme: /"; grep -c . /etc/railwatch/railwatch.env | sed "s/^/  env lines: /"'
echo "== doctor"
docker exec rwi-vps railwatch doctor 2>&1 | grep -E '^(ok|FAIL|warn)' | sed 's/^/  /' || true

echo
echo "install: ${INSTALL_S} s (download, checksum, user, units, setup, ACME issuance, verified /up)"
echo "renewal: ${renewed:-NOT OBSERVED}; service pid ${pid_before} -> ${pid_after} (same pid = renewed in place)"
[ -n "$renewed" ] && [ "$pid_before" = "$pid_after" ] && echo PASS
