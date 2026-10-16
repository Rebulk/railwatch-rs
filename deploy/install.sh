#!/bin/sh
# Railwatch (railwatch-rs) on a fresh Linux VPS, in one command:
#
#   curl -fsSL https://github.com/Rebulk/railwatch-rs/releases/latest/download/install.sh \
#     | sudo sh -s -- --domain monitor.example.com --email you@example.com
#
# DNS for the domain must point at this server and port 443 (and 80, for the
# http -> https redirect) must be reachable. Railwatch serves HTTPS itself and
# gets its certificate from Let's Encrypt (TLS-ALPN-01 on port 443); no proxy
# or other package is installed. Without --domain it listens on
# 127.0.0.1:8790 for a proxy you run (--no-tls).
#
# What it does (each step is skipped when already done; see docs/SELF-HOST.md):
#   1. downloads the static binary for this CPU from the release over https,
#      checks it against the sha256 baked into this script by bin/release
#      (and, with cosign installed, the release's signed SHA256SUMS), and
#      installs /usr/local/bin/railwatch
#   2. creates the `railwatch` system user, /var/lib/railwatch (data, 0700)
#      and /etc/railwatch/railwatch.env (config, 0600, read by the binary
#      itself: KEY=VALUE data, never sourced by a shell)
#   3. installs railwatch.socket + railwatch.service: systemd holds the
#      listening sockets, so restarts and upgrades refuse no connection
#   4. first run: `railwatch setup` (admin, account, app, environment, alerts)
#   5. starts it, waits for HTTPS, runs `railwatch doctor`
#
# Re-running is the upgrade (also: `sudo railwatch upgrade`): the new binary
# is swapped in and the service restarted while systemd keeps the sockets
# open; config, data and certificates are kept. Flags given on a re-run
# update the config file (e.g. adding --backup-s3-* later).
#
# Options:
#   --domain NAME          public hostname: built-in HTTPS with Let's Encrypt
#   --email ADDR           admin account email; also the Let's Encrypt contact
#   --no-tls               plain HTTP on 127.0.0.1:8790 for your own proxy
#   --url URL              public URL with --no-tls (default http://127.0.0.1:8790)
#   --password-file PATH   read the admin password from PATH (non-interactive;
#                          under `curl | sh` stdin is the script, so use this)
#   --password-stdin       read it from stdin (when the script is run as a file)
#   --app NAME --env NAME  first application and environment (My app / production)
#   --alert-webhook URL [--alert-webhook-secret S] | --alert-slack URL | --alert-email ADDR
#                          where "your app stopped reporting" and new-issue alerts go
#   --backup-s3-endpoint URL --backup-s3-bucket B --backup-s3-access-key-id K
#   --backup-s3-secret-access-key S [--backup-s3-region R] [--backup-s3-prefix P]
#                          S3-compatible backups (AWS, R2, B2, MinIO), every 5 min
#   --version vX.Y.Z       install this release (default: the one this script came from)
#   --binary PATH          install this local binary instead of downloading
#   --release-url URL      download from URL/<file> instead of GitHub (a mirror, or tests); https only
#   --allow-http-release-url   tests only: accept an http --release-url
#   --acme-directory URL   ACME CA other than Let's Encrypt (staging, Pebble, step-ca)
#   --acme-ca-file PATH    trust this CA for the ACME directory itself (Pebble)
#   --memory-max SIZE      systemd MemoryMax for the service (default 2G)
#   --allow-private-alert-targets   alert destinations on private/loopback addresses
#   --upgrade              only swap the binary and restart (what `railwatch upgrade` runs)
#   --yes                  never prompt
set -eu

RAILWATCH_VERSION_DEFAULT=latest
RAILWATCH_REPO_DEFAULT=Rebulk/railwatch-rs
# bin/release replaces these with the sha256 of this release's tarballs, so a
# script fetched over https from the release pins its own binaries. Empty in
# the repository copy (then SHA256SUMS, checked with cosign when present).
RAILWATCH_SHA256_X86_64=
RAILWATCH_SHA256_AARCH64=

