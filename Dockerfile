# The Railwatch server as a container image: the same static binary the
# release tarballs carry, and nothing else (no shell, no package manager).
#
# Built by .github/workflows/release.yml from the release's own tarballs,
# so the image runs byte-for-byte the binary SHA256SUMS lists. To build one
# yourself from a release directory (bin/release vX.Y.Z -> dist/vX.Y.Z):
#
#   docker build --build-arg VERSION=vX.Y.Z --build-context dist=dist/vX.Y.Z -t railwatch .
#
# Run it (docs/SELF-HOST.md, "Docker"):
#
#   docker run -d --name railwatch -p 127.0.0.1:8790:8790 -v railwatch:/data \
#     -e RAILWATCH_URL=https://monitor.example.com -e RAILWATCH_TRUST_PROXY=1 \
#     ghcr.io/rebulk/railwatch:latest
#   docker exec -it railwatch railwatch setup     # once: the admin, the app, the token
#
# Serves plain HTTP on :8790 for the proxy in front of it (Caddy, Traefik,
# kamal-proxy...). For HTTPS from the server itself, use install.sh on a VM.

FROM --platform=$BUILDPLATFORM alpine:3.22@sha256:49340b346798875b5592e5d420241f2f9cfba5f47e62ebb1446a1b34734e898f AS unpack
ARG VERSION
ARG TARGETARCH
RUN --mount=from=dist,target=/dist \
    case "$TARGETARCH" in amd64) arch=x86_64 ;; arm64) arch=aarch64 ;; *) echo "unsupported $TARGETARCH" >&2; exit 1 ;; esac \
 && name="railwatch-$VERSION-$arch-linux-musl" \
 && (cd /dist && grep " $name.tar.gz\$" SHA256SUMS | sha256sum -c -) \
 && tar -xzf "/dist/$name.tar.gz" -C /tmp \
 && mkdir -p /out/usr/local/bin /out/usr/share/doc/railwatch /out/data \
 && mv "/tmp/$name/railwatch" /out/usr/local/bin/railwatch \
 && mv "/tmp/$name/LICENSE" "/tmp/$name/THIRD-PARTY-LICENSES" "/tmp/$name/SELF-HOST.md" /out/usr/share/doc/railwatch/ \
 && chown 10001:10001 /out/data \
 && chmod 0700 /out/data

FROM scratch
ARG VERSION
COPY --from=unpack /out/ /
# No CA bundle: the binary carries its own roots (webpki-roots) for every
# outgoing HTTPS call (alerts, S3 backups, Google sign-in, uptime checks).
USER 10001:10001
ENV RAILWATCH_DATA_DIR=/data \
    RAILWATCH_BIND=0.0.0.0:8790 \
    RAILWATCH_URL=http://localhost:8790
VOLUME /data
EXPOSE 8790
STOPSIGNAL SIGTERM
LABEL org.opencontainers.image.title="Railwatch" \
      org.opencontainers.image.description="Railwatch server: errors, performance and uptime for Rails apps, self-hosted" \
      org.opencontainers.image.source="https://github.com/Rebulk/railwatch-rs" \
      org.opencontainers.image.licenses="FSL-1.1-ALv2" \
      org.opencontainers.image.version="$VERSION"
ENTRYPOINT ["/usr/local/bin/railwatch"]
CMD ["serve"]
