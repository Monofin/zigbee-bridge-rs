//! Replay one mixed light's interview through the normal bindings and discovery.
use super::*;
use crate::{
    interview_log::{AttributeResult, Entry, IdentitySource, InterviewReport, Observation},
    mqtt::{Command, Operation},
};
use serde_json::json;
use zigbee_devices::standard_controls::StartupRequest;

const FIXTURE: u64 = 0x0200910000001001;
fn hue_interview() -> InterviewReport {
    lamp_interview("LTA006", vec![0, 3, 6, 8, 0x300, 0xfc03], vec![])
}

fn lamp_interview(model: &str, servers: Vec<u16>, clients: Vec<u16>) -> InterviewReport {
    let endpoint = 11;
    let ieee = FIXTURE;
    let mut observations = vec![
        Observation::Target {
            ieee: format!("{ieee:016x}"),
            short: 0x1234,
            identity_source: IdentitySource::Z2mDatabase,
        },
        Observation::ActiveEndpoints {
            sequence: 2,
            endpoints: vec![endpoint],
        },
        Observation::SimpleDescriptor {
            sequence: 3,
            endpoint,
            descriptor: Descriptor {
                profile: 260,
                device: 268,
                version: 0,
                servers,
                clients,
            },
        },
    ];
    for (attribute, s) in [(4, "Signify Netherlands B.V."), (5, model)] {
        let mut bytes = vec![s.len() as u8];
        bytes.extend(s.as_bytes());
        observations.push(Observation::Attribute {
            sequence: 4,
            endpoint,
            cluster: 0,
            attribute,
            result: AttributeResult::Value {
                data_type: 0x42,
                bytes,
            },
        });
    }
    observations.push(Observation::Finished);
    InterviewReport {
        finished: true,
        entries: observations
            .into_iter()
            .map(|observation| Entry {
                unix_ms: 0,
                observation,
            })
            .collect(),
    }
}

#[test]
fn ota_discovery_requires_both_a_declared_product_and_an_ota_client() -> Result<()> {
    // Also cover a device whose only supported application function is OTA.
    for (model, clients, expected) in [
        ("LTA006", vec![0x19], true),
        ("LTA006", vec![], false),
        ("Unknown Fixture", vec![0x19], false),
    ] {
        let mut bindings = Bindings::default();
        bindings.add(&lamp_interview(model, vec![0], clients), &Default::default())?;
        let configs = bindings.discovery_snapshot("zigbee2mqtt", "homeassistant", None)?;
        assert_eq!(
            configs.contains_key("homeassistant/update/0x0200910000001001/update/config"),
            expected,
        );
    }
    Ok(())
}

#[test]
fn device_wide_identify_and_startup_share_existing_mqtt_device_topic() -> Result<()> {
    let endpoint = 11;
    let ieee = FIXTURE;
    let mut devices = Bindings::default();
    devices.add(
        &hue_interview(),
        &[(ieee, "Fixture".to_owned())].into_iter().collect(),
    )?;

    let plan = |operation, key: &str, value| {
        devices.plan(
            &Command {
                device: "Fixture".to_string(),
                operation,
                properties: [(key.to_string(), value)].into_iter().collect(),
            },
            5,
        )
    };
    let (peer, identify) = plan(Operation::Set, "identify", json!("identify"))?;
    assert_eq!(peer, (ieee, endpoint));
    assert_eq!(identify[0].0.frame, [0x11, 5, 0, 3, 0]);
    assert!(plan(Operation::Get, "identify", json!("")).is_err());
    let (_, startup) = plan(Operation::Set, "color_temp_startup", json!("previous"))?;
    assert!(matches!(
        startup[0].1,
        Some(Read::Startup(StartupRequest::Write(65535)))
    ));
    assert_eq!(
        startup[0].0.frame,
        [0x10, 5, 2, 0x10, 0x40, 0x21, 0xff, 0xff]
    );
    assert!(matches!(
        plan(Operation::Get, "color_temp_startup", json!(""))?.1[0].1,
        Some(Read::Startup(StartupRequest::Read))
    ));
    let discovery = devices.discovery_snapshot("zigbee2mqtt", "homeassistant", None)?;
    let button = &discovery["homeassistant/button/0x0200910000001001/identify/config"];
    assert_eq!(button["device_class"], "identify");
    assert_eq!(
        button["unique_id"],
        "0x0200910000001001_identify_zigbee2mqtt"
    );
    assert_eq!(button["command_topic"], "zigbee2mqtt/Fixture/set/identify");
    assert_eq!(button["payload_press"], "identify");
    assert!(button.get("state_topic").is_none());
    assert!(!discovery.keys().any(|t| t.contains("color_temp_startup")));
    Ok(())
}

#[test]
fn hue_color_uses_only_a_confirmed_scene_on_the_current_membership() -> Result<()> {
    let mut devices = Bindings::default();
    let names = [(FIXTURE, "Fixture".to_owned())].into_iter().collect();
    let report = hue_interview();
    devices.add(&report, &names)?;
    let config = &devices.discovery_snapshot("base", "ha", None)?["ha/text/0x0200910000001001/effect_color/config"];
    assert_eq!(config["command_topic"], "base/Fixture/set/effect_color");
    assert!(config.get("state_topic").is_none());
    let mut command = Command {
        device: "Fixture".into(),
        operation: Operation::Set,
        properties: serde_json::from_value(
            json!({"effect_color":{"x":0.5,"y":0.25},"brightness":127}),
        )?,
    };
    let (_, fresh) = devices.plan(&command, 100)?;
    assert_eq!(&fresh[0].0.frame[5..], &[8, 0, 0xff, 0x7f, 0xff, 0x3f]);
    assert_eq!(
        fresh[1].0.cluster, 8,
        "brightness must follow effect activation"
    );
    let frame = zigbee_zcl::Frame::decode(&[0x1c, 0x0b, 0x10, 3, 10, 2, 0, 0x41, 3, 32, 0, 1])?;
    let scene = zigbee_devices::standard_controls::hue::report(frame).unwrap();
    devices
        .devices
        .get_mut(&(FIXTURE, 11))
        .unwrap()
        .confirmed_hue_scene = scene.scene_id;
    let (_, active) = devices.plan(&command, 100)?;
    assert_eq!(
        &active[0].0.frame[5..],
        &[0x28, 0, 0xff, 0x7f, 0xff, 0x3f, 1]
    );
    devices.add(&report, &names)?;
    let (_, rejoined) = devices.plan(&command, 100)?;
    assert_eq!(rejoined[0].0.frame, fresh[0].0.frame);
    command.operation = Operation::Get;
    assert!(devices.plan(&command, 100).is_err());
    Ok(())
}
