# Contributing

Contributions to the bridge, reusable crates, documentation and device profiles are welcome. For a behaviour change, describe what a user or device observes, the expected result, and the evidence for it. A short anonymised frame sequence, a small synthetic interview or a failing test is usually enough.

## Working on a change

1. Branch from public `main`. Keep each change focused and preserve existing user-facing MQTT topics, discovery identities and persisted state unless the change explicitly addresses them.
2. For protocol behaviour, identify the specification revision and section, or the vendor document or upstream implementation you used. Distinguish a requirement from an observed device quirk. Record model and manufacturer fingerprints when they affect matching.
3. Keep product mappings with the device profiles and executable behaviour in Rust. For generated ZCL types, change the generator or its pinned input and regenerate the output. Record the upstream revision, licence and applicable notices with imported material.
4. Add a regression test for the behaviour that changed. For work involving the serial port, tasks, timers or MQTT, cover the relevant shutdown, interruption or reconnect path. Use synthetic device identities in tests.
5. Run the applicable checks and include the results in the pull request. Say which hardware or fixture you tested with, and which parts still need hardware verification.

## Running checks

```sh
cargo test --locked --workspace --all-targets
cargo deny --locked --all-features check
```

GitHub Actions builds the release binaries and runs the workspace, broker-backed tests and dependency checks on pull requests and pushes to main. Install cargo-deny with `cargo install --locked cargo-deny` to run the policy locally. A dependency that needs a new licence requires review. The MPL-2.0 exception applies to `serialport`. [Distribution notes](THIRD-PARTY.md)

The default suite uses temporary stores and simulated devices. Tests marked `ignored` have additional prerequisites. To exercise the MQTT and serial integrations, run an isolated Mosquitto broker on loopback port 1883 and use the following command in its network namespace:

```sh
cargo test --locked --test bridge_roundtrip --test mqtt_broker --test mqtt_discovery -- --ignored --test-threads=1
```

For a packaging change, run `tools/packaging/test-compose.sh` on a Linux Docker host. It builds the image, opens a pseudo-terminal and checks graceful stop and restart with persistent state.

These tests publish discovery and device state. Use a disposable broker for them. The [catalogue guide](tools/devices/README.md) describes source pins and reproduction of the catalogue. The [standards coverage guide](COVERAGE.md) describes generation checks for the ZCL tables.

## Reports and licensing

Exclude network keys, install codes, lock credentials, broker passwords, identifiable household names and raw coordinator backups from issues, commits and attachments. Replace actual device addresses consistently so a report still shows routing and address changes. For a suspected security vulnerability, contact the maintainer privately before posting technical details in an issue.

Original contributions use the project’s MIT OR Apache-2.0 terms. For third-party material, identify the original author and source, retain copyright and licence notices, and describe your modifications. [Third-party material and distribution](THIRD-PARTY.md)
