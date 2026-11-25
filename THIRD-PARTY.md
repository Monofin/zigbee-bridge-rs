# Third-party material and distribution

Engineering review of 25 November 2026. The original bridge is available under MIT OR Apache-2.0. Recipients also need the terms that accompany upstream material. This review covers the source tree and Cargo.lock with SHA-256 `eb4678b82960aa411aecf209f682b5ce25e03215612a7b58a0400fbf3f6077cd`.

## Source distribution

| Material | Evidence and applicable terms |
| --- | --- |
| Zigbee cluster commands, device types and attribute tables | The three Rust outputs reproduced from the pinned Gecko SDK inputs and checked-in generators. All 24 XML files carry Silicon Labs’ Apache-2.0 header. The 25th pinned input is the package manifest. The generated Rust files carry attribution and the crate includes the licence. This covers the identified SDK inputs. Standards publications and firmware images require their own distribution rights. [Source lock](tools/zcl/source.lock.json) |
| Device profiles and converter-derived Rust code | The ZHA and ZHA device-handler Apache-2.0, deCONZ BSD-3-Clause and zigbee-herdsman-converters MIT notices match the pinned upstream texts. The Kelvin binary table reproduced from the pinned converter source. The upstream RGB-to-xy implementation also credits cie-rgb-converter, which has its own MIT notice. Preserve both notices with the Hue implementation. [Profile provenance](crates/zigbee-devices/profiles/LICENSING.md) |
| Optional device catalogue | The three device-listing importers have separate upstream pins and notices. Distributors of an exported catalogue should inspect the actual archive for the applicable notices. The source repository includes the importers and selected profiles. [Pins](tools/devices/sources.lock.json) · [Notices](tools/devices/licenses) |

## Binary distribution

The lockfile contains 137 registry packages. The review verified the registry archives against the lockfile checksums and inspected their package licence declarations. The default Linux ARM64 workspace dependency tree contained 96 registry packages across normal and build dependencies, including procedural macros. A build dependency in this count does not establish that its code is in a shipped executable.

This repository distributes source. For a binary distribution, check the release archive for the following items.

- Include applicable copyright notices and full licence texts for the code and data actually distributed. Examples include the WHATWG notice in `encoding_rs`, the separate BoringSSL and other notices in `ring`, and the project’s device profile notices. The locked `rumqttc` crate declares Apache-2.0 and lacks a standalone licence file in the registry archive. Check its upstream notices when preparing the bundle. [Apache-2.0 redistribution terms](https://www.apache.org/licenses/LICENSE-2.0#redistribution)
- `serialport 4.10.1` is MPL-2.0 and is in the ARM64 dependency tree. Its cached sources matched all 50 files in the locked crate archive. For an executable distribution, make the covered source, including any modifications shipped, available under the MPL and tell recipients how to obtain it. Keep the exact covered source available with the release or at the stated location. Mozilla permits a larger work to have its own terms. [MPL-2.0 §§3.2–3.3](https://www.mozilla.org/en-US/MPL/2.0/)
- Record the licensing choice for alternatives. For example, `unescaper` in this build offers MIT or GPL-3.0-only. MIT is available. Separately assess any bundled OS libraries, container base, firmware or catalogue for the specific release.

This source and dependency review does not establish independent legal sign-off, patent clearance, trademark permissions or Zigbee certification. For a binary or container release, repeat the review against the exact artifacts.
