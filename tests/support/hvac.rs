//! A previously unknown heating-only thermostat with a low/high fan.
use super::{endpoint, ieee, incoming, short};
use anyhow::{Result, bail, ensure};
use rumqttc::{AsyncClient, Event, EventLoop, Incoming, QoS};
use serde_json::{Value, json};
use zigbee_bridge_rs::{
    bridge::Bindings,
    interview_log::{
        AttributeResult, Descriptor, Entry, IdentitySource, InterviewReport, Observation,
    },
};
use zigbee_zcl::Frame;

const CONSTRAINTS: &[u8] = &[
    0x11, 0, 0x12, 0, 0x15, 0, 0x16, 0, 0x17, 0, 0x18, 0, 0x19, 0, 0x1b, 0,
];

fn constraints(seq: u8, report: bool) -> Vec<u8> {
    let mut bytes = vec![8, seq, if report { 0x0a } else { 1 }];
    for (id, n) in [
        (0x11, 2400i16),
        (0x12, 2175),
        (0x15, 500),
        (0x16, 3000),
        (0x17, 1700),
        (0x18, 3200),
    ] {
        bytes.extend([id, 0]);
        if !report {
            bytes.push(0);
        };
        bytes.push(0x29);
        bytes.extend(n.to_le_bytes());
    }
    for (id, ty, v) in [(0x19, 0x28, 20), (0x1b, 0x30, 4)] {
        bytes.extend([id, 0]);
        if !report {
            bytes.push(0);
        }
        bytes.extend([ty, v]);
    }
    bytes
}

pub const DEVICE: u16 = super::enrolment::NEW + 1;
const NAME: &str = "test-thermostat";

pub fn bind(devices: &mut Bindings) -> Result<()> {
    let ep = endpoint(DEVICE);
    let mut records = vec![
        Observation::Target {
            ieee: format!("{:016x}", ieee(DEVICE)),
            short: 0x1111,
            identity_source: IdentitySource::Announcement,
        },
        Observation::ActiveEndpoints {
            sequence: 2,
            endpoints: vec![ep],
        },
        Observation::SimpleDescriptor {
            sequence: 3,
            endpoint: ep,
            descriptor: Descriptor {
                profile: 0x0104,
                device: 0x0301,
                version: 1,
                servers: vec![0, 0x201, 0x202],
                clients: vec![],
            },
        },
    ];
    let mut field = |cluster, attribute, ty, bytes| {
        records.push(Observation::Attribute {
            sequence: 4,
            endpoint: ep,
            cluster,
            attribute,
            result: AttributeResult::Value {
                data_type: ty,
                bytes,
            },
        })
    };
    field(0, 4, 0x42, b"\x08Test HVAC".to_vec());
    for (id, n) in [(0, 1999i16), (0x12, 2100), (0x15, 500), (0x16, 3000)] {
        field(0x201, id, 0x29, n.to_le_bytes().to_vec());
    }
    for (cluster, id, n) in [
        (0x201, 0x1b, 2),
        (0x201, 0x1c, 4),
        (0x202, 0, 1),
        (0x202, 1, 1),
    ] {
        field(cluster, id, 0x30, vec![n]);
    }
    records.push(Observation::Finished);
    let report = InterviewReport {
        finished: true,
        entries: records
            .into_iter()
            .map(|observation| Entry {
                unix_ms: 0,
                observation,
            })
            .collect(),
    };
    devices.add(
        &report,
        &[(ieee(DEVICE), NAME.to_string())].into_iter().collect(),
    )?;
    let snapshot = devices.discovery_snapshot("fixture", "ha", None)?;
    let climate = &snapshot[&format!("ha/climate/0x{:016x}/climate/config", ieee(DEVICE))];
    ensure!(
        climate["modes"] == json!(["off", "heat"])
            && climate["min_temp"] == 5.
            && climate["max_temp"] == 30.
    );
    ensure!(
        climate["mode_command_topic"] == "fixture/test-thermostat/set/system_mode"
            && climate.get("state_topic").is_none()
    );
    ensure!(
        snapshot
            .values()
            .any(|p| p["options"] == json!(["off", "low", "high"])
                && p["command_topic"] == "fixture/test-thermostat/set/fan_mode")
    );
    Ok(())
}

