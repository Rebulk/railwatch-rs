#!/usr/bin/env bash
# End-to-end alert delivery proof: "my app went down" reaches a real HTTPS
# receiver with a verified HMAC signature, then "it is back" does too.
#
#   bench/alerts/silent_proof.sh [port=8799] [mode=ca|private]
#
# 1. A private CA and a 127.0.0.1 server certificate (openssl).
# 2. receiver.py on https://127.0.0.1:8797 verifies X-Railwatch-Signature.
# 3. `railwatch setup --alert-webhook https://127.0.0.1:8797/hook
#    --alert-webhook-secret ...` on a fresh data dir: the documented path.
#    mode=ca:      RAILWATCH_ALLOW_PRIVATE_ALERT_TARGETS=1 + RAILWATCH_ALERT_CA_FILE
#                  (TLS verified against the private CA).
#    mode=private: the same; there is no way to reach a loopback receiver
#                  without allow-private, so both modes need it. The CA file
#                  is what makes TLS verify instead of failing.
# 4. "The app": a health record from server web-1 every 5 s for 20 s, then
#    nothing (the app died), then beats again after the alert lands.
# 5. Reports when silent_environment and environment_recovered arrived,
#    relative to the last beat, and whether the signatures verified.
#
# Kills only the PIDs it started.
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
PORT=${1:-8799} RPORT=8797
BIN=$ROOT/target/release/railwatch
W=$ROOT/bench/scratch/gaps/proof
rm -rf "$W/data" "$W/received.jsonl"
mkdir -p "$W"
cd "$W"
if [ ! -f ca.pem ]; then
  openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -days 2 -subj "/CN=Railwatch proof CA" \
    -keyout ca.key -out ca.pem -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign" 2>/dev/null
  openssl req -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -subj "/CN=127.0.0.1" -keyout srv.key -out srv.csr 2>/dev/null
  printf "subjectAltName=IP:127.0.0.1\nbasicConstraints=CA:FALSE\nextendedKeyUsage=serverAuth\n" > ext
  openssl x509 -req -in srv.csr -CA ca.pem -CAkey ca.key -CAcreateserial -days 2 -out srv.pem -extfile ext 2>/dev/null
fi
SECRET=proof-$(date +%s)
pids=()
cleanup() { for p in "${pids[@]}"; do kill "$p" 2>/dev/null || true; done; }
trap cleanup EXIT

python3 -I "$HERE/receiver.py" $RPORT srv.pem srv.key "$SECRET" "$W/received.jsonl" > receiver.log 2>&1 &
pids+=($!)

export RAILWATCH_DATA_DIR=$W/data RAILWATCH_BIND=127.0.0.1:$PORT RAILWATCH_URL=http://127.0.0.1:$PORT
export RAILWATCH_ALLOW_PRIVATE_ALERT_TARGETS=1 RAILWATCH_ALERT_CA_FILE=$W/ca.pem RAILWATCH_SILENCE_SECONDS=30
echo 'correct horse battery staple' | "$BIN" setup --email ops@example.com --password-stdin --app Shop --env production \
  --alert-webhook "https://127.0.0.1:$RPORT/hook" --alert-webhook-secret "$SECRET" > setup.out
TOKEN=$(sed -n 's/^  RAILWATCH_TOKEN=//p' setup.out)
"$BIN" serve > serve.log 2>&1 &
pids+=($!)
until curl -sf "127.0.0.1:$PORT/up" > /dev/null; do sleep 0.1; done

beat() {
  python3 -I - "$PORT" "$TOKEN" <<'PY'
import gzip, json, sys, time, urllib.request
port, token = sys.argv[1], sys.argv[2]
rec = {"v": 1, "t": "health", "timestamp": time.time(), "deploy": "proof", "server": "web-1", "pid": 4242, "role": "web",
       "memory": 1, "threads_max": 5, "threads_busy": 1, "backlog": 0, "pool_size": 5, "pool_busy": 0, "pool_waiting": 0}
req = urllib.request.Request(f"http://127.0.0.1:{port}/ingest", data=gzip.compress((json.dumps(rec) + "\n").encode()), method="POST",
    headers={"Authorization": f"Bearer {token}", "Content-Type": "application/x-ndjson", "Content-Encoding": "gzip", "User-Agent": "railwatch-ruby/0.9.1"})
assert urllib.request.urlopen(req).status == 200
PY
}

for _ in 1 2 3 4 5; do beat; sleep 5; done
LAST=$(date +%s.%N)
echo "app stopped beating at $(date -u +%T)"
until grep -q silent_environment "$W/received.jsonl" 2>/dev/null; do
  sleep 1
  if (( $(date +%s) - ${LAST%.*} > 180 )); then echo "FAIL: no silent_environment within 180 s"; tail -20 serve.log; exit 1; fi
done
echo "app is back at $(date -u +%T)"
RESUMED=$(date +%s.%N)
for _ in 1 2 3 4 5 6 7 8; do beat; sleep 5; grep -q environment_recovered "$W/received.jsonl" && break; done
until grep -q environment_recovered "$W/received.jsonl" 2>/dev/null; do
  sleep 1
  if (( $(date +%s) - ${RESUMED%.*} > 120 )); then echo "FAIL: no environment_recovered within 120 s"; exit 1; fi
done

python3 -I - "$W/received.jsonl" "$LAST" "$RESUMED" <<'PY'
import json, sys
recs = [json.loads(l) for l in open(sys.argv[1])]
last, resumed = float(sys.argv[2]), float(sys.argv[3])
for r in recs:
    b = r["body"]
    ref = resumed if b["event"] == "environment_recovered" else last
    print(f'{b["event"]:22} +{r["received_at"] - ref:5.1f} s  signature_ok={r["signature_ok"]}  alert_id={b["alert_id"]}  url={b["url"]}')
    print(f'{"":22} payload.message: {b["payload"].get("message")}')
assert all(r["signature_ok"] for r in recs), "a signature did not verify"
assert [r["body"]["event"] for r in recs] == ["silent_environment", "environment_recovered"], [r["body"]["event"] for r in recs]
print("PASS: both alerts delivered over verified TLS with valid HMAC signatures")
PY
sqlite3 "$W/data/control.sqlite3" "SELECT id, event, status, delivery_attempts, sent_at FROM alerts ORDER BY id"
