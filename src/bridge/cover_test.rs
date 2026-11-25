//! A future model goes through the same interview, discovery and MQTT planner.
use super::*;
use crate::{
    interview_log::{AttributeResult, Entry, IdentitySource, InterviewReport, Observation},
    mqtt::{Command, Operation},
};
use serde_json::{Value, json};
use zigbee_devices::cover::{Axis, CLUSTER};

const EP: u8 = 4;
const IEEE: u64 = 0x0200_0000_0000_0021;

#[path = "../../tests/support/cover_interview.rs"]
mod fixture;

fn cover_fixture(profile: u16, server: bool, kind: u8, config: u8, commands: &[u8]) -> Result<Bindings> {
    let mut bindings = Bindings::default();
    fixture::bind(&mut bindings, (IEEE, EP), profile, server, kind, config, commands)?;
    Ok(bindings)
}

fn plan(
    bindings: &Bindings,
    operation: Operation,
    key: &str,
    value: Value,
) -> Result<Vec<Planned>> {
    let (id, steps) = bindings.plan(
        &Command {
            device: "Shade".into(),
            operation,
            properties: [(key.into(), value)].into_iter().collect(),
        },
        17,
    )?;
    assert_eq!(id, (IEEE, EP));
    Ok(steps)
}

#[test]
fn standard_cover_uses_profile_server_and_command_evidence_with_stable_identity() -> Result<()> {
    let topic = format!("homeassistant/cover/0x{IEEE:016x}/cover/config");
    for profile in [0x104, 0xc05e] {
        let bindings = cover_fixture(profile, true, 8, 0x18, &[5, 8])?;
        let configs = bindings.discovery_snapshot("zigbee2mqtt", "homeassistant", None)?;
        let ha = &configs[&topic];
        assert_eq!(ha["unique_id"], format!("0x{IEEE:016x}_cover_zigbee2mqtt"));
        assert_eq!(ha["tilt_command_topic"], "zigbee2mqtt/Shade/set");
        assert_eq!(ha["set_position_topic"], "zigbee2mqtt/Shade/set");
        for (key, val, frame) in [
            ("state", json!("OPEN"), vec![0x11, 17, 0]),
            ("state", json!("CLOSE"), vec![0x11, 17, 1]),
            ("state", json!("STOP"), vec![0x11, 17, 2]),
            ("position", json!(0), vec![0x11, 17, 5, 100]),
            ("position", json!(100), vec![0x11, 17, 5, 0]),
            ("tilt", json!(15), vec![0x11, 17, 8, 85]),
        ] {
            let steps = plan(&bindings, Operation::Set, key, val)?;
            assert_eq!(
                (steps[0].0.endpoint, steps[0].0.cluster, &steps[0].0.frame),
                (EP, CLUSTER, &frame)
            );
            assert!(steps[0].1.is_none());
        }
        for bad in [json!(-1), json!(101), json!(1.2), json!("25")] {
            assert!(plan(&bindings, Operation::Set, "position", bad).is_err());
        }
        let steps = plan(&bindings, Operation::Get, "tilt", Value::Null)?;
        assert_eq!(steps[0].0.frame, [0x10, 17, 0, 9, 0]);
        assert!(matches!(steps[0].1, Some(Read::Cover(Axis::Tilt))));
    }
    for (profile, server) in [(0x104, false), (0xc05e, false), (0x0109, true)] {
        let bindings = cover_fixture(profile, server, 8, 0x18, &[5, 8])?;
        assert!(
            !bindings
                .discovery_snapshot("zigbee2mqtt", "homeassistant", None)?
                .contains_key(&topic)
        );
        assert!(plan(&bindings, Operation::Set, "state", json!("OPEN")).is_err());
    }
    let bindings = cover_fixture(0x104, true, 7, 0x10, &[])?;
    let configs = bindings.discovery_snapshot("zigbee2mqtt", "homeassistant", None)?;
    assert!(configs[&topic].get("tilt_status_topic").is_some());
    assert!(configs[&topic].get("position_topic").is_none());
    assert!(configs[&topic].get("tilt_command_topic").is_none());
    assert!(plan(&bindings, Operation::Set, "tilt", json!(55)).is_err());
    assert_eq!(
        plan(&bindings, Operation::Get, "state", Value::Null)?[0]
            .0
            .frame,
        [0x10, 17, 0, 9, 0]
    );
    let open_loop = cover_fixture(0x104, true, 4, 0, &[5])?;
    assert!(plan(&open_loop, Operation::Get, "state", Value::Null).is_err());
    assert!(plan(&open_loop, Operation::Set, "position", json!(55)).is_err());
    assert!(plan(&open_loop, Operation::Set, "state", json!("STOP")).is_ok());
    Ok(())
}