DOMAIN="" EMAIL="" URL="" NO_TLS=0 APP="" ENVNAME="" PASSWORD_STDIN=0 PASSWORD_FILE="" ALLOW_PRIVATE=0 MEMORY_MAX=""
VERSION=$RAILWATCH_VERSION_DEFAULT BINARY="" RELEASE_URL="" ACME_DIRECTORY="" ACME_CA_FILE="" UPGRADE_ONLY=0 YES=0 ALLOW_HTTP=0
ALERT_ARGS="" S3_ENDPOINT="" S3_BUCKET="" S3_KEY="" S3_SECRET="" S3_REGION="" S3_PREFIX=""
BIN=/usr/local/bin/railwatch ETC=/etc/railwatch ENVFILE=/etc/railwatch/railwatch.env DATA=/var/lib/railwatch
UNITS=/etc/systemd/system

die() { echo "install.sh: $*" >&2; exit 1; }
step() { printf '\n==> %s\n' "$*"; }
# Single-quoted for a shell word (alert args are passed through `eval set`).
q() { printf "'%s'" "$(printf '%s' "$1" | sed "s/'/'\\\\''/g")"; }
need() { [ $# -ge 2 ] && [ -n "$2" ] || die "$1 needs a value"; }
started=$(date +%s)

while [ $# -gt 0 ]; do
  case "$1" in
    --domain) need "$@"; DOMAIN=$2; shift 2 ;;
    --email) need "$@"; EMAIL=$2; shift 2 ;;
    --url) need "$@"; URL=$2; shift 2 ;;
    --no-tls) NO_TLS=1; shift ;;
    --app) need "$@"; APP=$2; shift 2 ;;
    --env) need "$@"; ENVNAME=$2; shift 2 ;;
    --password-stdin) PASSWORD_STDIN=1; shift ;;
    --password-file) need "$@"; PASSWORD_FILE=$2; shift 2 ;;
    --alert-webhook|--alert-webhook-secret|--alert-slack|--alert-email) need "$@"; ALERT_ARGS="$ALERT_ARGS $1 $(q "$2")"; shift 2 ;;
    --backup-s3-endpoint) need "$@"; S3_ENDPOINT=$2; shift 2 ;;
    --backup-s3-bucket) need "$@"; S3_BUCKET=$2; shift 2 ;;
    --backup-s3-access-key-id) need "$@"; S3_KEY=$2; shift 2 ;;
    --backup-s3-secret-access-key) need "$@"; S3_SECRET=$2; shift 2 ;;
    --backup-s3-region) need "$@"; S3_REGION=$2; shift 2 ;;
    --backup-s3-prefix) need "$@"; S3_PREFIX=$2; shift 2 ;;
    --version) need "$@"; VERSION=$2; shift 2 ;;
    --binary) need "$@"; BINARY=$2; shift 2 ;;
    --release-url) need "$@"; RELEASE_URL=${2%/}; shift 2 ;;
    --allow-http-release-url) ALLOW_HTTP=1; shift ;;
    --acme-directory) need "$@"; ACME_DIRECTORY=$2; shift 2 ;;
    --acme-ca-file) need "$@"; ACME_CA_FILE=$2; shift 2 ;;
    --memory-max) need "$@"; MEMORY_MAX=$2; shift 2 ;;
    --allow-private-alert-targets) ALLOW_PRIVATE=1; shift ;;
    --upgrade) UPGRADE_ONLY=1; shift ;;
    --yes|-y) YES=1; shift ;;
    -h|--help) sed -n '2,58p' "$0" 2>/dev/null | sed 's/^# \{0,1\}//' || echo "see docs/SELF-HOST.md"; exit 0 ;;
    *) die "unknown option $1 (--help)" ;;
  esac
done

[ "$(id -u)" = 0 ] || die "run as root: curl -fsSL .../install.sh | sudo sh -s -- --domain ..."
command -v systemctl > /dev/null && [ -d /run/systemd/system ] || die "needs systemd (the service and its socket are systemd units)"
FIRST_RUN=1; [ -f "$DATA/control.sqlite3" ] && FIRST_RUN=0
if [ "$UPGRADE_ONLY" = 1 ] && [ "$FIRST_RUN" = 1 ]; then die "--upgrade: nothing installed yet in $DATA"; fi
if [ "$FIRST_RUN" = 1 ] && [ "$UPGRADE_ONLY" = 0 ] && [ -z "$DOMAIN" ] && [ "$NO_TLS" = 0 ]; then
  die "pass --domain monitor.example.com (HTTPS with Let's Encrypt) or --no-tls (behind your own proxy)"
