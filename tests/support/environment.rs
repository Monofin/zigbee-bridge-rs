//! One anonymous multisensor, including a device that clamps its target on write.
use super::{endpoint, ieee, incoming, short};
use anyhow::{Result, ensure};
use rumqttc::{AsyncClient, Event, EventLoop, Incoming, QoS};
use serde_json::{Value, json};
use zigbee_bridge_rs::{
    bridge::Bindings,
    interview_log::{Descriptor, Entry, IdentitySource, InterviewReport, Observation},
};
use zigbee_zcl::{Frame, foundation::Payload};

pub const DEVICE: u16 = super::meter::LAST + 1;

pub fn bind(bindings: &mut Bindings) -> Result<()> {
    let ep = endpoint(DEVICE);
    let observations = [
        Observation::Target {
            ieee: format!("{:016x}", ieee(DEVICE)),
            short: 1,
            identity_source: IdentitySource::Announcement,
        },
        Observation::ActiveEndpoints {
            sequence: 1,
            endpoints: vec![ep],
        },
        Observation::SimpleDescriptor {
            sequence: 2,
            endpoint: ep,
            descriptor: Descriptor {
                profile: 0x0104,
                device: 0xffff,
                version: 1,
                servers: vec![0x0400, 0x0401, 0x0404, 0x0406, 0x0408, 0x040d, 0x042a],
                clients: vec![],
            },
        },
        Observation::Finished,
    ];
    let report = InterviewReport {
        finished: true,
        entries: observations
            .into_iter()
            .map(|observation| Entry {
                unix_ms: 0,
                observation,
            })
            .collect(),
    };
    bindings.add(
        &report,
        &[(ieee(DEVICE), "environment".into())].into_iter().collect(),
    )?;
    let cfg = bindings.discovery_snapshot("test", "ha", None)?;
    for (domain, name) in [
        ("binary_sensor", "occupancy"),
        ("sensor", "illuminance"),
        ("sensor", "flow"),
        ("number", "illuminance_target"),
        ("sensor", "pm25_volume_percent"),
    ] {
        let c = &cfg[&format!("ha/{domain}/0x{:016x}/{name}/config", ieee(DEVICE))];
        ensure!(
            c["state_topic"] == "test/environment",
            "unexpected {name} state topic"
        );
    }
    Ok(())
}

pub fn respond(args: &[u8]) -> Result<Vec<Vec<u8>>> {
    let frame = Frame::decode(&args[16..])?;
    let cluster = u16::from_le_bytes(args[5..7].try_into()?);
    ensure!(args[1..3] == short(DEVICE).to_le_bytes() && args[8] == endpoint(DEVICE));
    let args_for = |zcl: &[u8]| {
        let mut msg = incoming(DEVICE, cluster, zcl);
        msg[5 + 6] = args[7]; // Remote response is addressed to the APS endpoint that sent the request.
        msg
    };
    let seq = frame.header.sequence;
    let mut response = vec![8, seq, 1];
    match Payload::decode(frame)? {
        Payload::WriteAttributes(rows) => {
            ensure!(
                cluster == 0x0401
                    && rows.len() == 1
                    && rows[0].id == 0x10
                    && rows[0].value.as_bytes() == 10000u16.to_le_bytes()
            );
            Ok(vec![args_for(&[8, seq, 4, 0])])
        }
        Payload::ReadAttributes(ids) => {
            let &[id] = ids.as_slice() else {
                anyhow::bail!("expected a single read")
            };
            response.extend(id.to_le_bytes());
            response.push(0);
            match (cluster, id) {
                (0x0401, 0x10) => response.extend([0x21, 0x20, 0x4e]), // 100 lx actually applied
                (0x0406, 0) => response.extend([0x18, 1]),
                (0x040d, 0) => {
                    response.push(0x39);
                    response.extend(0.0005_f32.to_le_bytes());
                }
                _ => anyhow::bail!("unexpected environmental request"),
            }
            let mut replies = vec![args_for(&response)];
            if cluster == 0x0406 {
                replies.push(args_for(&[8, 0xa1, 10, 0, 0, 0x18, 0]));
            }
            Ok(replies)
        }
        _ => anyhow::bail!("unexpected command"),
    }
}

pub async fn verify(client: &AsyncClient, net: &mut EventLoop, base: &str) -> Result<()> {
    let topic = format!("{base}/environment");
    for (op, key, value, expected) in [
        ("get", "occupancy", json!(""), json!(false)), // Later, unsolicited clear.
        ("get", "carbon_dioxide", json!(""), json!(500.0)),
        ("set", "illuminance_target", json!(10), json!(100.0)),
    ] {
        client
            .publish(
                format!("{topic}/{op}"),
                QoS::AtLeastOnce,
                false,
                serde_json::to_vec(&json!({(key):value}))?,
            )
            .await?;
        loop {
            if let Event::Incoming(Incoming::Publish(p)) = net.poll().await?
                && p.topic == topic
            {
                let state: Value = serde_json::from_slice(&p.payload)?;
                if let Some(actual) = state.get(key) {
                    if key == "occupancy" && actual == true {
                        continue;
                    }
                    if let Some(n) = expected.as_f64() {
                        ensure!(
                            (actual.as_f64().unwrap_or(f64::NAN) - n).abs() < 0.001,
                            "unexpected {key}: {actual}"
                        );
                    } else {
                        ensure!(actual == &expected, "unexpected {key}: {actual}");
                    }
                    break;
                }
            }
        }
    }
    Ok(())
}
