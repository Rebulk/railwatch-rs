#!/usr/bin/env bash
# A local kamal-proxy edge for deploy/kamal-proxy/railwatch-swap proofs:
# a docker network, kamal-proxy v0.9.2 on
# 127.0.0.1:18443 with a self-signed certificate for the rehearsal names,
# and $LAB/opt holding the binary, env and swap.conf.
#
#   bench/swap/lab.sh up BIN_DIR     BIN_DIR has railwatch + rw-load built in rust:1-bookworm
#   bench/swap/lab.sh down           remove every lab container, volume and the opt dir
#
# Hostnames: railwatch.rehearsal and rs.rehearsal (certificate for both).
# Never touches anything not prefixed rwswap.
set -euo pipefail
LAB=${LAB:-/var/tmp/rwswap/lab}
NET=rwswap
PROXY=rwswap-proxy
CERTS=${CERTS:-/tmp/rwswap-tls}

down() {
  docker ps -aq --filter label=rwswap-lab | xargs -r docker rm -f > /dev/null
  docker ps -aq --filter label=service=railwatch_rs --filter network=$NET | xargs -r docker rm -f > /dev/null
  docker rm -f $PROXY > /dev/null 2>&1 || true
  docker volume rm -f rwswap_rs_data rwswap_rails_storage > /dev/null 2>&1 || true
  docker network rm $NET > /dev/null 2>&1 || true
  rm -rf "$LAB"
}

case "${1:-}" in
up)
  BIN=${2:?BIN_DIR}
  down
  # A CA and a leaf for the rehearsal names (webpki refuses a self-signed
  # CA:TRUE certificate as a server's own; the gem's Net::HTTP would too).
  [ -f "$CERTS/ca.pem" ] || {
    mkdir -p "$CERTS"
    openssl req -x509 -newkey rsa:2048 -nodes -keyout "$CERTS/ca.key" -out "$CERTS/ca.pem" -days 30 -subj /CN=rwswap-ca 2> /dev/null
    openssl req -newkey rsa:2048 -nodes -keyout "$CERTS/key.pem" -out "$CERTS/leaf.csr" -subj /CN=railwatch.rehearsal 2> /dev/null
    printf 'subjectAltName=DNS:railwatch.rehearsal,DNS:rs.rehearsal,DNS:rwswap-receiver\nbasicConstraints=CA:FALSE\nextendedKeyUsage=serverAuth\n' > "$CERTS/leaf.ext"
    openssl x509 -req -in "$CERTS/leaf.csr" -CA "$CERTS/ca.pem" -CAkey "$CERTS/ca.key" -CAcreateserial -days 30 -extfile "$CERTS/leaf.ext" -out "$CERTS/cert.pem" 2> /dev/null
    chmod 644 "$CERTS"/*.pem
  }
  docker image inspect rwswap-bookworm:amd64 > /dev/null 2>&1 || {
    docker pull -q --platform linux/amd64 debian:bookworm-slim > /dev/null && docker tag debian:bookworm-slim rwswap-bookworm:amd64
  }
  docker network create $NET > /dev/null
  docker volume create rwswap_rs_data > /dev/null
  # Owned by the server's uid (10001), as the container runs.
  docker run --rm -v rwswap_rs_data:/data rwswap-bookworm:amd64 chown 10001:10001 /data
  docker run -d --name $PROXY --label rwswap-lab=1 --network $NET -p 127.0.0.1:18080:80 -p 127.0.0.1:18443:443 \
    -v "$CERTS":/certs:ro basecamp/kamal-proxy:v0.9.2 > /dev/null
  mkdir -p "$LAB/opt/bin"
  cp "$BIN/railwatch" "$LAB/opt/bin/railwatch"
  cp "$BIN/rw-load" "$LAB/rw-load"
  cp "$CERTS/ca.pem" "$LAB/opt/bin/ca.pem"
  HERE=$(cd "$(dirname "$0")" && pwd)
  cp "$HERE/../../deploy/kamal-proxy/railwatch-swap" "$LAB/opt/"
  cat > "$LAB/opt/railwatch.env" <<EOF
RAILWATCH_DATA_DIR=/data
RAILWATCH_BIND=0.0.0.0:8790
RAILWATCH_URL=https://rs.rehearsal
RAILWATCH_TRUST_PROXY=1
RAILWATCH_ALERT_DELIVERY=off
RAILWATCH_RATE_LIMIT_IP=0
RAILWATCH_RATE_LIMIT_ENV=0
RAILWATCH_LOG=info
RAILWATCH_ALLOW_PRIVATE_ALERT_TARGETS=1
# The rehearsal's webhook receiver presents the lab certificate.
RAILWATCH_ALERT_CA_FILE=/opt/bin/ca.pem
EOF
  chmod 600 "$LAB/opt/railwatch.env"
  cat > "$LAB/opt/swap.conf" <<EOF
ROOT=$LAB/opt
ENV_FILE=$LAB/opt/railwatch.env
VOLUME=rwswap_rs_data
NETWORK=$NET
PROXY=$PROXY
NAME=rwswap-rs
SERVICES="railwatch_rs-web=rs.rehearsal"
TLS_ARGS="--tls-certificate-path /certs/cert.pem --tls-private-key-path /certs/key.pem"
RESOURCES="--memory 3g --memory-swap 3g --cpus 4 --pids-limit 512"
IMAGE=rwswap-bookworm:amd64
EOF
  echo "lab up: proxy https://127.0.0.1:18443, opt $LAB/opt"
  ;;
down) down; echo "lab down" ;;
*) echo "usage: lab.sh up BIN_DIR | down"; exit 2 ;;
esac