fi
[ -n "$DOMAIN" ] && [ "$NO_TLS" = 1 ] && die "--domain and --no-tls are exclusive"
case "$RELEASE_URL" in
  ""|https://*) ;;
  *) [ "$ALLOW_HTTP" = 1 ] || die "--release-url must be https (--allow-http-release-url is for tests)" ;;
esac
# Values go into a KEY=VALUE file the binary parses one line at a time.
for v in "$DOMAIN" "$EMAIL" "$URL" "$ACME_DIRECTORY" "$MEMORY_MAX" "$S3_ENDPOINT" "$S3_BUCKET" "$S3_KEY" "$S3_SECRET" "$S3_REGION" "$S3_PREFIX"; do
  case "$v" in *"
"*|*"$(printf '\r')"*) die "option values cannot contain line breaks" ;; esac
done
case "$MEMORY_MAX" in ""|[0-9]*[0-9KMGT%]|infinity) ;; *) die "--memory-max: expected a size like 2G" ;; esac
if [ -n "$S3_ENDPOINT$S3_BUCKET$S3_KEY$S3_SECRET" ] && { [ -z "$S3_ENDPOINT" ] || [ -z "$S3_BUCKET" ] || [ -z "$S3_KEY" ] || [ -z "$S3_SECRET" ]; }; then
  die "backups need all of --backup-s3-endpoint, --backup-s3-bucket, --backup-s3-access-key-id, --backup-s3-secret-access-key"
fi

# Fetch with curl or wget, whichever this box has: https only (TLS 1.2+,
# redirects stay on https) unless --allow-http-release-url (tests).
fetch() {
  if command -v curl > /dev/null; then
    if [ "$ALLOW_HTTP" = 1 ]; then curl -fsSL --retry 3 -o "$2" "$1"
    else curl -fsSL --retry 3 --proto '=https' --proto-redir '=https' --tlsv1.2 -o "$2" "$1"; fi
  elif command -v wget > /dev/null; then
    if [ "$ALLOW_HTTP" = 1 ]; then wget -q -O "$2" "$1"
    else wget -q --https-only --secure-protocol=TLSv1_2 -O "$2" "$1"; fi
  else die "needs curl or wget"; fi
}
sha256() {
  if command -v sha256sum > /dev/null; then sha256sum "$1" | cut -d' ' -f1
  else openssl dgst -sha256 -r "$1" | cut -d' ' -f1; fi
}

# ---------------------------------------------------------------------------
step "binary"
case "$(uname -m)" in
  x86_64|amd64) ARCH=x86_64 ;;
  aarch64|arm64) ARCH=aarch64 ;;
  *) die "no release for $(uname -m) (x86_64 and aarch64 are built); build from source: docs/SELF-HOST.md" ;;
esac
TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT INT TERM
if [ -n "$BINARY" ]; then
  [ -f "$BINARY" ] || die "$BINARY: no such file"
  cp "$BINARY" "$TMP/railwatch"
  echo "using $BINARY (no checksum: a local file)"
