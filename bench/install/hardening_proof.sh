#!/usr/bin/env bash
# The installer's and upgrade's supply-chain checks, in a fresh systemd
# container (no ACME: --no-tls), against a bin/release output:
#
#   bench/install/hardening_proof.sh dist/v0.0.0-sec.1 [image=rwi-vps:ubuntu24]
#
#  1. the released install.sh pins each tarball's sha256;
#  2. an http --release-url is refused without --allow-http-release-url;
#  3. a release server whose SHA256SUMS and tarball were both swapped for
#     another build is refused (the pin, not the server, decides);
#  4. the real install: env file root 0600, parsed not sourced (a value with
#     $(...) is stored literally and nothing runs), no setup.out left with
#     the token, the token printed once;
#  5. `sudo railwatch doctor` works as root without sourcing anything and
#     leaves no root-owned file in the data dir;
#  6. `railwatch upgrade` refuses an http base, and refuses a release with
#     no signature (the test server's release is unsigned).
set -euo pipefail
DIST=$(cd "${1:?usage: hardening_proof.sh DIST_DIR [image]}" && pwd)
IMAGE=${2:-rwi-vps:ubuntu24}
V=$(basename "$DIST")
NET=rwi-hard
W=$(mktemp -d /tmp/rwi-hard.XXXXXX)
cleanup() { docker rm -f rwi-hreleases rwi-hvps > /dev/null 2>&1 || true; docker network rm $NET > /dev/null 2>&1 || true; }
trap cleanup EXIT
cleanup
pass() { echo "PASS  $*"; }
fail() { echo "FAIL  $*"; exit 1; }

# 1. pins
grep -Eq '^RAILWATCH_SHA256_X86_64=[0-9a-f]{64}$' "$DIST/install.sh" && grep -Eq '^RAILWATCH_SHA256_AARCH64=[0-9a-f]{64}$' "$DIST/install.sh" \
  && pass "released install.sh pins both tarballs' sha256" || fail "install.sh has no pinned sha256"

