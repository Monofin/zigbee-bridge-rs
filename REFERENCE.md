# Bridge reference

## MQTT administration and migration

Install the [Home Assistant dashboard](homeassistant/README.md) for pairing, device details, firmware and bindings. The bridge owns the radio and publishes the confirmed pairing state and numeric `api_version` in retained `BASE/bridge/info`.

The admin API is at major version **1**. Subscribe to `BASE/bridge/v1/response/OPERATION` before publishing to `BASE/bridge/v1/request/OPERATION`. Gate writes on `BASE/bridge/state` being online and the advertised major being supported. New optional fields and operations can be added within v1. Incompatible changes require a new major topic. Unknown major topics are ignored. The original `bridge/request` and `bridge/response` routes remain available for Zigbee2MQTT clients. Device state, set/get topics and discovery identities keep their existing formats. Supply a `transaction` value to match its reply. Administrative commands are non-retained. Only grant administrative publish rights to trusted broker clients.

| Operation | Example payload | Confirmation |
|---|---|---|
| `device/describe` | `{"id":"lamp","endpoint":1,"cluster":6,"transaction":"inspect-1"}` | Reads the current member's saved interview and identifies catalogue candidates for that server. Omit cluster to see its actual descriptor. Omit endpoint to list endpoint IDs. |
| `health_check` | `{}` | Adapter reports joined |
| `permit_join` | `{"time":60,"device":"Coordinator"}` | Coordinator admission open, with a deadline. Omit `device` to include routers. Send `{"time":0}` to close |
| `device/command` | `{"id":"lamp","endpoint":1,"command":{"op":"scene_store","group":3,"scene":2},"transaction":"scene-1"}` | Operates on one interviewed Groups, Scenes or Alarms server and returns its confirmed result. |
| `device/lock` (v1) | `{"id":"front-door","endpoint":1,"action":"lock","transaction":"door-42"}` | Commands a credentialed endpoint and reads back its lock state. Requires a transaction ID. |
| `device/bind`, `device/unbind` | `{"from":"switch","to":"Coordinator","clusters":["genOnOff"],"transaction":"bind-1"}` | ZDO status from the source endpoint for each cluster. Numeric group destinations are supported. Reporting configuration is a separate operation |
| `device/ota_update/check` | `{"id":"lamp"}` | Compatibility with its last observed query and the local image catalogue |
| `device/ota_update/schedule`, `device/ota_update/unschedule` | `{"id":"lamp"}` | Per-device authorization saved or cancelled |
| `device/ota_update/update` | `{"id":"lamp","transaction":"ota-1"}` | Sends Image Notify and waits for the device to report its new running version |

For an administrative change, choose a unique transaction ID and keep it when retrying the same request. The bridge records the operation before dispatch and keeps its final response for seven days. Retry on the same base, API namespace and operation to receive that response. Reusing its ID with different arguments returns an error. If shutdown interrupted an operation before its result was recorded, the bridge reports an indeterminate outcome. Inspect the device or network before using a fresh ID. An ingress refusal carrying `"retry":"same_transaction"` means the client can resend the same ID to obtain its outcome when capacity returns. Read-only queries always read the current state. Mutations without an ID execute on each delivery.

Publish endpoint operations to `BASE/bridge/v1/request/device/command`. Groups support `group_list`, `group_view`, `group_add` and `group_remove`. Scenes support `scene_list`, `scene_view`, `scene_store`, `scene_recall` and `scene_remove`. Store captures the endpoint’s current scene settings. Alarms support `alarm_pop`, `alarm_reset`, `alarm_reset_all` and `alarm_clear_log`. Reading the oldest entry with `alarm_pop` removes it on the device, so use a transaction ID for retries. A timeout leaves the device outcome unknown.

For a credentialed door lock, opt in with `--lock-credentials DIR` on the managed listener. Use a private directory outside coordinator state (mode 0700). Give each authorised endpoint a regular, owner-only file named `<16 lowercase hex IEEE digits>-<decimal endpoint>.pin`. It contains the lock’s numeric PIN, with an optional final newline. The bridge opens the file for each actuation, so removing it revokes access. Rediscovery on startup or a fresh interview updates the Home Assistant lock entity.