else
  if [ -z "$RELEASE_URL" ]; then
    if [ "$VERSION" = latest ]; then RELEASE_URL="https://github.com/$RAILWATCH_REPO_DEFAULT/releases/latest/download"
    else RELEASE_URL="https://github.com/$RAILWATCH_REPO_DEFAULT/releases/download/$VERSION"; fi
  fi
  fetch "$RELEASE_URL/SHA256SUMS" "$TMP/SHA256SUMS" || die "cannot download $RELEASE_URL/SHA256SUMS"
  TARBALL=$(grep -o "railwatch-v[^ ]*-$ARCH-linux-musl.tar.gz" "$TMP/SHA256SUMS" | head -1)
  [ -n "$TARBALL" ] || die "$RELEASE_URL/SHA256SUMS lists no $ARCH build"
  # Trust, strongest first: the sha256 bin/release baked into this script
  # (it came over https from the release); the release's SHA256SUMS signed
  # by its workflow (cosign); SHA256SUMS alone only for an unbaked copy of
  # this script with no cosign, said out loud.
  case "$ARCH" in x86_64) pinned=$RAILWATCH_SHA256_X86_64 ;; aarch64) pinned=$RAILWATCH_SHA256_AARCH64 ;; esac
  listed=$(grep " $TARBALL\$" "$TMP/SHA256SUMS" | cut -d' ' -f1)
  if [ -n "$pinned" ] && [ "$listed" != "$pinned" ]; then
    die "$RELEASE_URL/SHA256SUMS says $listed for $TARBALL, this installer pins $pinned; not installing"
  fi
  if command -v cosign > /dev/null && fetch "$RELEASE_URL/SHA256SUMS.sigstore.json" "$TMP/SHA256SUMS.sigstore.json" 2> /dev/null; then
    tag=${TARBALL#railwatch-}; tag=${tag%-"$ARCH"-linux-musl.tar.gz}
    cosign verify-blob "$TMP/SHA256SUMS" --bundle "$TMP/SHA256SUMS.sigstore.json" \
      --certificate-identity "https://github.com/$RAILWATCH_REPO_DEFAULT/.github/workflows/release.yml@refs/tags/$tag" \
      --certificate-oidc-issuer https://token.actions.githubusercontent.com \
      --certificate-github-workflow-trigger push --certificate-github-workflow-repository "$RAILWATCH_REPO_DEFAULT" > /dev/null 2>&1 \
      || die "SHA256SUMS signature does not verify for $RAILWATCH_REPO_DEFAULT $tag; not installing"
    echo "SHA256SUMS: signature verified ($tag)"
  elif [ -z "$pinned" ]; then
    echo "warning: this install.sh has no pinned checksums and cosign is not installed: trusting $RELEASE_URL/SHA256SUMS as served"
  fi
  fetch "$RELEASE_URL/$TARBALL" "$TMP/$TARBALL" || die "cannot download $RELEASE_URL/$TARBALL"
  got=$(sha256 "$TMP/$TARBALL")
  [ "$listed" = "$got" ] || die "$TARBALL: sha256 $got does not match SHA256SUMS ($listed); not installing"
  echo "$TARBALL: sha256 ok${pinned:+ (pinned by this installer)}"
  tar -xzf "$TMP/$TARBALL" -C "$TMP"
  mv "$TMP/${TARBALL%.tar.gz}/railwatch" "$TMP/railwatch"
fi
chmod 0755 "$TMP/railwatch"
"$TMP/railwatch" version > /dev/null 2>&1 || die "the downloaded binary does not run on this machine ($(uname -m))"
NEW_VERSION=$("$TMP/railwatch" version)
UPGRADE=0
if [ -f "$BIN" ] && cmp -s "$TMP/railwatch" "$BIN"; then
  echo "$NEW_VERSION already installed"
else
  [ -f "$BIN" ] && { UPGRADE=1; echo "$("$BIN" version 2>/dev/null || echo 'previous build') -> $NEW_VERSION"; }
  install -m 0755 "$TMP/railwatch" "$BIN.new"
  mv -f "$BIN.new" "$BIN"   # atomic: the running process keeps its old inode
  echo "installed $NEW_VERSION at $BIN"
fi

if [ "$UPGRADE_ONLY" = 1 ]; then
  step "restart (sockets held by systemd: no connection is refused)"
  if [ "$UPGRADE" = 1 ]; then systemctl restart railwatch.service; fi
  for _ in $(seq 1 100); do systemctl is-active -q railwatch.service && break; sleep 0.1; done
  systemctl is-active -q railwatch.service || { journalctl -u railwatch -n 30 --no-pager; die "railwatch did not come back"; }
  echo "$NEW_VERSION running ($(( $(date +%s) - started )) s)"
  exit 0
fi

# ---------------------------------------------------------------------------
step "user, directories, config"
if ! id railwatch > /dev/null 2>&1; then
  NOLOGIN=$(command -v nologin || echo /sbin/nologin)
  if command -v useradd > /dev/null; then useradd --system --home-dir "$DATA" --shell "$NOLOGIN" railwatch
  else adduser -S -D -H -h "$DATA" -s "$NOLOGIN" railwatch; fi   # busybox (Alpine with systemd is rare, but)
fi
install -d -o railwatch -g railwatch -m 0700 "$DATA"
install -d -o root -g root -m 0755 "$ETC"
umask 077
[ -f "$ENVFILE" ] || {
  cat > "$ENVFILE" <<'EOF'
# railwatch-rs configuration: every key is RAILWATCH_<KEY>; `railwatch doctor`
# lists them all with where each value came from. Restart after editing:
#   systemctl restart railwatch     (no connection is refused: the socket is held)
# KEY=VALUE per line, read as data (systemd and the railwatch binary agree on
# this subset): no quotes needed, no $expansion, no shell.
#
# Email (password resets, invitations, email alerts):
# RAILWATCH_SMTP_ADDRESS=smtp.example.com
# RAILWATCH_SMTP_PORT=587
# RAILWATCH_SMTP_USERNAME=
# RAILWATCH_SMTP_PASSWORD=
# RAILWATCH_SMTP_FROM=Railwatch <railwatch@example.com>
# External dead-man switch (healthchecks.io etc.): pinged every 30 s while healthy.
# RAILWATCH_DEADMAN_URL=
EOF
  echo "wrote $ENVFILE"
}
# setting KEY VALUE: set (or replace) one line of the env file. Values were
# checked for line breaks above; the file is parsed, never sourced.
setting() {
  grep -v "^$1=" "$ENVFILE" > "$ENVFILE.tmp" || true
  printf '%s=%s\n' "$1" "$2" >> "$ENVFILE.tmp"
  mv -f "$ENVFILE.tmp" "$ENVFILE"
  case "$1" in *SECRET*|*PASSWORD*) echo "  $1=(set)" ;; *) echo "  $1=$2" ;; esac
}
unset_setting() { grep -v "^$1=" "$ENVFILE" > "$ENVFILE.tmp" || true; mv -f "$ENVFILE.tmp" "$ENVFILE"; }
setting RAILWATCH_DATA_DIR "$DATA"
if [ -n "$DOMAIN" ]; then
  setting RAILWATCH_TLS_DOMAIN "$DOMAIN"
  unset_setting RAILWATCH_URL; unset_setting RAILWATCH_TRUST_PROXY
  [ -n "$EMAIL" ] && setting RAILWATCH_TLS_EMAIL "$EMAIL"
  [ -n "$ACME_DIRECTORY" ] && setting RAILWATCH_TLS_ACME_DIRECTORY "$ACME_DIRECTORY"
  if [ -n "$ACME_CA_FILE" ]; then
    install -m 0644 "$ACME_CA_FILE" "$ETC/acme-ca.pem"
    setting RAILWATCH_TLS_ACME_CA_FILE "$ETC/acme-ca.pem"
  fi
