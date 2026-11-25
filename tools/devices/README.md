# Device catalogue

The reference index stores upstream definitions for offline lookup. The Rust device library resolves capabilities from interview evidence and its own reviewed descriptions. Radio sessions and the coordinator device database have separate lifetimes. A catalogue rebuild can therefore run with no adapter attached.

## Rebuild and query

Use the commits in [sources.lock.json](sources.lock.json). Check out each repository at its recorded revision. The Python tools read the committed Git tree and need no installed quirks or deCONZ libraries. Use Python 3.12 or newer for the pinned ZHA syntax.

For the Z2M extractor, build a clean upstream checkout with its declared Node and pnpm versions. Use `pnpm install --frozen-lockfile` and `pnpm run build`. This generates both the compiled definitions and the upstream model index. The subsequent Node extraction loads upstream code. The trial ran it in a read-only build container with networking disabled and no adapter attached. The Rust runtime consumes data files only.

```sh
python3 tools/devices/zha.py "$ZHA_CHECKOUT" > zha.jsonl
python3 tools/devices/deconz.py "$DECONZ_CHECKOUT" > deconz.jsonl
node tools/devices/z2m.mjs "$Z2M_CHECKOUT" > z2m.jsonl
cargo run -p zigbee-catalogue -- build devices.redb zha.jsonl deconz.jsonl z2m.jsonl
cargo run -p zigbee-catalogue -- stats devices.redb
cargo run -p zigbee-catalogue -- z2m-converters devices.redb
cargo run -p zigbee-catalogue -- model devices.redb 'SP 120'
cargo run -p zigbee-catalogue -- model devices.redb 'BF 263' --full
```

Build to a new path. The builder rejects empty inputs and mixed revisions within a source, imports all three sources in one transaction, and publishes the file after syncing. The manifest records the actual revisions imported. Compare the JSONL digests against the lock file to reproduce this particular catalogue. Retain the [upstream notices](licenses) with derived listings.

Bundle a redistributable catalogue with the revision manifest and licence texts. From the repository root, with the built database at `devices.redb`, run:

```sh
tar -czf devices-catalogue.tar.gz devices.redb tools/devices/sources.lock.json tools/devices/licenses
```


## Merge the sources by wire-model string

`zigbee-catalogue merged devices.redb` exports a JSON object with one entry in `models` per exact wire-model string. For a single model, use `zigbee-catalogue merged devices.redb 'SP 120'`. Each entry has a `candidates` map grouped by upstream source. Its numeric references resolve to the complete definitions in the same document’s `records` object. Reference numbers are local to that catalogue build. Multiple model keys can reference the same record. The full export retains definitions without a wire-model key in `unkeyed`. An explicitly declared empty string is its own key.

For `TS0601`, take the manufacturer from the device’s Basic cluster and the profile, endpoint and server clusters from its simple descriptor. Compare those observations with each reviewed fingerprint as a whole. A source entry may also have other restrictions or a predicate requiring review. The offline device resolver selects a reviewed datapoint profile only when its required evidence is present. The merged catalogue itself supplies no write authority.

## Look up an interview by fingerprint

`zigbee-catalogue fingerprint devices.redb interview.json` prints each matching source reference as a JSON line. The lookup covers the 18 model-less definitions at these source revisions and the four ZHA ZLinky signatures. The model index also retains the five deCONZ ZLinky meter-mode descriptions and the Z2M definition.

For an endpoint interview, supply the full list of application endpoints. Fields use the same names as the `describe` example below. Include `manufacturerID`, `type` or `powerSource` when known. A declared cluster list requires the observed set, including an explicitly observed empty list. A missing field cannot establish that match. For a Green Power commissioning observation, use the device ID and the numeric source ID, for example `{"greenPowerDeviceID":2,"sourceID":4194304}`. No match produces zero output lines. Overlapping references appear separately.

ZHA model extraction follows parent signature dictionaries, copied model lists and deep-copied signatures. The fingerprint extractor reads endpoint edits and Green Power ranges from the pinned quirk source. Its wire constants and supported builder predicates were reviewed against that revision and its locked zigpy 2.4.0 dependency. Review them when advancing the ZHA pin.

