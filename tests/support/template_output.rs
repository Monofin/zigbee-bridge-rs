//! HA 1.2 generic Level Controllable Output, with no model or manufacturer supplied.
use super::{endpoint, ieee, incoming, short};
use anyhow::{Result, ensure};
use rumqttc::{AsyncClient, Event, EventLoop, Incoming, QoS};
use serde_json::{Value, json};
use zigbee_bridge_rs::{
    bridge::Bindings,
    interview_log::{
        AttributeResult, Descriptor, Entry, IdentitySource, InterviewReport, Observation,
    },
};
use zigbee_zcl::{Frame, FrameType, foundation::Payload};

pub const DEVICE: u16 = super::equipment::DEVICE + 1;

pub fn bind(b: &mut Bindings) -> Result<()> {
    let ep = endpoint(DEVICE);
    let facts = vec![
        Observation::Target {
            ieee: format!("{:016x}", ieee(DEVICE)),
            short: short(DEVICE),
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
                profile: 0x104,
                device: 3,
                version: 1,
                servers: vec![0, 3, 4, 5, 6, 8],
                clients: vec![3],
            },
        },
        Observation::Attribute {
            sequence: 3,
            endpoint: ep,
            cluster: 8,
            attribute: 2,
            result: AttributeResult::Value {
                data_type: 0x20,
                bytes: vec![12],
            },
        },
        Observation::Attribute {
            sequence: 3,
            endpoint: ep,
            cluster: 8,
            attribute: 3,
            result: AttributeResult::Value {
                data_type: 0x20,
                bytes: vec![180],
            },
        },
        Observation::Finished,
    ];
    b.add(
        &InterviewReport {
            finished: true,
            entries: facts
                .into_iter()
                .map(|observation| Entry {
                    unix_ms: 0,
                    observation,
                })
                .collect(),
        },
        &[(ieee(DEVICE), "adjustable_output".into())]
            .into_iter()
            .collect(),
    )?;
    let cfg = b.discovery_snapshot("test", "ha", None)?;
    let id = format!("0x{:016x}", ieee(DEVICE));
    ensure!(cfg.contains_key(&format!("ha/switch/{id}/switch/config")));
    let level = &cfg[&format!("ha/number/{id}/level/config")];
    ensure!(level["min"] == 12. && level["max"] == 180. && level["step"] == 1.);
    ensure!(
        !cfg.keys()
            .any(|k| k.starts_with(&format!("ha/light/{id}/")))
    );
    Ok(())
}

#[derive(Default)]
pub struct Output {
    confirmed: Option<(u16, u8)>,
}
impl Output {
    pub(super) fn awaiting_readback(&self) -> bool {
        self.confirmed.is_some()
    }

    pub fn respond(&mut self, args: &[u8]) -> Result<Vec<Vec<u8>>> {
        let frame = Frame::decode(&args[16..])?;
        let cluster = u16::from_le_bytes(args[5..7].try_into()?);
        let seq = frame.header.sequence;
        let body = if frame.header.frame_type == FrameType::ClusterSpecific {
            ensure!(!frame.header.disable_default_response && self.confirmed.is_none());
            match cluster {
                6 => ensure!(frame.header.command_id == 1 && frame.payload.is_empty()),
                8 => ensure!(frame.header.command_id == 0 && frame.payload == [70, 0, 0, 0, 0]),
                _ => anyhow::bail!("unexpected output command"),
            }
            self.confirmed = Some((cluster, seq));
            vec![0x18, seq, 0x0b, frame.header.command_id, 0]
        } else {
            ensure!(matches!(Payload::decode(frame)?,Payload::ReadAttributes(ids) if ids==[0]));
            ensure!(matches!(self.confirmed.take(),Some((c,prev)) if c==cluster && prev!=seq));
            // One output stays off; the level actuator quantizes the requested level.
            if cluster == 6 {
                vec![0x18, seq, 1, 0, 0, 0, 0x10, 0]
            } else {
                vec![0x18, seq, 1, 0, 0, 0, 0x20, 64]
            }
        };
        let mut reply = incoming(DEVICE, cluster, &body);
        reply[11] = args[7];
        Ok(vec![reply])
    }
}

pub async fn verify(client: &AsyncClient, net: &mut EventLoop, base: &str) -> Result<()> {
    let topic = format!("{base}/adjustable_output");
    for (key, value, expected) in [
        ("state", json!("ON"), json!("OFF")),
        ("level", json!(70), json!(64)),
    ] {
        client
            .publish(
                format!("{topic}/set"),
                QoS::AtLeastOnce,
                false,
                serde_json::to_vec(&json!({(key):value}))?,
            )
            .await?;
        loop {
            if let Event::Incoming(Incoming::Publish(p)) = net.poll().await?
                && p.topic == topic
            {
                let body: Value = serde_json::from_slice(&p.payload)?;
                if let Some(actual) = body.get(key) {
                    ensure!(*actual == expected, "unconfirmed output {key}: {actual}");
                    break;
                }
            }
        }
    }
    Ok(())
}
