//! Attach product-specific Tuya data points to the bridge. The codecs own the
//! wire values; this adapter supplies MQTT properties and discovery entities.
use super::Planned;
use crate::mqtt::{
    Command, Operation,
    discovery::{Feature, Sensor},
};
use anyhow::{Result, ensure};
use zigbee_devices::tuya::household::{Device, Family};

impl super::Bindings {
    /// Explicit site policy for Tuya gateway-status replies. A local broker
    /// connection on its own cannot establish internet reachability.
    pub fn set_tuya_gateway_internet(&mut self, online: bool) {
        self.tuya_gateway_internet = Some(online);
    }
}

pub(super) fn version_endpoint(
    devices: &super::Bindings,
    ieee: u64,
    report: &crate::interview_log::InterviewReport,
) -> Option<(u8, u16)> {
    use crate::interview_log::Observation;
    let has_build_id = report.entries.iter().any(|entry| matches!(&entry.observation,
        Observation::Attribute { cluster: 0, attribute: 0x4000, result, .. }
            if result.character_string().is_some_and(|id| !id.is_empty())));
    if has_build_id {
        return None;
    }
    devices.devices.range((ieee, 0)..=(ieee, u8::MAX)).find_map(|(&(_, ep), binding)| {
        binding.datapoints.filter(|d| d.supports(zigbee_devices::tuya::Service::McuVersion))
            .map(|_| (ep, binding.descriptor.profile))
    })
}

pub(super) fn features(device: Device) -> Vec<Feature> {
    if device.family() == Family::Cover {
        return vec![Feature::Cover, Feature::CompositeSensor("options")];
    }
    let mut features = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for field in device.fields() {
        let key = field.key.as_str();
        if !seen.insert(key) {
            continue;
        }
        if let Some([off, on]) = &field.switch {
            features.push(if key == "state" {
                Feature::Switch
            } else {
                Feature::StringBinary {
                    property: key,
                    off,
                    on,
                    writable: field.writable,
                }
            });
            continue;
        }
        let feature = match key {
            "current" => Feature::Sensor(Sensor::Current),
            "power" => Feature::Sensor(Sensor::Power),
            "voltage" => Feature::Sensor(Sensor::MainsVoltage),
            "energy" => Feature::Sensor(Sensor::Energy),
            "presence" => Feature::Binary("presence", "occupancy"),
            "soil_moisture" => Feature::Sensor(Sensor::SoilMoisture),
            "battery" => Feature::Sensor(Sensor::Battery),
            "temperature" => Feature::Sensor(Sensor::Temperature),
            "target_distance" => Feature::Sensor(Sensor::TargetDistance),
            "illuminance" => Feature::Sensor(Sensor::Illuminance),
            "battery_state" => Feature::TextSensor("battery_state"),
            _ if field.writable && !field.choices.is_empty() => Feature::Select {
                property: key,
                options: &field.choices,
            },
            _ if field.writable => {
                let (Some(max), Some(step)) = (field.max, field.step) else {
                    continue;
                };
                let unit = match key {
                    "minimum_range" | "maximum_range" => Some("m"),
                    "fading_time" | "detection_delay" | "countdown" => Some("s"),
                    _ => None,
                };
                Feature::Number {
                    property: key,
                    min: field.min,
                    max,
                    step,
                    unit,
                }
            }
            _ => continue,
        };
        features.push(feature);
    }
    features
}

