# Installing and upgrading the bridge

## Install from a source revision

Use a clean checkout of the complete workspace on Linux with Cargo and Rust. The bridge manifest declares Rust 1.90. The release-profile binaries have been exercised on x86-64 and ARM64 Linux with Rust 1.96. Keep the broker configuration, its credential files and the coordinator state outside both the checkout and the installation directory.

From the repository root, choose a revision you have reviewed and build into its own prefix.

```sh
revision=$(git rev-parse HEAD)
prefix="$HOME/.local/opt/zigbee-bridge/$revision"
cargo install --locked --path . --root "$prefix" --bins
```

This installs `bin/zigbee-bridge`, `bin/z2m_migrate`, and the offline discovery renderer `bin/zigbee-discovery`. Use a distinct release identifier for a build with local changes. Use a separate Cargo target directory for each source revision if overriding `CARGO_TARGET_DIR`. The locked source build includes the device profiles. Optional external catalogues and OTA images are configured separately.

To resume a previously configured coordinator, give the listener its actual network identity and serial settings. Substitute the serial device, state directory, channel, PAN, extended PAN, baud, flow control and broker configuration from your installation.

```sh
"$prefix/bin/zigbee-bridge" listen "$port" "$state_dir" \
  "$channel" "$pan_hex" "$extended_pan_hex" \
  --baud "$baud" --flow "$flow" --mqtt-config "$broker_config"
```

Starting the listener resets the adapter session. Only one service can own the port. For an existing Zigbee2MQTT installation, stage a copy of its data and complete the [migration checks](MIGRATION.md) before handing over the adapter. Follow the [Home Assistant installation](homeassistant/README.md) for the dashboard.

## Docker Compose

