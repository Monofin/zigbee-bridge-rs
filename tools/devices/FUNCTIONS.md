# Device function inventory

The locked listings contain 7,252 source rows. A row can describe several products, and several rows can describe one product. The inventory keeps each source declaration and its model/manufacturer alternatives. A reference identifies work to port or verify. Runtime dispatch still requires the interviewed product and endpoint evidence.

| Source | Rows | Observed implementation surface |
|---|---:|---|
| Zigbee2MQTT converters | 4,690 | 7,856 distinct static exposure shapes including nesting, access bits, endpoints, ranges, units and enum values. Another 259 definitions compute their exposures at runtime. There are 157 named inbound and 120 named outbound converter symbols. |
| deCONZ | 1,113 | 1,059 subdevice-type/resource pairs. There are 2,006 distinct parse, 1,781 read and 364 write declarations. These operations reference 52 scripts and 348 inline expression digests. A further 225 rows use a match predicate. |
| ZHA quirks | 1,449 | 753 distinct registration/kind entries. The inventory locates direct base classes, class overrides and attributes, plus 2,430 builder steps using 116 method names. Generic entity discovery in ZHA core is outside these quirk registrations. |

## Reproduce

Use the three JSONL files produced by the [catalogue importer](README.md) and a Git checkout containing the pinned ZHA revision. The scanner validates the digest of each listing and reads ZHA source from the committed Git tree. Python 3.12 or newer is recommended.

```sh
python3 tools/devices/function_inventory.py run/devices run/devices/zha-upstream \
    --output run/devices/function-inventory.json
gzip -n -c run/devices/function-inventory.json > run/devices/function-inventory.json.gz
```

For each source, `requirements` groups signatures by operation. A signature carries all the source rows referencing it as indices into `definitions`. For instance, selecting Z2M `from_zigbee` entries with `signature.symbol == "fz.battery"` finds the 487 definitions that reference that converter. A separate `uses` count preserves repeated occurrences within a row.

`partial_family` links a known, narrower Rust primitive to its upstream converter. `requires_review` means the index carries no coverage claim. Neither status grants a write path. Device-level parity needs separate fixtures or observations for each exposed behaviour. Matching an MQTT property name or a cluster number alone is insufficient.

## Implementation order

1. Share the standard clusters. On/Off reporting occurs in 2,531 Z2M definitions and brightness reporting in 1,698. Lighting commands should use the verified endpoint, advertised colour capabilities and a fresh address lookup. The inventory retains the low-brightness quirk on 737 definitions. For sensors, the Rust measurement decoder handles standard battery percentage/voltage, temperature, relative humidity and measured pressure. Voltage curves, mis-scaled reports and scaled pressure require their own reviewed rules.
2. Port repeated vendor functions once. Tuya's on/off action converter occurs in 17 definitions. The single-button decoder selects the seven fingerprinted TS0041 white labels when their reported endpoints match. The MINI-ZSB supplies captured single, double and hold payloads. Other variants need independent verification. Keep each action as an ordered occurrence even when the previous action had the same value. The TS0041's descriptor advertises an On/Off server despite being a remote. The product description must also establish actuator semantics before exposing a writable switch.
3. Treat executable extensions as code. Z2M has 9,290 anonymous inbound and 12,442 anonymous outbound declaration slots. Many can share a factory. Map each slot back to its enclosing definition before deciding which factories have equivalent behaviour. Keep configure/onEvent/endpoint hooks separate from wire codecs and MQTT publication. The 6,446 Tuya datapoint entries across 532 definitions carry converter directions, property, option keys and source location. Converter functions, closure values, sentinels and write ranges require review. The deCONZ operation's source pointer leads to its arguments, inline expression or script. ZHA classes require following replacement clusters and inherited methods. A receiver stored in a variable can acquire builder steps outside the recorded registration chain.
4. Drive the MQTT mapping from validated exposures. Include endpoint, read/write/report access, unit, bounds, enum vocabulary and whether a value represents state or a discrete event. Compare replayed device observations and proposed commands with their pinned source contracts before enabling a family. Multiple source references can select one Rust implementation. Preserve conflicting or insufficient evidence for review.