pub(super) fn plan(device: Device, cmd: &Command, seq: u8) -> Result<Vec<Planned>> {
    ensure!(
        cmd.operation == Operation::Set,
        "This Tuya definition reports state; it has no per-property GET"
    );
    let mut requests = Vec::new();
    for (key, value) in &cmd.properties {
        let steps = device.set(key, value, seq.wrapping_add(requests.len() as u8))?;
        requests.extend(steps.into_iter().map(|request| (request, None)));
    }
    Ok(requests)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mqtt::discovery::Feature;
    use serde_json::json;
    use zigbee_devices::Endpoint;

    #[test]
    fn dual_socket_interview_exposes_and_routes_independent_controls() -> Result<()> {
        use crate::bridge::Bindings;
        use crate::interview_log::{
            AttributeResult, Descriptor, Entry, IdentitySource, InterviewReport, Observation,
        };
        let ieee = 0x0200_0000_0000_0001;
        let mut facts = vec![
            Observation::Target {
                ieee: format!("{ieee:016x}"),
                short: 7,
                identity_source: IdentitySource::Announcement,
            },
            Observation::ActiveEndpoints {
                sequence: 1,
                endpoints: vec![1],
            },
            Observation::SimpleDescriptor {
                sequence: 2,
                endpoint: 1,
                descriptor: Descriptor {
                    profile: 0x0104,
                    device: 0x0051,
                    version: 1,
                    servers: vec![0, 6, 0xef00],
                    clients: vec![],
                },
            },
        ];
        for (attribute, text) in [(4, "_TZE284_lq0ffndf"), (5, "TS0601")] {
            let mut bytes = vec![text.len() as u8];
            bytes.extend_from_slice(text.as_bytes());
            facts.push(Observation::Attribute {
                sequence: 3,
                endpoint: 1,
                cluster: 0,
                attribute,
                result: AttributeResult::Value {
                    data_type: 0x42,
                    bytes,
                },
            });
        }
        facts.push(Observation::Finished);
        let mut bindings = Bindings::default();
        let mut report = InterviewReport {
            finished: true,
            entries: facts.into_iter().map(|observation| Entry { unix_ms: 0, observation }).collect(),
        };
        bindings.add(&report, &[(ieee, "socket".to_owned())].into())?;
        assert_eq!(version_endpoint(&bindings, ieee, &report), Some((1, 0x104)));
        report.entries.push(Entry { unix_ms: 0, observation: Observation::Attribute {
            sequence: 3, endpoint: 1, cluster: 0, attribute: 0x4000,
            result: AttributeResult::Value { data_type: 0x42, bytes: vec![1, b'1'] },
        }});
        assert_eq!(version_endpoint(&bindings, ieee, &report), None);
        let configs = bindings.discovery_snapshot("bridge", "homeassistant", None)?;
        for (key, on, off) in [
            ("state_left", "ON", "OFF"),
            ("state_right", "ON", "OFF"),
            ("child_lock", "LOCK", "UNLOCK"),
            ("backlight_mode", "ON", "OFF"),
        ] {
            let entity = configs
                .values()
                .find(|config| config["command_topic"] == format!("bridge/socket/set/{key}"))
                .unwrap();
            assert_eq!(entity["payload_on"], on);
            assert_eq!(entity["payload_off"], off);
        }
        for (property, unit) in [
            ("current", "A"),
            ("power", "W"),
            ("voltage", "V"),
            ("energy", "kWh"),
        ] {
            assert!(configs.iter().any(|(topic, value)| {
                topic.ends_with(&format!("/{property}/config"))
                    && value["unit_of_measurement"] == unit
            }));
        }
        let command = |operation, properties: serde_json::Value| Command {
            device: "socket".into(),
            operation,
            properties: properties.as_object().unwrap().clone(),
        };
        let (id, steps) = bindings.plan(
            &command(
                Operation::Set,
                json!({"state_left":"ON","state_right":"OFF"}),
            ),
            42,
        )?;
        assert_eq!(id, (ieee, 1));
        assert_eq!(steps.len(), 2);
        for ((request, readback), (seq, dp, value)) in steps.iter().zip([(42, 1, 1), (43, 2, 0)]) {
            assert_eq!((request.endpoint, request.cluster), (1, 0xef00));
            assert_eq!(request.frame, [0x11, seq, 0, 1, 0, dp, 1, 0, 1, value]);
            assert!(readback.is_none());
        }
        assert!(
            bindings
                .plan(&command(Operation::Get, json!({"state_left":""})), 1)
                .is_err()
        );
        assert!(
            bindings
                .plan(&command(Operation::Set, json!({"countdown":30})), 1)
                .is_err()
        );
        // A hybrid endpoint can also implement a native server independently of its datapoints.
        for entry in &mut report.entries {
            if let Observation::SimpleDescriptor { descriptor, .. } = &mut entry.observation {
                descriptor.servers.push(0x0406);
                descriptor.clients.push(6);
            }
        }
        bindings.add(&report, &[(ieee, "socket".to_owned())].into())?;
        let (_, steps) = bindings.plan(&command(Operation::Get, json!({"occupancy":""})), 9)?;
        assert_eq!((steps[0].0.cluster, steps[0].0.frame.as_slice()), (0x0406, &[0x10,9,0,0,0][..]));
        let triggers = format!("ha/device_automation/0x{ieee:016x}");
        let generic = format!("{triggers}/ep_01_on_off/config");
        let configs = bindings.discovery_snapshot("base", "ha", None)?;
        assert_eq!(configs[&generic]["topic"], "base/bridge/feedback/socket/zcl_01_command");

        // With the same client cluster, a native TS0041 action handler owns
        // the button gestures. Its discovery must not acquire duplicate triggers.
        for entry in &mut report.entries {
            if let Observation::Attribute { cluster: 0, attribute, result, .. } = &mut entry.observation {
                let id = match *attribute {
                    4 => "_TZ3000_qgwcxxws",
                    5 => "TS0041",
                    _ => continue,
                };
                let mut bytes = vec![id.len() as u8];
                bytes.extend_from_slice(id.as_bytes());
                *result = AttributeResult::Value {data_type:0x42, bytes};
            }
        }
        bindings.add(&report, &[(ieee, "button".to_owned())].into())?;
        let configs = bindings.discovery_snapshot("base", "ha", None)?;
        assert!(!configs.contains_key(&generic));
        for gesture in ["single", "double", "hold"] {
            assert_eq!(configs[&format!("{triggers}/action_{gesture}/config")]["topic"], "base/button/action");
        }
        Ok(())
    }

    #[test]
    fn exact_fingerprints_plan_wire_commands_and_same_discovery_surface() -> Result<()> {
        let ep = Endpoint {
            id: 1,
            profile: 260,
            servers: &[0xef00],
            clients: &[],
        };
        let soil = Device::resolve("_TZE284_oitavov2", "TS0601", &ep)?.unwrap();
        assert_eq!(features(soil).len(), 5);
        assert!(features(soil).into_iter().any(|f| matches!(
            f,
            Feature::Select {
                property: "temperature_unit",
                ..
            }
        )));
        let radar = Device::resolve("_TZE204_qasjif9e", "TS0601", &ep)?.unwrap();
        let radar_surface = features(radar);
        assert_eq!(radar_surface.len(), 8); // both illuminance reports share one entity
        assert!(
            radar_surface
                .iter()
                .any(|f| matches!(f, Feature::Binary("presence", "occupancy")))
        );
        let command = |op, body: serde_json::Value| Command {
            device: "radar".into(),
            operation: op,
            properties: body.as_object().unwrap().clone(),
        };
        let cmd = command(
            Operation::Set,
            json!({"minimum_range":0.45,"radar_sensitivity":9}),
        );
        let actions = plan(radar, &cmd, 8)?;
        assert_eq!(actions.len(), 2);
        assert_eq!(
            actions[0].0.frame,
            [0x11, 8, 0, 1, 0, 3, 2, 0, 4, 0, 0, 0, 45]
        );
        assert_eq!(
            actions[1].0.frame,
            [0x11, 9, 0, 1, 0, 2, 2, 0, 4, 0, 0, 0, 9]
        );
        assert!(
            plan(
                radar,
                &command(Operation::Get, json!({"minimum_range":""})),
                8
            )
            .is_err()
        );
        assert!(plan(radar, &command(Operation::Set, json!({"presence":true})), 8).is_err());
        Ok(())
    }
}