To repeat the offline acceptance check against the full rebuilt catalogue:

```sh
zigbee-catalogue merged devices.redb > all-models.json
python3 tools/devices/check_fingerprints.py path/to/zigbee-catalogue devices.redb all-models.json
```

## Interpreting a result

A row identifies an upstream source location. A model lookup returns candidates and preserves the original spelling and whitespace. Use `--full` to see the source-specific constraints. Flat manufacturer and model lists are search fields. Each upstream uses a different unit of registration.

| Source | What a row represents | Limits |
|---|---|---|
| ZHA | An extracted v1/v2 quirk identity pair or a declaration marked unresolved | Generic entity discovery also uses profile, device type and endpoint clusters. A model can work with no named quirk. |
| deCONZ | A paired manufacturer/model entry in a DDF, with its subdevices, fingerprints and resource-item references | Some DDFs also require a JavaScript predicate. Those candidates carry a review flag. Legacy C++ handlers and external bundles are outside this index. |
| Z2M | A generated device definition with its model alternatives, complete fingerprints, aliases, exposes and converter references | Runtime-dependent exposure lists are marked. An unnamed converter, callback or datapoint map needs review at its recorded source. |

The converter command ranks named Z2M functions by how many definitions reference them, counting a symbol once per definition and direction. This provides a practical implementation order. Anonymous callbacks remain attached to their individual definitions.

## Try the Rust descriptions

The `describe` example reads an interview from stdin. The profile and cluster IDs in this JSON example are decimal. Model and manufacturer may be absent. The resulting properties carry the endpoint and the evidence that selected them.

```sh
echo '{"manufacturer":"innr","model":"BF 263","endpoints":[{"id":1,"profile":260,"device_type":257,"servers":[6,8],"clients":[25]}]}' | cargo run -q -p zigbee-devices --example describe
```

