#!/usr/bin/env bash
# Runs each release binary on several distributions (the static musl build
# needs nothing from the host): version, env create, doctor's control DB
# line, and serve + GET /up answering 200.
#
#   bench/install/matrix.sh dist/v0.2.0-rc.1
#
# aarch64 runs under qemu-user (binfmt) on an x86_64 host.
set -uo pipefail
DIST=$(cd "${1:?usage: matrix.sh DIST_DIR}" && pwd)
W=$(mktemp -d /tmp/rwi-matrix.XXXXXX); trap 'rm -rf "$W"' EXIT
IMAGES="alpine:3.22 debian:13-slim ubuntu:24.04 rockylinux/rockylinux:9-minimal"

# The probe, run inside each container by its own /bin/sh.
cat > "$W/probe.sh" <<'EOF'
set -e
[ "$(uname -m)" = "$ARCH" ] || { echo "wrong arch $(uname -m)"; exit 1; }
v=$(railwatch version)
railwatch env create t1 > /dev/null
railwatch doctor 2> /dev/null | grep -q "^ok    control DB" || { echo "doctor: control DB not ok"; exit 1; }
railwatch serve > /tmp/serve.log 2>&1 &
p=$!
# Listening on 127.0.0.1:8790: 0100007F:2256, state 0A, in /proc/net/tcp.
i=0
until grep -q "0100007F:2256 00000000:0000 0A" /proc/net/tcp; do
  i=$((i + 1)); [ $i -gt 100 ] && { echo "serve did not listen"; exit 1; }; sleep 0.1
done
if command -v curl > /dev/null; then curl -fsS -o /dev/null http://127.0.0.1:8790/up; up=curl
elif command -v wget > /dev/null; then wget -q -O /dev/null http://127.0.0.1:8790/up; up=wget
elif command -v bash > /dev/null; then
  status=$(bash -c 'exec 3<>/dev/tcp/127.0.0.1/8790; printf "GET /up HTTP/1.0\r\n\r\n" >&3; head -1 <&3')
  case "$status" in *" 200 "*) up=bash ;; *) echo "/up: $status"; exit 1 ;; esac
else echo "no way to fetch /up"; exit 1; fi
kill $p; wait $p 2> /dev/null || true
. /etc/os-release
echo "$v, $ID $VERSION_ID, /up 200 via $up"
EOF

printf '%-8s %-32s %-6s %s\n' arch image result detail
for arch in x86_64 aarch64; do
  tarball=$(ls "$DIST"/railwatch-*-"$arch"-linux-musl.tar.gz)
  mkdir -p "$W/$arch" && tar -xzf "$tarball" -C "$W/$arch" --strip-components=1
  platform=linux/amd64; [ "$arch" = aarch64 ] && platform=linux/arm64
  for image in $IMAGES; do
    out=$(docker run --rm --platform "$platform" -e ARCH="$arch" -e RAILWATCH_DATA_DIR=/tmp/d \
      -v "$W/$arch/railwatch:/usr/local/bin/railwatch:ro" -v "$W/probe.sh:/probe.sh:ro" "$image" sh /probe.sh 2>&1)
    rc=$?
    printf '%-8s %-32s %-6s %s\n' "$arch" "$image" "$([ $rc = 0 ] && echo ok || echo FAIL)" "$(echo "$out" | tail -1)"
  done
done
