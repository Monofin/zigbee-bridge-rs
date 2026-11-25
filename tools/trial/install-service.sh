#!/bin/sh
# Run as the intended service user on the target host. Installs an idle trial.
set -eu
root=$(realpath "${1:?trial installation directory}")
release=$(realpath "${2:?directory containing trial.py and bin/zigbee-bridge}")
container=${3:-zigbee2mqtt}
user=$(id -un)
case "$root:$release:$container:$user" in *[!a-zA-Z0-9_./:-]*) echo 'unsupported service path or account name' >&2; exit 2;; esac
test -x "$release/bin/zigbee-bridge"
test -f "$release/trial.py"
test ! -e "$root/handover.json"
if systemctl is-active --quiet zigbee-bridge-trial.service; then
    echo 'stop the active trial before upgrading' >&2
    exit 1
fi
docker inspect "$container" >/dev/null
scratch=$(mktemp -d)
trap 'rm -f "$scratch/zigbee-bridge-trial.service" "$scratch/zigbee-bridge-recovery.service"; rmdir "$scratch"' EXIT
common="User=$user
Environment=BRIDGE_TRIAL_ROOT=$root BRIDGE_Z2M_CONTAINER=$container PYTHONPATH=$root/deps/paho/usr/lib/python3/dist-packages
UMask=0077
NoNewPrivileges=yes"
cat > "$scratch/zigbee-bridge-trial.service" <<EOF
[Unit]
Description=Supervised Zigbee bridge trial with Z2M rollback
After=docker.service network-online.target zigbee-bridge-recovery.service
Requires=docker.service
OnFailure=zigbee-bridge-recovery.service
[Service]
Type=notify
$common
ExecStart=/usr/bin/python3 $release/trial.py serve
ExecStopPost=/usr/bin/python3 $release/trial.py recover
TimeoutStartSec=180
TimeoutStopSec=180
KillMode=control-group
Restart=no
EOF
cat > "$scratch/zigbee-bridge-recovery.service" <<EOF
[Unit]
Description=Recover original Zigbee owner after an interrupted trial
After=docker.service network-online.target
Requires=docker.service
Wants=network-online.target
ConditionPathExists=$root/handover.json
[Service]
Type=oneshot
$common
ExecStart=/usr/bin/python3 $release/trial.py recover
TimeoutStartSec=180
Restart=on-failure
RestartSec=30
[Install]
WantedBy=multi-user.target
EOF
systemd-analyze verify "$scratch"/*.service
sudo install -m 0644 "$scratch"/*.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable zigbee-bridge-recovery.service