Publish `{"id":"front-door","endpoint":1,"action":"lock","transaction":"door-42"}` to `BASE/bridge/v1/request/device/lock`. The same route accepts `unlock`. Restrict publish access to operators and automations authorised to operate those locks. The request and journal contain the action and transaction ID. The PIN travels only in the APS-protected command. The coordinator checks the peer-specific key table and reads the lock’s current security policy before sending it. After acknowledgement it reads LockState and returns `lock_state` and `matches_request`. Locks configured for install-code-derived APS security still require a commissioning path that can establish that key’s provenance. [ZCL R8 §7.3](https://csa-iot.org/developer-resource/specifications-download-request/) · [EZSP key-table lookup](https://docs.silabs.com/zigbee/8.2.3/sisdk-ezsp-reference-guide/09-security-frames)

Use the coordinator’s `listen` mode with `--mqtt-config FILE`. The JSON bootstrap takes `host`, `port`, `client_id`, `base_topic` and `discovery_prefix`. TLS is enabled by default. An optional `ca_file` selects a private CA. Supply `username` and `password_file` together for authentication. For UART transport use `--flow none|xonxoff|rtscts` and `--baud RATE` to match the adapter image. Relative credential paths resolve beside the bootstrap. The bench shorthand `--mqtt-local-port PORT` uses loopback.

### OTA firmware

Supply `--ota-manifest FILE`. Its `images` array lists `id`, `file` (a local basename), the publisher’s HTTPS `source` and the independently trusted `sha256` for each original file. The server checks the Zigbee header, hardware constraints and digest. The client checks the image according to its firmware policy. Compatible clients receive an offer only with per-device authorization. A new query after reboot establishes the installed version. An empty catalogue answers queries with `NO_IMAGE_AVAILABLE`.

See the [offline migration and rollback guide](MIGRATION.md) for staging a copied Z2M database, comparing HA discovery and checking adapter compatibility before any service handover.

## Device database

We chose `redb` for the coordinator’s host-side device database. The running bridge still uses `devices.json`; migration to the redb schema is part of building the replacement. Device and Home Assistant discovery identities must survive backup and restore.

The bench trial restored a snapshot taken while the coordinator committed a later device rename. With the same radio and its existing registry, Home Assistant reused both the device and entity registry entries and received a fresh state update. Power-loss testing on the target Pi and storage is deferred until a replacement candidate is ready.

## Interviewed devices and standard access

A product can use an ordinary application cluster before it has a dedicated model handler. Start with a `bridge/request/device/describe` request and use the endpoint's **server** list for reads. With a cluster specified, the response lists up to 32 catalogue attributes with their names, ZCL types, MQTT keys and any mapped On/Off control. If `next_after` is present and non-null, pass it as `after` for the next page. An optional attribute in the catalogue still needs confirmation from the member.

For example, if endpoint 3 advertises Temperature Measurement (1026), publish `{"zcl_03_0402_0000":""}` to `BASE/DEVICE/get`. A successful response on `BASE/DEVICE` includes `{"zcl_03_0402_0000":{"type":41,"raw":2150}}`. For this attribute, 2150 means 21.50 °C. An unsupported-attribute response carries its ZCL status. The generic representation preserves wire magnitudes, including attribute-specific sentinel values. Integer fields wider than 48 bits are decimal strings so JavaScript consumers can preserve them exactly.

When the descriptor and device handler identify an On/Off actuator, `{"zcl_03_0006_0000":"OFF"}` on `BASE/DEVICE/set` sends Off to endpoint 3. Some buttons advertise an On/Off server for their own events, so a server declaration alone enables reads. The bridge resolves the current short address before sending and publishes physical state on device confirmation. Scalar reads and On/Off commands work on the Home Automation and Light Link application profiles served by the adapter. Other profile types can be identified from saved descriptors.

Native properties on an interviewed endpoint use the same MQTT routes as other bridge devices. For example, publish `{"occupancy":""}` or `{"illuminance":""}` to the endpoint’s `/get` topic. A binary output that advertises write access accepts `{"binary_output":"ON"}` on `/set`. For an interviewed thermostat with weekly schedule support, publish `{"weekly_schedule_page":{"day":"monday","mode":"heat"}}` on `/get` to read one day’s heating transitions. A successful write followed by a readback publishes the value the device applied.

To read several schedule days, send `{"weekly_schedule":{"pages":[{"day":"monday","mode":"heat"},{"day":"tuesday","mode":"heat"}]}}` to the thermostat’s `/get` topic. To replace Monday on a heating-only thermostat, send `{"weekly_schedule":{"pages":[{"day":"monday","mode":"heat","transitions":[{"minute":360,"setpoint":20},{"minute":1320,"setpoint":16}]}]}}` to `/set`. Minutes run from local midnight and setpoints use °C. On a heating-and-cooling thermostat, use `"mode":"both"` and supply `heat` and `cool` in each transition.

The bridge validates the requested days against the interviewed transition capacity and setpoint limits. It reads each edited day back before sending the next. The device topic’s `weekly_schedule.completed` lists the confirmed days. A rejection, timeout or difference stops the operation and preserves earlier confirmations. When one day can hold more than ten transitions, `/get` emits `weekly_schedule_chunk` events with row offsets. The protocol has no last-chunk marker. If the response times out before its end can be proven, the result is `partial` and subsequent days stay unread. Edits require a day small enough for a verifiable readback.

For a specialist profile, the cluster description can still identify catalogue attributes on an advertised server. Its response contains `standard_access: false` and a null MQTT key for each attribute. The generic report decoder emits telemetry for endpoints with no native binding. Endpoints newly recognised solely through the standard environment, I/O and equipment mappings keep their raw telemetry alongside their named properties. The coordinator option `--standard-reports all` includes native endpoints. A standard remote can also publish `zcl_EP_command` with its decoded cluster, command ID and name. Use the dedicated product features for Home Assistant entities, engineering units, optional controls and vendor protocols.

### IAS ACE keypads and panels

To operate an alarm, bind the keypad's IAS ACE client directly to the alarm panel's IAS ACE server with `device/bind` and `clusters: ["ssIasAce"]`. Set `from_endpoint` and `to_endpoint` to the endpoints advertised by those devices. The physical panel applies its own authorization policy and reports its arming state. The bridge reads a physical panel’s `panel_status`, `panel_alarm_status`, seconds remaining and audible notification code on its server endpoint. Its status queries and accepted replies use APS link-key encryption.

For a keypad that needs the coordinator’s own enrolled zone directory, put its IEEE address and endpoint in `ias-ace-keypads.json` in the private state directory before starting the bridge. For example, `[{"ieee":"0200000000000001","endpoint":1}]` approves one reader. The directory service on local endpoint 2 requires an APS link-key-encrypted unicast and an interviewed IAS ACE client. It returns confirmed zone allocations, known types and status pages. Status requires a protected notification or attribute report received in the current membership within five minutes, accounting for notification delay. Missing status returns a failure. This service declines arm, disarm, bypass and emergency requests. A physical panel supplies authoritative panel and bypass state. The allowlist contains only device identifiers.

## Ember adapter

ASH and EZSP belong to the Ember adapter. Device capabilities, membership, MQTT and discovery can use a coordinator interface independent of the radio firmware. The Ember session permits one outstanding command, acknowledges callbacks while idle and queues callbacks arriving during commands. A timeout, reset or cancelled exchange retires that session. The bench process handles stack status, device announcements and ZDO/ZCL interview replies. An independent EZSP 19 probe and a read of the stored coordinator node identity succeeded on isolated bench hardware. See the [adapter migration gate](MIGRATION.md#ezsp-19-adapter-gate) for the production checks.

On an isolated Unix bench, run `cargo run --example ash_version -- PORT 115200 xonxoff 13 8`, substituting the serial path. This sends a software reset to the network coprocessor and asks its EZSP version. Each invocation releases the serial port on exit. On a SONOFF Dongle‑M MG24 with the vendor 7.4.5 image, two fresh acquisitions returned EZSP 13, stack type 2 and stack version 0x7450. The first request offered version 16 and the second offered 13. The pseudo-terminal test separately covers a link acknowledgement followed by reset before the application reply. [SONOFF firmware manifest](https://github.com/iHost-Open-Source-Project/hassio-ihost-sonoff-dongle-flasher/blob/136ab2fe736a1e5e9c880c2334d2c95adf1e2f25/firmware-build/FIRMWARE_LIST.json#L214-L220)

### Bench coordinator

Run the following commands on an isolated EZSP 13 or 19 Ember adapter with exclusive serial access. Each invocation sends an NCP software reset. Use one private state directory for the bench. Choose a channel, a short PAN ID in hexadecimal and a unique 16-digit extended PAN ID before formation. Keep those three settings for subsequent runs.

```text
cargo run --example bench_coordinator -- inspect PORT STATE_DIR
cargo run --example bench_coordinator -- form PORT STATE_DIR CHANNEL PAN_HEX EXT_PAN_HEX
cargo run --example bench_coordinator -- run PORT STATE_DIR CHANNEL PAN_HEX EXT_PAN_HEX
cargo run --example bench_coordinator -- interview PORT STATE_DIR CHANNEL PAN_HEX EXT_PAN_HEX
cargo run --example bench_coordinator -- probe PORT STATE_DIR CHANNEL PAN_HEX EXT_PAN_HEX LAST_NWK_HEX IEEE_HEX
```

Inspection reads the saved node identity without initializing a network. Formation requires an empty primary node token, unset network keys and a network initialization result of NOT_JOINED. A successful formation stores fresh security material on the radio. Resume verifies the saved coordinator role, PAN IDs and channel before initializing. Both paths wait for the NETWORK_UP callback, read back the network identity, joined state and node address, close joining and verify the trust-centre policy. The running process services callbacks until stopped. SIGINT or SIGTERM drains the supervised serial worker and releases the port. A later run resumes the same network from radio flash. [Silicon Labs networking reference](https://docs.silabs.com/zigbee/8.2.1/sisdk-ezsp-reference-guide/06-networking-frames) · [Pinned 7.4 stack token layout](https://github.com/SiliconLabs/gecko_sdk/blob/8a2efd2cb191cb250fd4c1e655b16b8a80be1997/protocol/zigbee/stack/config/token-stack.h)

Add `--catalogue PATH_TO_LISTINGS_REDB` to an interview or probe to print the observed identity and the candidate definitions from ZHA, deCONZ and Zigbee2MQTT after radio shutdown. Each candidate carries its source revision, source path and matching evidence. To replay a saved structured attempt, use `bench_coordinator report INTERVIEW.jsonl --catalogue PATH_TO_LISTINGS_REDB`. For older text logs, extract the Basic strings and endpoint descriptors from a single attempt and pass the resulting interview JSON to `zigbee-catalogue identify PATH_TO_LISTINGS_REDB INTERVIEW.json`.

The interview resumes the saved network, opens local pairing for up to 180 seconds and prints a readiness message. Reset the test device at that prompt. Its announcement closes admissions. The host requests its active endpoints and simple descriptors. On Home Automation endpoints it reads On/Off and Basic manufacturer and model attributes where the corresponding server is present. Replies and timeouts are printed to stdout. Save this output privately if you need a record. Use `probe` after an announcement to repeat the interview with pairing closed. If the NCP has lost its address mapping, the diagnostic labels the last announced address explicitly. If active endpoint discovery fails, a labelled probe on endpoint 1 can still test whether the light answers.

The trust centre closes admission on the first announcement. The coordinator retains the commissioning key until Ember reports requester verification success for each tracked joiner. The fallback is 60 seconds after admissions close. The timeout covers legacy devices and incomplete exchanges. The host records which condition released the key.

## MQTT bench bridge

Start a listener with a saved complete interview and a local MQTT broker, or use `interview-listen` to commission and serve a device in the same adapter session.

```sh
cargo run --example bench_coordinator -- listen PORT STATE_DIR CHANNEL PAN_HEX EXT_PAN_HEX --mqtt-local-port 1883 --device-report INTERVIEW.jsonl
```

Repeat `--device-report` to load further devices. The bridge uses a registered friendly name when available, then falls back to the IEEE address. It resolves the current network address before transmitting. Use `--light-index STANDARD_LIGHTS.json` for model-reviewed light selection. Omitting it enables generic light capabilities from the interview on this bench. Generation and provenance are described in the [function inventory](tools/devices/FUNCTIONS.md).

Publish `{"state":"OFF"}` to `zigbee2mqtt/NAME/set` or `{"brightness":""}` to `zigbee2mqtt/NAME/get`. Confirmed reports and matched read responses go to `zigbee2mqtt/NAME`. The managed listener saves confirmed state by device and endpoint and republishes the latest snapshot on reconnect. Button actions and raw attribute results publish separately on the compatible device topic. Actions also use the existing action topic. Expired gestures are discarded during recovery. Bridge availability and Home Assistant discovery configurations are retained. Each queue has a bound and reports overload on `zigbee2mqtt/bridge/logging`. The MQTT worker waits for radio readiness and drains accepted publications on orderly shutdown.

Broker integration tests use Mosquitto on loopback inside a disposable network namespace. Run `cargo test --locked --test bridge_roundtrip --test mqtt_broker --test mqtt_discovery -- --ignored --test-threads=1` there. The round-trip test uses a Linux pseudo-terminal and a simulated adapter.

## Device support design

Start from the interview. Preserve each endpoint’s profile, device type, server and client clusters, manufacturer, model and available firmware identification. Together these can identify a standard dimmable light. Optional attributes, commands and reporting need their own evidence. A remote that sends On/Off commands on a client cluster must not appear as a controllable lamp. Record the origin of each capability and keep an interrupted interview incomplete until it can resume.

Use reusable capability definitions for features such as on/off, level, metering and button events. Each definition describes the supported operations, wire mapping, units and reporting or binding requests. Keep stable endpoint and feature identities so the MQTT adapter can reproduce Zigbee2MQTT topics, access modes, payloads and Home Assistant discovery IDs across rejoins. Preserve observations separately from commands the host requested.

For exceptions, use narrowly matched overrides with a source revision and a regression example. Match manufacturer, model, endpoint signature and firmware range where known. An ambiguous match should leave the standard path active and be visible in diagnostics. A reviewed Rust function can implement a transformation that the data model cannot express. It returns a decoded observation or a requested operation. The session owner resolves the current device address, schedules the operation, correlates replies, and revokes outstanding work on leave or shutdown.

| Upstream | What to reuse as evidence |
|---|---|
| Zigbee2MQTT | The public feature names, access modes and MQTT behaviour are the compatibility target. Its device definitions live in zigbee-herdsman-converters. Reusable extensions compose inbound decoding, outbound commands, exposed features and setup. Some definitions also contain executable callbacks. [Device support guide](https://www.zigbee2mqtt.io/advanced/support-new-devices/01_support_new_devices.html) |
| ZHA | Its standard light matches an endpoint by device type and server clusters. Device quirks can reinterpret a cluster or add an entity or event when discovery alone falls short. [Standard light](https://github.com/zigpy/zha/blob/2da1a5b5676158ac7b603ce6b9f42641cb90e7a0/zha/application/platforms/light/__init__.py#L878-L907) · [Quirks](https://github.com/zigpy/zha-device-handlers/blob/b07fa76876e081d48744968f9fb806e7f1fee0ee/README.md) |
| deCONZ | Its Device Description Files compose reusable items, endpoint fingerprints, bindings and reporting. Items can call named functions or use JavaScript for a value transformation. Translate reviewed data with explicit units and preserve the source revision and applicable notices for any imports. [DDF format](https://dresden-elektronik.github.io/deconz-dev-doc/modules/ddf/) · [Innr plug example](https://github.com/dresden-elektronik/deconz-rest-plugin/blob/c24e81cf01c121583887acea24e647c3496ebd9a/devices/innr/sp_120.json) |

The Innr BF 263 is a first acceptance case. The upstream definition combines a standard light with one low-brightness exception. Its absolute-brightness converter changes a request for level 1 to level 2 before sending. Compare requested level, transmitted level, device readback and visible output on an isolated network before enabling that override. Compare MQTT exposes and discovery payloads independently. A second case, the Innr SP 120, tests whether one endpoint can expose switching, power and cumulative energy with independent units and source attributes. [BF 263 definition](https://github.com/Koenkk/zigbee-herdsman-converters/blob/ed29f80014f11cd3234e0e7371d1aace44e08982/src/devices/innr.ts#L179-L185) · [Level conversion](https://github.com/Koenkk/zigbee-herdsman-converters/blob/ed29f80014f11cd3234e0e7371d1aace44e08982/src/converters/toZigbee.ts#L1376-L1380) · [SP 120 metering quirk](https://github.com/zigpy/zha-device-handlers/blob/b07fa76876e081d48744968f9fb806e7f1fee0ee/zhaquirks/innr/innr_sp120_plug.py)

### Description and runtime boundary

The planned description format is data with a versioned schema. Loading it, resolving its fingerprint and listing its capabilities must work offline without running device-supplied code. A description selects named capabilities and codecs and supplies typed parameters such as units, scale, enum values and supported operations. Adding a model covered by existing behaviour takes one description. New behaviour is implemented and tested in Rust and is then available by name. The loader rejects unknown functions, incompatible types and conflicting mappings. Scripts and callbacks are outside the description format. The first implementation includes an offline reference catalogue, a data-driven Tuya profile loader and a reusable Rust device library. See the [catalogue inputs, lookup commands and offline interview example](tools/devices/README.md).

### Tuya acceptance case

Many Tuya products use standard clusters. For MCU-backed products sharing the model identifier `TS0601`, cluster `0xEF00` carries product-specific datapoints. The wire decoder can recover a datapoint ID, wire type and value without knowing the product. The meaning and write permission come from a matched description. Preserve the fingerprint and a reference to the original bytes so an unknown datapoint can be diagnosed later. An unknown product gets no datapoint writes. A matched MCU profile can also declare a time epoch. Its clock requests receive UTC and local wall time from the coordinator’s configured host timezone. [Zigbee2MQTT Tuya guide](https://www.zigbee2mqtt.io/advanced/support-new-devices/02_support_new_tuya_devices.html)

Use ZHA’s Tuya builder as the model for composing capabilities. Its presence sensors share range and distance mappings and select typed helpers for illuminance and timed settings. Zigbee2MQTT’s datapoint tables provide a compact reference for actual mappings and the MQTT property names we need to preserve. deCONZ shows which parts can live in a file, including an explicit datapoint ID; its radar description also calls a lux conversion script. Represent ordinary scaling, polarity and enum maps as data. Reserve named Rust codecs for layouts such as a packed schedule. [ZHA shared radar definition](https://github.com/zigpy/zha-device-handlers/blob/b07fa76876e081d48744968f9fb806e7f1fee0ee/zhaquirks/tuya/tuya_motion.py#L198-L250) · [Zigbee2MQTT radar mapping](https://github.com/Koenkk/zigbee-herdsman-converters/blob/ed29f80014f11cd3234e0e7371d1aace44e08982/src/devices/tuya.ts#L17138-L17194) · [deCONZ radar description](https://github.com/dresden-elektronik/deconz-rest-plugin/blob/c24e81cf01c121583887acea24e647c3496ebd9a/devices/tuya/_TZE204_qasjif9e_mmwave_radar.json#L78-L90)

Use the following cases when the device layer is implemented. These are expectations derived from pinned upstream definitions, awaiting wire verification.

| Case | Required result |
|---|---|
| Two TS0601 radars both report datapoint 104 with value 30 | For `_TZE204_qasjif9e`, publish 30 lux. For `_TZE204_laokfqwu`, publish a 30-second interval. Only the interval variant can accept a write to that datapoint. |
| Presence polarity and alternative light reports | Zigbee2MQTT interprets presence value 1 as true for `_TZE204_qasjif9e`, and 0 as true for `_TZE284_iadro9bf`. It also maps datapoints 12 and 104 to illuminance in that product family. A confirmed alternative report must update the same feature and discovery identity. |
| Partial interview or unexpected datapoint type | Keep the raw observation available. A model-only match or a mismatched wire type cannot authorize a write or publish a guessed measurement. |

[First radar in ZHA](https://github.com/zigpy/zha-device-handlers/blob/b07fa76876e081d48744968f9fb806e7f1fee0ee/zhaquirks/tuya/tuya_motion.py#L358-L397) · [Second radar in ZHA](https://github.com/zigpy/zha-device-handlers/blob/b07fa76876e081d48744968f9fb806e7f1fee0ee/zhaquirks/tuya/tuya_motion.py#L400-L426) · [Second radar in Zigbee2MQTT](https://github.com/Koenkk/zigbee-herdsman-converters/blob/ed29f80014f11cd3234e0e7371d1aace44e08982/src/devices/tuya.ts#L17251-L17262)

## Time service

The bridge serves the ZCL Time cluster on endpoint 1 from the host clock.
Set the service’s `TZ` to its IANA time zone (for example `Europe/London`)
or configure the host’s system zone. A container configured for UTC serves
UTC as local time. The server publishes the current or next representable
DST season and its signed shift. When zone data is unavailable it serves UTC
without claiming local rules. LastSetTime and ValidUntilTime are omitted
because the host clock interface supplies neither guarantee.
