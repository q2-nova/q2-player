#!/bin/sh
# Runs net.rs's wpa_supplicant test against a real wpa_supplicant (wired
# driver on lo) in a Debian container, sharing the control socket's folder.
# SELinux labelling is off for it: the daemon's replies cross into the host.
set -eu
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
# A short directory: the client socket path (ctrl/q2nd-wpa-PID-N) must fit
# in sun_path's 108 bytes, which a checkout's build/ can exceed.
DIR=$(mktemp -d /tmp/q2nd-wpa.XXXXXX)
mkdir "$DIR/ctrl"
# The same path inside: wpa_supplicant replies to the client's bound path.
printf 'ctrl_interface=%s/ctrl\nupdate_config=1\n' "$DIR" > "$DIR/wpa.conf"
podman rm -f q2nd-wpa > /dev/null 2>&1 || true
podman run -d --name q2nd-wpa --cap-add NET_ADMIN,NET_RAW --security-opt label=disable -v "$DIR:$DIR" -e DIR="$DIR" docker.io/library/debian:trixie-slim sh -c \
    'apt-get update -qq > /dev/null && apt-get install -y -qq wpasupplicant > /dev/null 2>&1 &&
     exec wpa_supplicant -Dwired -ilo -c"$DIR/wpa.conf"' > /dev/null
trap 'podman rm -f q2nd-wpa > /dev/null; rm -rf "$DIR"' EXIT
i=0
while [ ! -S "$DIR/ctrl/lo" ] && [ $i -lt 120 ]; do sleep 1; i=$((i + 1)); done
cd "$ROOT"
Q2ND_WPA_CTRL="$DIR/ctrl/lo" Q2ND_WPA_CONF="$DIR/wpa.conf" cargo test -p q2nd wpa_supplicant_round_trip -- --ignored
echo "--- saved configuration:"; cat "$DIR/wpa.conf"