The standard-light device types come from [ZHA's default matching rule](https://github.com/zigpy/zha/blob/2da1a5b5676158ac7b603ce6b9f42641cb90e7a0/zha/application/platforms/light/const.py#L94-L112). The two example Tuya profiles use the [pinned Z2M mappings](https://github.com/Koenkk/zigbee-herdsman-converters/blob/ed29f80014f11cd3234e0e7371d1aace44e08982/src/devices/tuya.ts#L17138-L17263). The checked-in JSON selects named Rust codecs. An unknown manufacturer, partial fingerprint or mismatched wire type yields no interpreted datapoint. A numeric write requires an explicit writable mapping and a value within its bounds. Writes are returned as proposed records. These profiles have been exercised with synthetic data.

### Application device types and endpoint evidence

The Rust API `zigbee_zcl::device_type::lookup(profile, device)` returns every indexed application template for that pair. An interview supplies the endpoint's real input and output clusters. Each entry identifies its source, version-specific template name, source profile and server/client cluster selections. The two lock flags in each SDK template distinguish required roles, optional roles preselected by the SDK, optional roles initially off and explicitly excluded roles. A repeated declaration with conflicting flags is marked separately. A missing role or a type with no template data does not establish cluster support. Smart Energy revisions and overlapping Home Automation and Lighting/Occupancy definitions remain separate candidates.

The generator verifies the Gecko SDK input hashes in the existing source lock. The Light Link certification proforma permits both `0xc05e` and `0x0104` in ZLL simple descriptors. Each ZLL template therefore appears under both keys. Lighting and Occupancy devices use `0x0104`. Where an identifier is ambiguous, such as a common-profile `0x0000` switch or lamp, inspect the interviewed cluster roles. The supplemental Home Automation allocation is Mini Split AC. Building Automation Table 2 also allocates device types absent from the SDK. Supplementary IDs without a reviewed template have empty cluster tables. Two sample templates and two contradictory Building Automation identifiers remain accessible in `SDK_EXCEPTIONS`. For the latter, the specification assigns the On/Off Light to profile `0x0105`, and distinguishes Constructed BACnet (`0x000a`) from Tunneled BACnet (`0x000b`). The original SDK codes remain inspectable. [ZLL conformance proforma §8.1.4, PI1](https://device.report/m/e899e580ebd581cd301dac85f100363b3e3a1a14b7abe80d165ba292e51e07bb.pdf) · [Zigbee specification](https://zigbeealliance.org/wp-content/uploads/2019/11/docs-05-3474-21-0csg-zigbee-specification.pdf) · [Lighting and Occupancy device specification](https://zigbeealliance.org/wp-content/uploads/2019/11/docs-15-0014-05-0plo-Lighting-OccupancyDevice-Specification-V1.0.pdf) · [Home Automation 1.2, Table 5.1](https://community.nxp.com/pwmxy87654/attachments/pwmxy87654/wireless-connectivity/698/1/075367r03ZB_AFG-Home_Automation_Profile_for_Public_Download.pdf) · [Building Automation, Table 2](https://www.scribd.com/document/200273641/12001r01ZB-MWG-ZigBee-Building-Automation-Standard-Version-1-0-Public-Downl)

Generate the Rust table and cross-reference the three locked device listings as follows. Use a Python version capable of parsing the pinned upstream ZHA syntax and the zigpy wheel identified in that checkout's `uv.lock`. Both generators run offline.

```sh
python3 tools/zcl/device_types.py --source "$SDK_XML_DIR"
python3 tools/devices/device_type_inventory.py \
    --listings "$LISTINGS" --zha "$ZHA_CHECKOUT" --wheel "$ZIGPY_WHEEL" \
    --sdk "$SDK_XML_DIR" --output "$EVIDENCE_DIR"
```

`device-type-inventory.json.gz` includes all source rows, including rows with no recoverable type. Selectors retain source grouping, alternatives, stated conditions and endpoint roles. A missing cluster list and an explicitly empty list are distinct. Partial lists retain known numeric entries separately and identify expressions which need review. ZHA bindings and replacement clusters describe post-match behavior and cannot establish the original endpoint type. Green Power commissioning selectors describe a separate namespace. `rows_per_type` counts each source row at most once per pair. Entries with no references are included in the type evidence, and the summary lists observed pairs lacking an indexed type. Identical hardware can have several source rows. This inventory establishes neither physical deployment nor implementation coverage. The archive embeds the three listing notices and the generated-table notice.

The generator also checks every profile/device enum in the pinned zigpy wheel. A separate comparison with zigpy commit `c6836d5e2960f04e0c189faa733517400234f94a` found the same 55 HA, 13 Light Link and 7 Green Power identifiers. Current ZHA consumes these profile-qualified enums and separately defines product-specific matches. Its IKEA CCT matcher explicitly declares a common-profile endpoint with the legacy ZLL colour-temperature device ID. Product assignments in a manufacturer-specific profile stay scoped to that profile. The two observed ZLL descriptors `0x1000` and `0xe15e` lack a corresponding device entry in the ZLL type table. Code `0x1000` does identify a commissioning *cluster*. For unindexed observations under the common profile, the report distinguishes explicitly reserved HA 1.2 ranges from other unassigned numbers and keeps the exact upstream source paths. [zigpy profile enums](https://github.com/zigpy/zigpy/tree/c6836d5e2960f04e0c189faa733517400234f94a/zigpy/profiles) · [ZHA light type matches](https://github.com/zigpy/zha/blob/2da1a5b5676158ac7b603ce6b9f42641cb90e7a0/zha/application/platforms/light/const.py) · [IKEA ZHA quirk](https://github.com/zigpy/zha-device-handlers/blob/b07fa76876e081d48744968f9fb806e7f1fee0ee/zhaquirks/ikea/cctlightzha.py) · [ZLL device table](https://www.silabs.com/documents/public/user-guides/ug103-09-fundamentals-zll.pdf)

The [specialist profile and unindexed fingerprint review](SPECIALIST_PROFILES.md) lists the first wire exchange for each specialist profile and traces all 31 observed unindexed profile/device pairs to their original selectors.