Use this on a Linux host with Docker Engine, Compose v2, a commissioned Ember coordinator and a reachable MQTT 5 broker. A new adapter needs [network formation](REFERENCE.md#bench-coordinator) once. For a live Zigbee2MQTT cutover, use the [supervised handover below](#side-by-side-trial-with-a-docker-based-z2m), which also handles discovery and rollback.

1. Copy `compose.env.example` to `.env` beside `compose.yaml`. Set `BRIDGE_TAG` to the output of `git rev-parse --short HEAD` from a clean checkout. Each revision then has its own local image tag.
2. Set `SERIAL_DEVICE` to the adapter's stable `/dev/serial/by-id/…` path. Get `SERIAL_GID` with `stat -Lc %g /dev/serial/by-id/…`. Enter the commissioned channel, hexadecimal PAN and 16-digit extended PAN (omit the `0x` prefix). Match baud and flow control to the radio firmware. If resuming a deployment with custom child limit, transmit power or OTA settings, carry its existing listener options into a Compose command override.
3. Set `BRIDGE_UID` and `BRIDGE_GID` to the numeric IDs of the non-root host account that owns the state. Use existing, absolute, private directories for `CONFIG_DIR` and `STATE_DIR`, outside the checkout. The bridge needs write access to state and read access to config. For a fresh host store, create the state directory with mode 0700. For an established bridge, preserve the whole stopped store.
4. Put the [broker configuration](README.md#broker-configuration) in `CONFIG_DIR/broker.json`. Set its host to an address reachable **from the container**. A broker running on the Docker host can be reached at its LAN address if its listener and ACL permit it. Use relative paths for the password and private CA files beside the configuration. The config mount is read-only. Home Assistant connects to the same broker.

From the checkout, validate and build while the previous coordinator is still running. Hand over exclusive access to the radio and state before starting the service.

```sh
docker compose config --quiet
docker compose build bridge
docker compose up -d --no-build bridge
docker compose ps -a
docker compose logs --tail 50 -f bridge
```

The service runs the bridge directly as PID 1. `docker compose stop bridge` sends SIGTERM and allows up to 60 seconds for shutdown. `docker compose start bridge` starts the stopped container with its existing settings. `docker compose restart bridge` cycles the current process. After changing settings or the image tag, use `docker compose up -d --no-build bridge` to apply them. `docker compose down` removes the container and its Compose network. The host state and configuration directories persist.

The default restart policy retries a failing process up to three times. To start automatically after a Docker daemon restart, set `RESTART_POLICY=unless-stopped` after commissioning succeeds and reapply the service. Follow the [MQTT health check](README.md#use-it-from-mqtt-or-home-assistant) to verify readiness across the bridge and broker. `docker compose ps` alone checks the container process. Pairing and device administration use MQTT or the HA dashboard even when Compose owns the process.

For an upgrade, build and tag the candidate first, stop the running bridge, then take the [stopped-state backup below](#upgrade-and-rollback). To roll back, stop the candidate, select the previous image tag and its matching state directory in `.env`, and run `docker compose up -d --no-build bridge`. Confirm a fresh MQTT health reply. A source build includes the upstream crate archives and project notices under `/usr/share/doc/zigbee-bridge/`, including the exact covered source for `serialport`. See the [distribution notes](THIRD-PARTY.md) before redistributing an image.

## Side-by-side trial with a Docker-based Z2M

The two installations share the commissioned radio by taking turns. Staging reads Z2M's data and subscribes to retained MQTT discovery. The original process keeps the serial port throughout staging. A subsequent trial uses the original MQTT and discovery namespaces so existing HA clients follow the selected coordinator.

On the Linux host, use a service account with access to the serial device and Docker. Install Python 3, PyYAML, paho-mqtt 2.x, `fuser` and systemd. Choose a private directory separate from Z2M's data. Copy the three Linux binaries into the release's `bin/` and the operational helpers alongside it.

```sh
umask 077
root="$HOME/zigbee-bridge-trial"
release="$root/releases/$revision"
mkdir -p "$release/bin" "$root/runs"
install -m 0755 "$prefix/bin/zigbee-bridge" "$prefix/bin/zigbee-discovery" "$prefix/bin/z2m_migrate" "$release/bin/"
install -m 0644 tools/trial/*.py tools/devices/discovery_parity.py "$release/"
install -m 0755 tools/trial/install-service.sh "$release/"
BRIDGE_TRIAL_ROOT="$root" BRIDGE_Z2M_CONTAINER=zigbee2mqtt python3 "$release/trial.py" stage
"$release/install-service.sh" "$root" "$release" zigbee2mqtt
```

Review `standby/stage.json` for omissions and `standby/discovery-comparison.json` for missing identities and changed routes. The stage stores a private source snapshot, including the coordinator backup and broker credentials. Restrict access and apply the same retention policy as for Z2M backups. The discovery comparison checks device identifiers and command/state routes. The retained snapshot also preserves the original payload bytes for rollback.

**To give the radio to Rust**, run `sudo systemctl start zigbee-bridge-trial.service`. The service captures fresh discovery, stops Z2M, imports its final disk state, checks the port and starts the installed listener. systemd waits for a transaction-matched MQTT health reply. During a trial, bridge administration uses the v1 MQTT routes. Keep pairing closed and defer network membership, group, binding and OTA changes during an A/B comparison. Z2M cannot infer host-side changes made by its successor.

**To return the radio to Z2M**, run `sudo systemctl stop zigbee-bridge-trial.service`. Rollback allows up to 15 seconds for the exclusive discovery writer to restore the captured payloads and withdraw trial-owned additions. It joins that writer, returns the radio to Z2M and checks a fresh health reply. If retained readback remains outstanding, recovery keeps its marker and verifies the active owner’s discovery without writing to those topics. A failed trial start enters the same recovery path. The boot recovery unit runs if it finds an interrupted-handover marker. If recovery needs another attempt, inspect `journalctl -u zigbee-bridge-trial -u zigbee-bridge-recovery` and run `sudo systemctl start zigbee-bridge-recovery.service`. Leave the marker in place until recovery succeeds.

Zigbee2MQTT resumes with its own device database and friendly names. A device first paired during the trial, or a name changed through the Rust bridge, is saved in the Rust state directory. Keep that directory when reverting. To use a trial-only device under Zigbee2MQTT, check its inventory after rollback and pair it there if needed.

The trial wrapper supports a local Ember serial adapter, explicit network parameters, Z2M source profiles using MQTT 3.1.1 or 5, a single TLS or plaintext broker and optional user/password authentication. The Rust bridge requires an MQTT 5 broker. Home Assistant and other clients may continue using MQTT 3.1.1. The wrapper verifies the coordinator identity, channel, PAN and network key against the saved backup, imports RTS/CTS, baud, transmit power and the direct-child limit where configured, and checks the last two against the NCP at startup. Other deployment shapes can use the standalone importer and listener directly.

## Upgrade and rollback

1. Build the candidate into a new prefix while the current service is running. Record the previous executable path, arguments and configuration. Save the current HA dashboard files and settings if those will change.
2. Quiesce clients that issue administrative changes. Record this bridge's retained discovery topics and exact payloads. Capture a verified backup suitable for the adapter if the trial will change radio state.
3. Stop the old service gracefully and wait for it to exit. With the state directory unlocked, back up the **whole directory**, including member records, database files and required markers. Make a separate working copy for the candidate, preserving ownership and permissions. Keep broker credentials and any external catalogues or manifests available at their configured paths.
4. Start the candidate with the same adapter, network identity, MQTT base and discovery prefix. Confirm a fresh online announcement and closed pairing state. Check the advertised admin API major, send a health request with a fresh transaction and verify the reply. Check a known device's HA identity and a fresh device report before returning control to automations.
5. To roll back, stop and drain the candidate, preserve its state for diagnosis, then start the previous executable with its matching stopped backup and configuration. Reconcile device, radio and administrative changes made since that backup. Restoring host files cannot reverse actions already accepted by a device.
6. Restore the recorded discovery payloads and clear only candidate-added retained topics owned by this bridge. Verify from a new subscription. Restore any changed HA dashboard assets and confirm fresh health and device state again.

In the host-side rehearsal, the installed baseline and candidate each resumed the same simulated network with one staged member. Both returned a fresh MQTT health reply. The restored baseline recovered the same device identity and interview. Six candidate-only discovery topics remained at the broker until removed. Adapter recovery and a hardware upgrade on the intended target are separate checks.
