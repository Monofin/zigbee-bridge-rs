#!/bin/sh
# Linux smoke test. The pseudo-terminal never answers the radio handshake.
set -eu
cd "$(dirname "$0")/../.."
umask 077
work=$(mktemp -d)
export CONFIG_DIR="$work/config" STATE_DIR="$work/state"
export BRIDGE_UID=$(id -u) BRIDGE_GID=$(id -g)
export SERIAL_DEVICE=/dev/ptmx SERIAL_GID=$(stat -Lc %g /dev/ptmx)
export ZIGBEE_CHANNEL=20 ZIGBEE_PAN=729c ZIGBEE_EXT_PAN=0807060504030201 SERIAL_FLOW=none
export BRIDGE_TAG=${BRIDGE_TAG:-compose-test} RESTART_POLICY=no
export COMPOSE_PROJECT_NAME="bridge-test-$$"
compose() { docker compose --env-file /dev/null -f compose.yaml "$@"; }
cleanup() {
    compose down >/dev/null
    rm -r "$work"
}
trap cleanup EXIT
mkdir "$STATE_DIR" "$CONFIG_DIR"
echo '{"host":"127.0.0.1","port":1883,"tls":false,"client_id":"fixture","base_topic":"fixture","discovery_prefix":"ha-fixture"}' > "$CONFIG_DIR/broker.json"
compose config --quiet
compose build bridge
compose run --rm --no-deps --entrypoint sh bridge -ec '
    test -r /usr/share/doc/zigbee-bridge/devices/LICENSE-MIT-cie-rgb
    test -r /etc/ssl/certs/ca-certificates.crt
    cd /usr/share/doc/zigbee-bridge
    set -- dependencies/serialport-*.crate
    test "$#" -eq 1 && test -s "$1"
'
for operation in up start; do
    if [ "$operation" = up ]; then compose up -d --no-build bridge; else compose start bridge; fi
    container=$(compose ps -q bridge)
    test -n "$container"
    # Wait until the listener opens the port. The lock file persists across stops.
    docker exec "$container" timeout 7 sh -c '
        while :; do
            for fd in /proc/1/fd/*; do
                if [ "$(readlink "$fd")" = /dev/zigbee ]; then exit 0; fi
            done
            sleep 0.1
        done
    '
    compose stop bridge
    test "$(docker inspect --format '{{.State.ExitCode}}' "$container")" = 0
    test -f "$STATE_DIR/bridge.lock"
done