elif [ "$NO_TLS" = 1 ]; then
  unset_setting RAILWATCH_TLS_DOMAIN; unset_setting RAILWATCH_TLS_EMAIL
  setting RAILWATCH_URL "${URL:-http://127.0.0.1:8790}"
  case "${URL:-}" in https://*) setting RAILWATCH_TRUST_PROXY 1 ;; esac
fi
[ "$ALLOW_PRIVATE" = 1 ] && setting RAILWATCH_ALLOW_PRIVATE_ALERT_TARGETS 1
if [ -n "$S3_ENDPOINT" ]; then
  setting RAILWATCH_BACKUP_S3_ENDPOINT "$S3_ENDPOINT"
  setting RAILWATCH_BACKUP_S3_BUCKET "$S3_BUCKET"
  setting RAILWATCH_BACKUP_S3_ACCESS_KEY_ID "$S3_KEY"
  setting RAILWATCH_BACKUP_S3_SECRET_ACCESS_KEY "$S3_SECRET"
  [ -n "$S3_REGION" ] && setting RAILWATCH_BACKUP_S3_REGION "$S3_REGION"
  [ -n "$S3_PREFIX" ] && setting RAILWATCH_BACKUP_S3_PREFIX "$S3_PREFIX"
fi
chown root:root "$ENVFILE"; chmod 0600 "$ENVFILE"
umask 022
TLS_ON=0; grep -q '^RAILWATCH_TLS_DOMAIN=' "$ENVFILE" && TLS_ON=1

# ---------------------------------------------------------------------------
step "systemd units"
UNITS_CHANGED=0
# The socket unit: 443 + 80 for built-in HTTPS, loopback for a proxy.
if [ "$TLS_ON" = 1 ]; then LISTEN="ListenStream=0.0.0.0:443
ListenStream=0.0.0.0:80"; else LISTEN="ListenStream=127.0.0.1:8790"; fi
cat > "$TMP/railwatch.socket" <<EOF
# Installed by install.sh. Holds railwatch's listening sockets across
# restarts and upgrades: connections queue in the kernel, none is refused.
[Unit]
Description=Railwatch listening sockets