The [TS0041 definition](https://github.com/Koenkk/zigbee-herdsman-converters/blob/ed29f80014f11cd3234e0e7371d1aace44e08982/src/devices/tuya.ts#L8734-L8759) identifies the seven white-label manufacturers and omits periodic battery reporting for this device. Its [action converter](https://github.com/Koenkk/zigbee-herdsman-converters/blob/ed29f80014f11cd3234e0e7371d1aace44e08982/src/lib/tuya.ts#L4042-L4054) supplies the action vocabulary.

## Select a review batch

```sh
python3 tools/devices/function_batch.py run/devices z2m brightness set --limit 20
python3 tools/devices/function_batch.py run/devices z2m battery publish --offset 20 --limit 20
python3 tools/devices/function_batch.py run/devices deconz state/on get --limit 20
```

Each batch reports the eligible count, exclusions, pinned revision and source locations. Z2M candidates must declare the requested operation on the property, inherit compatible access from containing shapes, and have a wire fingerprint or a manufacturer-constrained alias for a single wire model. A set/get candidate must also name that property on an outbound converter implementing that operation. Inbound converters advertise input cluster and message types; their output property still needs verification in source and tests. deCONZ candidates must have the relevant parse/read/write operation and manufacturer/model pair. Scripted match predicates need separate review. The index preserves applicable subdevice fingerprints and wire-operation pointers. The ZHA listing establishes quirk identity but has no authoritative MQTT access metadata, so its rows go to quirk review.

At these pins, the query finds 162 Z2M brightness/set candidates, 298 Z2M battery/publish candidates and 333 deCONZ state/on/parse candidates. These are sizes of independent review queues. A model match, exposure or presence in a batch never enables a runtime command.

## Generate the initial light applicability index

```sh
python3 tools/devices/light_index.py run/devices run/devices/z2m-source run/devices/standard-lights.json
```

This export materializes **25 exact Basic model strings from 21 of the 4,690 Z2M definitions**. The source proof is narrow. The generator accepts complete committed modules made up solely of plain product records using the default `m.light()` factory. It checks the upstream row ordinal, manufacturer vendor label, wire models, access bits, converter entry points and inbound message types. It rejects models shared with a different source definition or a named quirk in either comparison listing. Of the remaining 4,669 Z2M rows, 737 explicitly enable the brightness-one quirk and 3,932 fall outside the grammar. That count includes nonlights. The BF 263 is one of the 737. It has its own converter option even though its endpoint looks like an ordinary dimmable light.

The JSON separates exact selectors and source references from a shared access contract. Each property has independent permissions for accepting reports, reading an attribute and sending an absolute set command. This baseline covers ON, OFF and absolute brightness 0–254. It makes no claim for toggles, remembered fades, effects, configuration hooks, availability reporting, model-less third-party fingerprints or other product functions. Device behaviour still needs live or captured evidence. The host must first establish descriptor completeness and resolve competing family matches. `zigbee_devices::light_index::LightIndex` then checks the supplied light-endpoint count is one, the device type is recognised, and Basic, On/Off and Level Control servers are present. Unknown model strings and endpoint-shape mismatches fail lookup.

### Identify and startup color temperature

The reviewed product table records the application endpoint, device type and optional controls for each Basic manufacturer/model pair. The endpoint must also advertise the server cluster for the requested control. Identify sends the standard three-second Identify command and exposes one Home Assistant button per supported device. A sleeping device may need to wake before delivery. This table covers the converter's default duration; per-device `identify_timeout` overrides require a configuration import path.

`color_temp_startup` uses Color Control attribute `0x4010`. Numeric values and named presets use the device's observed physical range or the reviewed product range if that is absent. `previous` writes `0xffff`. A matching successful write response publishes the accepted setting. Read replies and reports publish the confirmed attribute, separately from the current color temperature. The existing Home Assistant lighting discovery has no standalone startup-temperature entity. [Identify converter](https://github.com/Koenkk/zigbee-herdsman-converters/blob/ed29f80014f11cd3234e0e7371d1aace44e08982/src/lib/modernExtend.ts#L560-L604) · [Startup-temperature converter](https://github.com/Koenkk/zigbee-herdsman-converters/blob/ed29f80014f11cd3234e0e7371d1aace44e08982/src/converters/toZigbee.ts#L1429-L1458)

## Household access replay

The comparison takes a private projection of the application's Basic model, manufacturer, device type and endpoint descriptors. Supplying the retained Zigbee2MQTT exposure definitions resolves conditional and dynamic properties for the devices present. Preserve the nested schema of list and composite exposures as one MQTT property.

```sh
python3 tools/devices/household_coverage.py prepare run/devices run/household/models.json run/household/input.json --live-exposes run/household/live-exposes.json
ZIGBEE_COVERAGE_INPUT="$PWD/run/household/input.json" ZIGBEE_COVERAGE_OUTPUT="$PWD/run/household/replay.json" cargo test -p zigbee-bridge-rs --lib household_access -- --ignored
python3 tools/devices/household_coverage.py finish run/household/input.json run/household/replay.json run/household/coverage.json
```

The replay constructs complete interviews with synthetic identities, runs the bridge's endpoint resolver and exercises GET/SET planning with representative values. Its report column lists registered observation handlers. Counts apply to distinct product-group/property/operation combinations. Physical operation, full value-domain behaviour, device configuration and retained Home Assistant identity need independent checks. Files containing the household inventory belong under the ignored private run directory.

The typed settings registry shares one standard StartUpOnOff implementation across reviewed products. A light selector includes the Basic manufacturer and model, application endpoint, device type and required clusters. The product's upstream definition must expose the property and use the pinned standard converters in both directions. The standard `previous` value is enum8 `0xff` at On/Off attribute `0x4003`. The reviewed TS0001 variant instead uses enum8 `2` at attribute `0x8002` and omits `toggle`. A write updates MQTT after the device confirms it. The bridge can decode current state and startup policy from the same report. [Upstream read/write mapping](https://github.com/Koenkk/zigbee-herdsman-converters/blob/ed29f80014f11cd3234e0e7371d1aace44e08982/src/converters/toZigbee.ts#L352-L372) · [Upstream report mapping](https://github.com/Koenkk/zigbee-herdsman-converters/blob/ed29f80014f11cd3234e0e7371d1aace44e08982/src/converters/fromZigbee.ts#L1006-L1019)

The standard effect mapping is gated by the product's resolved converter, its complete effect vocabulary and its Identify/Color Control servers. Six effects select an Identify command. Products that also expose colour loops use Move Hue. On successful delivery of a loop stop the bridge schedules a colour read after 100 ms using the active membership. It can continue handling radio callbacks during that interval. The four reviewed Hue products use a separate manufacturer-specific effect converter, including multi-command stop and scene-state reports. The Rust Philips2 codec sends the scene and speed together, sends both Hue and Identify stops for the two shared stop commands, and decodes matching manufacturer state reports. The effect colour input preserves the scene confirmed for the current membership. The four white-lamp profiles have encoder and discovery tests only for this feature. The IKEA colour options setting exposes `execute_if_off` as a boolean inside a single `color_options` object, backed by bit zero of the Color Control Options attribute. [Effect converter](https://github.com/Koenkk/zigbee-herdsman-converters/blob/ed29f80014f11cd3234e0e7371d1aace44e08982/src/converters/toZigbee.ts#L1488-L1507) · [Move Hue and post-stop read](https://github.com/Koenkk/zigbee-herdsman-converters/blob/ed29f80014f11cd3234e0e7371d1aace44e08982/src/converters/toZigbee.ts#L1217-L1252) · [Hue effect mapping](https://github.com/Koenkk/zigbee-herdsman-converters/blob/ed29f80014f11cd3234e0e7371d1aace44e08982/src/lib/philips.ts#L967-L1035)

## OTA discovery

The OTA profile is generated from the converter revision in `sources.lock.json`. Run the importer against its built checkout to regenerate the product list.

```sh
node tools/devices/z2m.mjs "$CONVERTER_CHECKOUT" --ota-profiles > crates/zigbee-devices/profiles/ota.json
```

The resolver requires both a declared product and an interviewed application endpoint with the OTA Upgrade client cluster. A device can report its installed version before a matching image is available. Transfers require a compatible image in the coordinator's approved catalogue.


## Relay pulse timers and motion input reads

The reviewed SONOFF relays and S60ZBTPG use the CoolKit protocolData command on cluster `0xfc11` for `inching_control_set`. The MQTT SET takes all three fields, `inching_control`, `inching_mode` and `inching_time`. Time ranges from 0.5 to 3599.5 seconds in half-second steps. The pulse timer runs in the device. The property has no read or report in the pinned definition. The external relay input publishes the On/Off Toggle command as an action. A ZBMINIR2 reporting software build 1.1.0 or later also decodes the detached-switch event attribute. [SONOFF inching encoder](https://github.com/Koenkk/zigbee-herdsman-converters/blob/ed29f80014f11cd3234e0e7371d1aace44e08982/src/devices/sonoff.ts#L2686-L2774) · [SONOFF external actions](https://github.com/Koenkk/zigbee-herdsman-converters/blob/ed29f80014f11cd3234e0e7371d1aace44e08982/src/devices/sonoff.ts#L1933-L1968)

Occupancy and illuminance GET requests on a device topic select a matching standard input server from the interview. An explicit endpoint topic addresses that endpoint. The MQTT state comes from the correlated Zigbee attribute response. VALLHORN's product-specific endpoint 3 lux report also works when the simple descriptor omits its lux server. That report quirk authorizes reception only. The two reviewed Tuya cover variants publish `options.motor_speed` from a reported DP 105 value. Their configured travel direction has a SET path and no confirmed state source.

## IKEA level configuration and RGB colour temperature

The IKEA level configuration profile covers `execute_if_off` and `current_level_startup`. The bridge reads each attribute separately because both are optional. Changing ExecuteIfOff first reads the Options bitmap and preserves its other bits. A write response prompts a fresh read of the settings. Only received attribute values enter the MQTT composite, and a new membership starts with an empty cache. The startup level accepts 1–254 and the wire presets `minimum` (0) and `previous` (255). [IKEA light definition](https://github.com/Koenkk/zigbee-herdsman-converters/blob/ed29f80014f11cd3234e0e7371d1aace44e08982/src/lib/ikea.ts#L84-L94)

LED1624G9 implements its 153–500 mired control through XY. A read requests ColorMode, CurrentX and CurrentY. A setting sends MoveToColor. A confirmed XY observation publishes its measured coordinates, XY mode and derived colour temperature together. To regenerate the lookup, run `python3 tools/devices/kelvin_xy.py CONVERTER_CHECKOUT crates/zigbee-devices/profiles/kelvin-xy.bin` against the converter revision in the source lock. [Product definition](https://github.com/Koenkk/zigbee-herdsman-converters/blob/ed29f80014f11cd3234e0e7371d1aace44e08982/src/devices/ikea.ts#L245-L259) · [Colour conversion](https://github.com/Koenkk/zigbee-herdsman-converters/blob/ed29f80014f11cd3234e0e7371d1aace44e08982/src/lib/color.ts#L229-L241)

### Thermostat and Fan Control

Home Automation heating/cooling units, thermostats and mini-split ACs with a
Thermostat server expose `local_temperature` and `system_mode`. Occupied
heating and cooling setpoints require a successful attribute read or report
and a compatible control sequence. A Fan Control server has its own
`fan_mode`, including on a standalone fan. Pairing reads both sequences and
the thermostat's occupied setpoints and adjustable limits. Migration retains
matching typed cached values from each server endpoint.

MQTT GETs and SETs use the ordinary device property topics. Each successful
attribute write schedules a new read. Device reports and correlated readbacks
supply the published state, including when the thermostat rounds a setpoint.
Temperature is in °C. A controller with a single heating or cooling sequence,
confirmed mode and known minimum and maximum setpoints gets a Home Assistant
MQTT climate entity. Fan speed options come from the reported fan sequence.
Missing capabilities yield read-only state. Dual-system setpoints remain
read-only pending support for the live opposite-setpoint/deadband constraint.
The operating mode controls offer off, heat and/or cool according to the
sequence. Manufacturer-specific modes need a reviewed mapping.

## Standard IAS Zone

On an interviewed HA or Light Link IAS Zone server, `zone_type` and `zone_status` support MQTT GET. Attribute reports and unicast Zone Status Change Notifications update the raw `zone_status` bitmap. A successful ZoneType read or report selects the primary Home Assistant binary sensor. Alarm1 indicates motion, first-portal opening, fire, water, carbon monoxide or vibration according to the zone type. MQTT `contact` means closed when true; Home Assistant shows the opening sensor active when open. The second alarm and diagnostic flags remain available in the raw bitmap. A fire indication uses Home Assistant's safety class because the zone type cannot identify a smoke-sensing mechanism. Existing product-specific input handlers retain their interpretations and discovery identities.

On managed pairing, the coordinator advertises an IAS client and reads ZoneState, IAS_CIE_Address and ZoneID from each interviewed IAS server. For an unclaimed zone it reserves a stable ID, writes its own IEEE address, waits for a successful attribute write, then sends Zone Enroll Response. An explicit Zone Enroll Request receives its own transaction sequence. A zone using Auto-Enroll-Response can receive the assignment without asking. The coordinator confirms completion by reading back the enrolled state, its own address and the assigned ID. Existing zones on this CIE can retain their valid IDs. A different panel address, a conflicting assignment or incomplete ownership data prevents configuration. [ZCL R8 §8.2.2.1.3](https://csa-iot.org/developer-resource/specifications-download-request/) · [IAS attributes and command identifiers](https://software-dl.ti.com/simplelink/esd/simplelink_cc13x2_26x2_sdk/3.10.01.11/exports/docs/zigbee/doxygen/zigbee/html/group___s_s___i_a_s___z_o_n_e.html)

The managed state directory contains `ias-zones.redb`. A reservation commits before transmitting the assignment. A restart resumes pending work using a fresh short-address lookup. A confirmed leave releases all that member’s IDs and revokes its enrollment authority. A new managed interview can authorize it again. An existing interview with no enrollment record gets a read-only survey. A new allocation waits while an earlier IAS ID on this panel is unknown. The post-join procedure does not require reopening network admission.

Publish `{"id":"sensor","endpoint":1}` to `BASE/bridge/request/device/describe` to inspect `data.ias_enrollment`. A successful readback returns `{"state":"confirmed","zone_id":0}` for the first available number. A reserved number may also be visible during verification. A silent endpoint gets three bounded inspection attempts. A later enrollment request, a zone notification from a timed-out endpoint or an authenticated rejoin can resume work. Per-attempt diagnostics live with the radio session.

## Tuya metered switch and double socket

MG-ZG01W matches `_TZE200_gbagoilo` or `_TZE284_xnwxmj8z`. MG-AU03GPOZLP-XX matches `_TZE284_lq0ffndf`. Both require Basic model `TS0601` and endpoint 1 with an HA-profile Tuya MCU server. The similar `_TZE204_gbagoilo` maps to a different upstream product. The common profile handles signed, big-endian current, power and voltage values with their respective divisors, relay power-on behaviour and the panel backlight. The single relay supplies state and countdown. The double socket adds left/right relay control, energy and child lock, all on endpoint 1. Discovery uses the corresponding MQTT switch, sensor, select and number properties. [Product definitions](https://github.com/Koenkk/zigbee-herdsman-converters/blob/ed29f80014f11cd3234e0e7371d1aace44e08982/src/devices/tuya.ts#L8215-L8281) · [Wire value conversion](https://github.com/Koenkk/zigbee-herdsman-converters/blob/ed29f80014f11cd3234e0e7371d1aace44e08982/src/lib/tuya.ts#L527-L575)

Writes use the pinned converter's BOOL or four-octet VALUE encoding, and reports supply the observed state. No per-property GET is defined. Synthetic frames exercise all declared datapoints, both relay roles, alternate upstream report commands and negative fingerprints. A synthetic interview runs through the normal discovery and control routes. Physical commissioning, including whether a variant needs the upstream Basic-cluster initialisation read before its first report, still needs testing. [Device codecs](../../crates/zigbee-devices/src/tuya/household.rs) · [Bridge routing tests](../../src/bridge/tuya.rs) · [Upstream initialisation](https://github.com/Koenkk/zigbee-herdsman-converters/blob/ed29f80014f11cd3234e0e7371d1aace44e08982/src/lib/tuya.ts#L1338-L1343)

### Tuya services at commissioning and runtime

Match manufacturer, model and endpoint before selecting MCU services. The service set in the [TS0601 profiles](../../crates/zigbee-devices/profiles/ts0601-household.json) is independent of the datapoint map and clock epoch.

| Matched definition | Fresh admission | Incoming requests and reports |
|---|---|---|
| TS0601 soil, radar and both metered outlet definitions using `tuyaBase({dp:true})` | Send one Basic Read Attributes with IDs `0004, 0000, 0001, 0005, 0007, fffe`. After admission, request the MCU version once if the interview supplied no Basic software build ID. | Decode DP commands `01, 02, 05, 06, 0a` using the product map. Recognise `11` as an MCU version report and `25` as an internet-status request. |
| Both profiled TS0601 curtain variants | Use the ordinary interview. | Decode DP commands `01, 02`. Reply to time command `24` using the profile epoch. Motor commands and DP meanings remain specific to the manufacturer. |
| Reviewed TS0041 buttons and the ZN231392 TS0001 valve profile | Send the same Basic setup read. The button's configure handler omits battery reporting configuration. | Keep the existing button action or relay handlers. A Basic setup read gives neither device a TS0601 datapoint map. |

The grouped Basic read follows the identity read on a newly admitted member. Loading saved members, running a diagnostic probe, and resuming the coordinator leave this step dormant. A missing optional attribute can be recorded as unsupported. A nonzero MCU version report publishes `mcu_version` as a separate MQTT state property. The Zigbee module build ID and OTA image version keep their original meanings.

For managed listeners, `--tuya-internet-status online|offline` declares the value to return to profiled devices requesting gateway status. It is a fixed operator declaration. With no declaration a well-formed request receives ZCL failure. Local MQTT availability supplies no evidence of WAN connectivity. A valid reply echoes the two MCU sequence bytes and returns a single status byte. [Tuya gateway status](https://developer.tuya.com/en/docs/mcu-standard-protocol/mcusdk-zigbee-uart-protocol?id=Kdg17v4544p37) · [ZHA wire tests](https://github.com/zigpy/zha-device-handlers/blob/b07fa76876e081d48744968f9fb806e7f1fee0ee/tests/test_tuya_mcu.py)

The pinned base enables announce queries, periodic queries, Basic binding and forced clock updates individually. None of the reviewed soil, radar or metered definitions requests those options. The legacy curtain and IAS definitions use other converters. The three TS050x light definitions select the Tuya light extension. Recheck the exact definition before enabling a service for another fingerprint. Vendor weather and MCU OTA commands need a separate product contract and state machine. [Z2M service implementation](https://github.com/Koenkk/zigbee-herdsman-converters/blob/ed29f80014f11cd3234e0e7371d1aace44e08982/src/lib/tuya.ts) · [Product definitions](https://github.com/Koenkk/zigbee-herdsman-converters/blob/ed29f80014f11cd3234e0e7371d1aace44e08982/src/devices/tuya.ts)

One example of why the manufacturer matters is ZTH05Z. Its definition queries the datapoints on announcement except for the ONENUO fingerprint, for which the maintainer records excessive traffic and battery drain. Other definitions explicitly select a 2000 clock epoch or a periodic query. New profiles can share a codec while keeping their triggers and timing independent. [ZTH05Z exception](https://github.com/Koenkk/zigbee-herdsman-converters/blob/ed29f80014f11cd3234e0e7371d1aace44e08982/src/devices/tuya.ts#L4587-L4602)
