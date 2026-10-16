# A fresh Ubuntu 24.04 VPS as cloud images ship it (curl, ca-certificates,
# iproute2), with systemd as PID 1.
FROM jrei/systemd-ubuntu:24.04
RUN apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends curl ca-certificates iproute2 > /dev/null && rm -rf /var/lib/apt/lists/*
