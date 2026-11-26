# zigbee-bridge-rs

A Zigbee coordinator for Linux that connects Ember radios to MQTT 5 and Home Assistant. It publishes sensor readings and button actions, sends commands to devices, and manages pairing, bindings and local OTA updates over MQTT. Its Zigbee2MQTT-compatible topics and Home Assistant discovery identities support a staged migration from an existing installation.

The serial adapter must run Ember coordinator firmware speaking EZSP 13 or 19. Device support depends on the endpoints and clusters a device advertises and, for some products, a matching device profile. Check the [coverage guide](COVERAGE.md) before migrating devices you rely on.

## Deploy with Docker Compose

Use this method on Linux with Docker Engine, Compose v2, a commissioned Ember USB coordinator and a reachable MQTT 5 broker. Compose builds the image from the checked-out source. For a new radio, [form the network](REFERENCE.md#bench-coordinator) first. To take over a running Zigbee2MQTT installation, use the [supervised migration](INSTALL.md#side-by-side-trial-with-a-docker-based-z2m).

```sh
git clone https://github.com/Monofin/zigbee-bridge-rs.git
cd zigbee-bridge-rs
cp compose.env.example .env
```

Fill in `.env` with the source revision for the image tag (`git rev-parse --short HEAD`), the host user and group that own the state, the serial device and its group, and the radio’s existing network and serial settings. Set `STATE_DIR` and `CONFIG_DIR` to existing absolute directories outside the checkout. Put [broker.json](#broker-configuration) and any credential files in the config directory. The [installation guide](INSTALL.md#docker-compose) lists each setting.

```sh
docker compose config --quiet
docker compose build bridge
# Start after the previous coordinator has released the radio and state.
docker compose up -d --no-build bridge
docker compose logs --tail 50 -f bridge
```

Look for the verified coordinator and expected channel, then run the [MQTT health check](#use-it-from-mqtt-or-home-assistant). Press Ctrl-C to leave the log view. Use `docker compose stop bridge` for a graceful shutdown and `docker compose start bridge` to resume using the same state.

## Quickstart without hardware

On Linux, install a C compiler/linker, pkg-config, the OpenSSL development headers (`libssl-dev` on Debian), and a current stable Rust toolchain. The workspace has been tested with Rust 1.96. From a shell:

```sh
git clone https://github.com/Monofin/zigbee-bridge-rs.git
cd zigbee-bridge-rs
cargo build --locked --release --bins
cargo test --locked --test serial_pty
```

The test creates a pair of virtual serial ports and exercises the bridge’s serial worker. It needs neither a Zigbee adapter nor an MQTT broker. The build puts the `zigbee-bridge`, `zigbee-discovery` and `z2m_migrate` executables in `target/release/`.

To try the installed bridge offline, give it a synthetic interview with one endpoint:

```sh
sample=$(mktemp)
printf '%s\n' \
  '{"unix_ms":1,"event":"active_endpoints","sequence":1,"endpoints":[1]}' \
  '{"unix_ms":2,"event":"finished"}' > "$sample"
./target/release/zigbee-bridge report "$sample"
```

The JSON output should contain `"finished": true` and endpoint `1`.

## Connect a coordinator

For a **new bench network**, follow the [formation and interview commands](REFERENCE.md#bench-coordinator) on a spare adapter. For an **existing Zigbee2MQTT network**, start with the [migration guide](MIGRATION.md) and the [supervised handover](INSTALL.md#side-by-side-trial-with-a-docker-based-z2m). Only one process can own the serial port at a time. Starting the bridge resets the adapter session.

To resume a commissioned network, use its existing channel, PAN ID, extended PAN ID and state directory. With the shell variables set to your installation’s values, start the bridge in the foreground:

```sh
./target/release/zigbee-bridge listen "$port" "$state_dir" \
  "$channel" "$pan_hex" "$extended_pan_hex" \
  --baud "$baud" --flow "$flow" --mqtt-config "$broker_config"
```

Supply the PAN IDs as hexadecimal digits without a `0x` prefix. The extended PAN ID is 16 digits. The bridge checks the network identity and starts with joining closed. Use Ctrl-C for an orderly shutdown. The [installation guide](INSTALL.md) covers upgrades and rollback.

### Broker configuration

For a loopback broker that allows anonymous clients, save this as `broker.json` and set `broker_config` to its path. Use a distinct base topic and client ID for a bench network.

```json
{
  "host": "127.0.0.1",
  "port": 1883,
  "tls": false,
  "client_id": "zigbee-bench",
  "base_topic": "zigbee-bench",
  "discovery_prefix": "homeassistant"
}
```

For an authenticated broker, supply `username` and `password_file`. TLS defaults to enabled. Set `ca_file` for a private CA. Relative credential paths resolve from the configuration file’s directory. Grant publish access to the bridge’s administrative request topics only to clients that should control the network.

### Common options

| Option | Use |
| --- | --- |
| `--baud RATE`, `--flow none\|xonxoff\|rtscts` | Match the radio firmware and serial connection. Defaults are 115200 and software flow control. |
| `--mqtt-config FILE` | Connect to the broker described above. For a local lab broker, `--mqtt-local-port PORT` is an alternative. |
| `--device-report FILE` | Load an interview into the managed MQTT bridge. Repeat for multiple reports. |
| `--catalogue FILE` | Look up matching definitions in a separately built device catalogue when interviewing or inspecting a report. |
| `--standard-reports unmapped\|all` | Publish decoded standard attribute telemetry for unmapped endpoints (default), or include endpoints with native handlers. |
| `--ota-manifest FILE` | Serve explicitly selected local firmware images to eligible devices. See the [OTA guide](REFERENCE.md#ota-firmware). |

## Use it from MQTT or Home Assistant

For the example base topic, watch `zigbee-bench/bridge/state` and `zigbee-bench/bridge/info`. With admin API major version 1, subscribe to `zigbee-bench/bridge/v1/response/health_check` and then publish `{"transaction":"check-1"}` to `zigbee-bench/bridge/v1/request/health_check` with retain disabled. Match the transaction in the response. The [MQTT reference](REFERENCE.md#mqtt-administration-and-migration) covers pairing windows, device commands, groups, binding and firmware. The [Home Assistant dashboard](homeassistant/README.md) exposes the same controls.

A newly paired device starts with its IEEE address as its MQTT route. Use `device/display_name` to give it a readable name in Home Assistant. Use `device/rename_route` when you also want to move its MQTT topics. Both operations preserve its Home Assistant unique IDs. [Naming and readback examples](REFERENCE.md#device-naming).

The device topic contains its merged state for MQTT consumers. Home Assistant subscribes to separate feedback topics so a battery report or firmware check cannot make an old light reading appear fresh. If a light first reports brightness, the bridge reads its power state before publishing the light to Home Assistant.

## Libraries and contributions

The workspace includes reusable crates for [ZCL wire types and codecs](crates/zigbee-zcl), [device profiles and behaviour](crates/zigbee-devices), and [catalogue matching](crates/zigbee-catalogue). See [CONTRIBUTING.md](CONTRIBUTING.md) for testing, upstream provenance and how to submit a change. The design notes cover [coordinator shutdown](LIFECYCLE.md), [address resolution](ADDRESS-RESOLUTION.md) and [MQTT delivery and recovery](MQTT-DESIGN.md).

## Licence

Original code and documentation are available under **MIT OR Apache-2.0**, at your option. Third-party tables, profiles and dependencies retain their upstream terms. See the [MIT licence](LICENSE-MIT), [Apache licence](LICENSE-APACHE), [third-party notices](THIRD-PARTY.md) and [device-profile attributions](crates/zigbee-devices/profiles/LICENSING.md).