[Socket]
$LISTEN
Backlog=4096
NoDelay=true

[Install]
WantedBy=sockets.target
EOF
cat > "$TMP/railwatch.service" <<'EOF'
# Installed by install.sh. Configuration: /etc/railwatch/railwatch.env.
[Unit]
Description=Railwatch (railwatch-rs)
Documentation=https://github.com/Rebulk/railwatch-rs/blob/main/docs/SELF-HOST.md
Requires=railwatch.socket
After=railwatch.socket network-online.target
Wants=network-online.target

[Service]
Type=simple
User=railwatch
Group=railwatch
# Read by systemd as root before dropping to User=, so the file stays 0600 root.
EnvironmentFile=/etc/railwatch/railwatch.env
Environment=RAILWATCH_DATA_DIR=/var/lib/railwatch
ExecStart=/usr/local/bin/railwatch serve
# SIGTERM: stop accepting, finish in-flight requests, commit queued writes.
KillSignal=SIGTERM
TimeoutStopSec=60
Restart=always
RestartSec=1
StateDirectory=railwatch
StateDirectoryMode=0700
MemoryMax=2G
LimitNOFILE=65536
# The service needs its state dir and the network, nothing else. Ports
# 80/443 come from railwatch.socket, so no CAP_NET_BIND_SERVICE either.
# `systemd-analyze security railwatch.service`: 1.1 (was 3.4); what is left
# is the network it serves and the host root directory.
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true
PrivateDevices=true
ProtectKernelTunables=true
ProtectKernelModules=true
ProtectControlGroups=true
RestrictSUIDSGID=true
LockPersonality=true
RestrictNamespaces=true
RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX
SystemCallArchitectures=native
SystemCallFilter=@system-service
SystemCallFilter=~@privileged @resources
SystemCallErrorNumber=EPERM
CapabilityBoundingSet=
AmbientCapabilities=
ProtectClock=true
ProtectKernelLogs=true
ProtectHostname=true
ProtectProc=invisible
ProcSubset=pid
RestrictRealtime=true
RemoveIPC=true
PrivateUsers=true
MemoryDenyWriteExecute=true
DevicePolicy=closed
UMask=0077

[Install]
WantedBy=multi-user.target
EOF
SOCKET_CHANGED=0
for f in railwatch.socket railwatch.service; do
  if ! cmp -s "$TMP/$f" "$UNITS/$f"; then
    install -m 0644 "$TMP/$f" "$UNITS/$f"; UNITS_CHANGED=1
    [ "$f" = railwatch.socket ] && SOCKET_CHANGED=1
  fi
done
if [ -n "$MEMORY_MAX" ]; then
  install -d "$UNITS/railwatch.service.d"
  printf '[Service]\nMemoryMax=%s\n' "$MEMORY_MAX" > "$TMP/memory.conf"
  cmp -s "$TMP/memory.conf" "$UNITS/railwatch.service.d/memory.conf" || { install -m 0644 "$TMP/memory.conf" "$UNITS/railwatch.service.d/memory.conf"; UNITS_CHANGED=1; }
fi
if [ "$UNITS_CHANGED" = 1 ]; then systemctl daemon-reload; echo "installed and reloaded"; else echo "unchanged"; fi
if [ "$TLS_ON" = 1 ]; then
  for port in 80 443; do
    if ss -Hltn "sport = :$port" 2>/dev/null | grep -q . && ! systemctl is-active -q railwatch.socket; then
      die "port $port is already in use ($(ss -Hltnp "sport = :$port" | sed 's/.*users:((\("[^"]*"\).*/\1/' | head -1)); stop that server first"
    fi
  done
fi