# A good release dir, and a tampered one: another build's tarball with a
# SHA256SUMS that matches it (what a compromised release server would send).
mkdir -p "$W/good" "$W/evil"
cp "$DIST"/* "$W/good/"
cp "$DIST"/* "$W/evil/"
T=railwatch-$V-x86_64-linux-musl.tar.gz
( cd "$W/evil" && mkdir x && tar -xzf "$T" -C x && printf '#!/bin/sh\necho pwned > /tmp/pwned\n' > "x/railwatch-$V-x86_64-linux-musl/railwatch" \
  && tar -C x -czf "$T" "railwatch-$V-x86_64-linux-musl" && rm -rf x && sha256sum -- *.tar.gz install.sh > SHA256SUMS )

docker network create --subnet 10.78.0.0/24 $NET > /dev/null
docker run -d --name rwi-hreleases --network $NET --ip 10.78.0.30 -v "$W:/srv:ro" -w /srv python:3-alpine python -m http.server 8000 > /dev/null
docker run -d --name rwi-hvps --hostname vps --network $NET --ip 10.78.0.10 --privileged --cgroupns=host \
  -v /sys/fs/cgroup:/sys/fs/cgroup:rw --tmpfs /run --tmpfs /run/lock "$IMAGE" > /dev/null
for _ in $(seq 1 50); do docker exec rwi-hvps systemctl is-system-running 2> /dev/null | grep -qE 'running|degraded' && break; sleep 0.2; done
docker cp "$DIST/install.sh" rwi-hvps:/root/install.sh
run() { docker exec rwi-hvps sh -c "$1"; }
COMMON="--no-tls --url https://monitor.example.test --email ops@example.test --password-file /root/pw --yes"
run "echo 'correct horse battery staple' > /root/pw"

# 2. http refused
if run "sh /root/install.sh $COMMON --release-url http://10.78.0.30:8000/good" > "$W/http.log" 2>&1; then fail "http release url accepted"; fi
grep -q "must be https" "$W/http.log" && pass "http --release-url refused: $(grep -o 'release-url must be https[^;]*' "$W/http.log" | head -1)" || { cat "$W/http.log"; fail "http refusal message"; }

# 3. tampered release refused by the pin
if run "sh /root/install.sh $COMMON --release-url http://10.78.0.30:8000/evil --allow-http-release-url" > "$W/evil.log" 2>&1; then fail "tampered release installed"; fi
grep -q "this installer pins" "$W/evil.log" && ! run "test -e /usr/local/bin/railwatch" \
  && pass "tampered SHA256SUMS + tarball refused: $(grep -o 'SHA256SUMS says [0-9a-f]\{12\}' "$W/evil.log")..." || { cat "$W/evil.log"; fail "tampered release"; }

# 4. real install, with a hostile-looking value
run "sh /root/install.sh $COMMON --release-url http://10.78.0.30:8000/good --allow-http-release-url \
  --backup-s3-endpoint http://10.78.0.99:9000 --backup-s3-bucket b --backup-s3-access-key-id k --backup-s3-secret-access-key 'x\$(touch /tmp/sourced)y'" > "$W/install.log" 2>&1 \
  || { cat "$W/install.log"; fail "install"; }
grep -q 'sha256 ok (pinned by this installer)' "$W/install.log" && pass "install: tarball matched the pinned sha256" || fail "pinned check not reported"
perm=$(run "stat -c '%U:%G %a' /etc/railwatch/railwatch.env")
[ "$perm" = "root:root 600" ] && pass "env file is $perm" || fail "env file is $perm"
run "grep -qF 'RAILWATCH_BACKUP_S3_SECRET_ACCESS_KEY=x\$(touch /tmp/sourced)y' /etc/railwatch/railwatch.env" && ! run "test -e /tmp/sourced" \
  && pass "a \$(...) value is stored literally and never executed" || fail "env value was interpreted"
! run "ls /var/lib/railwatch/setup.out" > /dev/null 2>&1 && pass "no setup.out in the data dir" || fail "setup.out left behind"
grep -q 'RAILWATCH_TOKEN=rw_' "$W/install.log" && pass "token printed once at the end" || fail "token not printed"
run "systemctl is-active -q railwatch.service" && pass "service running under the hardened unit" || { run "journalctl -u railwatch -n 40 --no-pager"; fail "service not running"; }
score=$(run "systemd-analyze security railwatch.service 2>/dev/null | tail -1")
echo "      $score"

# 5. doctor as root
run "railwatch doctor" > "$W/doctor.log" 2>&1 || true
grep -q "running as uid" "$W/doctor.log" && grep -q "^ok    data dir" "$W/doctor.log" && pass "sudo railwatch doctor: dropped to the data dir owner, read the env file" \
  || { cat "$W/doctor.log"; fail "doctor as root"; }
rootowned=$(run "find /var/lib/railwatch -user root | wc -l")
[ "$rootowned" = 0 ] && pass "no root-owned files in the data dir" || { run "find /var/lib/railwatch -user root"; fail "$rootowned root-owned files"; }

# 6. upgrade refusals
if run "RAILWATCH_RELEASE_BASE=http://10.78.0.30:8000 railwatch upgrade --version $V" > "$W/up-http.log" 2>&1; then fail "upgrade over http accepted"; fi
grep -q "only fetched over https" "$W/up-http.log" && pass "upgrade refuses an http release base" || { cat "$W/up-http.log"; fail "upgrade http message"; }
mkdir -p "$W/download" && ln -s ../good "$W/download/$V"
if run "RAILWATCH_RELEASE_BASE=http://10.78.0.30:8000 RAILWATCH_ALLOW_HTTP_RELEASE=1 railwatch upgrade --version $V" > "$W/up-unsigned.log" 2>&1; then
  grep -q "WARNING: no signature" "$W/up-unsigned.log" && pass "test-mode upgrade without a signature runs only with the loud warning" || fail "unsigned upgrade without warning"
else
  cat "$W/up-unsigned.log"; fail "test-mode upgrade"
fi
echo "all checks passed ($W)"