pub fn respond(args: &[u8]) -> Result<Vec<Vec<u8>>> {
    let cluster = u16::from_le_bytes(args[5..7].try_into()?);
    let f = Frame::decode(&args[16..])?;
    let seq = f.header.sequence;
    ensure!(
        args[1..3] == short(DEVICE).to_le_bytes()
            && args[7..9] == [1, endpoint(DEVICE)]
            && args[14] == seq
    );
    let bytes = match (cluster, f.header.command_id, f.payload) {
        (0x201, 2, [0x12, 0, 0x29, 0x98, 0x08]) => vec![8, seq, 4, 0], // 22.00 requested
        (0x201, 0, [0x12, 0]) => vec![8, seq, 1, 0x12, 0, 0, 0x29, 0x7f, 0x08], // device chose 21.75
        (0x201, 0, [0, 0]) => vec![8, seq, 1, 0, 0, 0, 0x29, 0xa7, 0x07],       // 19.59
        (0x201, 0, payload) if payload == CONSTRAINTS => constraints(seq, false),
        (0x201, 3, [0x12, 0, 0x29, 0xca, 8, 0x11, 0, 0x29, 0x92, 9]) => vec![8, seq, 4, 0], // atomic 22.50 / 24.50
        (0x201, 0, [0x11, 0, 0x12, 0]) => vec![
            8, seq, 1, 0x11, 0, 0, 0x29, 0x92, 9, 0x12, 0, 0, 0x29, 0xca, 8,
        ],
        (0x202, 2, [0, 0, 0x30, 3]) => vec![8, seq, 4, 0],
        (0x202, 0, [0, 0]) => vec![8, seq, 1, 0, 0, 0, 0x30, 3],
        _ => bail!(
            "unexpected HVAC wire request: cluster {cluster:#04x} payload {:?}",
            f.payload
        ),
    };
    let mut frames = Vec::new();
    if f.header.command_id == 0 {
        for wrong in [
            vec![0, seq, 1],
            vec![8, seq.wrapping_add(1), 1],
            vec![12, 0x34, 0x12, seq, 1],
        ] {
            let mut reply = wrong;
            reply.extend_from_slice(&bytes[3..]);
            *reply.last_mut().unwrap() = 0;
            frames.push(incoming(DEVICE, cluster, &reply));
        }
    }
    if cluster == 0x202 && f.header.command_id == 0 {
        // After the heating-only test, the thermostat changes its operating sequence.
        frames.push(incoming(DEVICE, 0x201, &constraints(seq, true)));
    }
    frames.push(incoming(DEVICE, cluster, &bytes));
    Ok(frames)
}
pub async fn verify(client: &AsyncClient, net: &mut EventLoop, base: &str) -> Result<()> {
    let topic = format!("{base}/{NAME}");
    for (op, name, payload, expected) in [
        ("set", "occupied_heating_setpoint", "22.00", json!(21.75)),
        ("get", "local_temperature", "", json!(19.59)),
        ("set", "fan_mode", "high", json!("high")),
    ] {
        client
            .publish(
                format!("{topic}/{op}/{name}"),
                QoS::AtLeastOnce,
                false,
                payload,
            )
            .await?;
        loop {
            let Event::Incoming(Incoming::Publish(p)) = net.poll().await? else {
                continue;
            };
            if p.topic != topic {
                continue;
            }
            let body: Value = serde_json::from_slice(&p.payload)?;
            if let Some(actual) = body.get(name) {
                ensure!(actual == &expected, "unexpected confirmed {name}: {actual}");
                break;
            }
        }
    }
    for proposed in [
        json!({"occupied_heating_setpoint":23.0}),
        json!({"occupied_heating_setpoint":22.5,"occupied_cooling_setpoint":24.5}),
    ] {
        client
            .publish(
                format!("{topic}/set"),
                QoS::AtLeastOnce,
                false,
                serde_json::to_vec(&proposed)?,
            )
            .await?;
    }
    let mut heat = false;
    let mut cool = false;
    while !(heat && cool) {
        if let Event::Incoming(Incoming::Publish(p)) = net.poll().await?
            && p.topic == topic
        {
            let value: Value = serde_json::from_slice(&p.payload)?;
            heat |= value["occupied_heating_setpoint"] == 22.5;
            cool |= value["occupied_cooling_setpoint"] == 24.5;
        }
    }
    Ok(())
}