# ---------------------------------------------------------------------------
if [ "$FIRST_RUN" = 1 ]; then
  step "first-time setup (admin, account, application, environment, alerts)"
  [ -n "$ALERT_ARGS" ] || echo "note: no --alert-webhook/--alert-slack/--alert-email: nothing will alert you when the app goes down (add one in Settings > Integrations)."
  set -- setup
  [ -n "$EMAIL" ] && set -- "$@" --email "$EMAIL"
  [ -n "$APP" ] && set -- "$@" --app "$APP"
  [ -n "$ENVNAME" ] && set -- "$@" --env "$ENVNAME"
  { [ "$PASSWORD_STDIN" = 1 ] || [ -n "$PASSWORD_FILE" ]; } && set -- "$@" --password-stdin
  eval "set -- \"\$@\" $ALERT_ARGS"
  # As the service user with the service's config, so the files setup
  # creates (control DB, secret_key_base) are the service's. Where the
  # password comes from: the file, stdin, or (interactive) the terminal;
  # under `curl | sh` stdin is the script itself, so prompts read /dev/tty.
  if [ -n "$PASSWORD_FILE" ]; then
    [ -r "$PASSWORD_FILE" ] || die "--password-file $PASSWORD_FILE: not readable"
    exec 3< "$PASSWORD_FILE"
  elif [ "$PASSWORD_STDIN" = 0 ] && [ ! -t 0 ] && [ -r /dev/tty ] && [ "$YES" = 0 ]; then exec 3< /dev/tty
  else exec 3<&0; fi
  # The binary reads the env file itself (as data) and, run as root,
  # becomes the owner of the data dir before touching it.
  ( cd / && RAILWATCH_ENV_FILE="$ENVFILE" "$BIN" "$@" <&3 ) > "$TMP/setup.out" \
    || { cat "$TMP/setup.out"; die "railwatch setup failed"; }
  exec 3<&-
fi

# ---------------------------------------------------------------------------
step "start"
systemctl enable -q railwatch.socket railwatch.service
if [ "$SOCKET_CHANGED" = 1 ] && systemctl is-active -q railwatch.socket; then
  # Different ports: the old sockets must go (the one restart that can refuse).
  systemctl stop railwatch.service railwatch.socket
fi
systemctl start railwatch.socket
if [ "$UPGRADE" = 1 ] || [ "$UNITS_CHANGED" = 1 ] || [ "$FIRST_RUN" = 0 ]; then
  systemctl restart railwatch.service
  [ "$UPGRADE" = 1 ] && echo "restarted on $NEW_VERSION (sockets held: no connection refused)"
else
  systemctl start railwatch.service
fi
if [ "$TLS_ON" = 1 ]; then
  HOST=$(sed -n 's/^RAILWATCH_TLS_DOMAIN=//p' "$ENVFILE")
  echo "waiting for the certificate for $HOST (first start: Let's Encrypt, usually under 30 s)"
  # Verified against the system's CAs; with --acme-ca-file (a test CA the
  # system does not trust) the check is that a certificate was issued and
  # is served, unverified.
  ok=0
  for _ in $(seq 1 90); do
    if [ -z "$ACME_CA_FILE" ]; then
      curl -fsS -o /dev/null --max-time 5 --resolve "$HOST:443:127.0.0.1" "https://$HOST/up" 2> /dev/null && { ok=1; break; }
    elif ls "$DATA"/acme/cached_cert_* > /dev/null 2>&1; then
      curl -kfsS -o /dev/null --max-time 5 --resolve "$HOST:443:127.0.0.1" "https://$HOST/up" 2> /dev/null && { ok=1; break; }
    fi
    sleep 1
  done
  if [ "$ok" = 1 ]; then echo "https://$HOST answers with its certificate"
  else echo "warning: https://$HOST is not answering with a certificate yet; journalctl -u railwatch shows the ACME log (DNS? port 443 reachable from the internet?)"; fi
else
  for _ in $(seq 1 50); do curl -fsS -o /dev/null http://127.0.0.1:8790/up 2> /dev/null && break; sleep 0.2; done
  curl -fsS -o /dev/null http://127.0.0.1:8790/up || { journalctl -u railwatch -n 30 --no-pager; die "railwatch did not answer on 127.0.0.1:8790"; }
  echo "railwatch answers on 127.0.0.1:8790"
fi

step "doctor"
( cd / && RAILWATCH_ENV_FILE="$ENVFILE" "$BIN" doctor 2> /dev/null ) | grep -E '^(ok|FAIL|warn)' || true

step "done in $(( $(date +%s) - started )) s"
if [ "$FIRST_RUN" = 1 ]; then
  grep -E '^Railwatch is set up' "$TMP/setup.out" || true
  echo "Give the Rails app these. The ingest token is shown ONCE and stored nowhere in plain text:"
  echo "copy it now (a new one: \`sudo railwatch env create <name>\`, or Settings in the dashboard)."
  grep -E '^  RAILWATCH_' "$TMP/setup.out" || true
else
  echo "Upgraded/updated. Tokens, data and certificates are unchanged."
fi
