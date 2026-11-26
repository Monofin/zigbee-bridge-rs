//! Anonymous I/O panel and an independent client-only remote on the broker replay.
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
use zigbee_zcl::{Frame, foundation::Payload};

pub const PANEL: u16 = super::environment::DEVICE + 1;
pub const REMOTE: u16 = PANEL + 1;
const BINARY: u16 = 0x0010;
const PRESENT: u16 = 0x0055;

pub fn bind(bindings: &mut Bindings) -> Result<()> {
    for (device, name, servers, clients) in [
        (PANEL, "panel", vec![BINARY], vec![]),
        (REMOTE, "remote", vec![], vec![6, 8]),
    ] {
        let ep = endpoint(device);
        let mut observations = vec![
            Observation::Target {
                ieee: format!("{:016x}", ieee(device)),
                short: short(device),
                identity_source: IdentitySource::Announcement,
            },
            Observation::ActiveEndpoints {
                sequence: 0,
                endpoints: vec![ep],
            },
            Observation::SimpleDescriptor {
                sequence: 1,
                endpoint: ep,
                descriptor: Descriptor {
                    profile: 0x104,
                    device: 0xfffe,
                    version: 1,
                    servers,
                    clients,
                },
            },
        ];
        if device == PANEL {
            observations.extend([0x51, PRESENT].map(|attribute| Observation::Attribute {
                sequence: 2,
                endpoint: ep,
                cluster: BINARY,
                attribute,
                result: AttributeResult::Value {
                    data_type: 0x10,
                    bytes: vec![0],
                },
            }));
            observations.push(Observation::AttributeAccess {
                endpoint: ep,
                cluster: BINARY,
                attribute: PRESENT,
                data_type: 0x10,
                access: 3,
            });
        }
        observations.push(Observation::Finished);
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
            &[(ieee(device), name.into())].into_iter().collect(),
        )?;
    }
    let configs = bindings.discovery_snapshot("test", "ha", None)?;
    ensure!(
        configs[&format!("ha/switch/0x{:016x}/binary_output/config", ieee(PANEL))]["command_topic"]
            == "test/panel/set/binary_output"
    );
    let triggers: Vec<_> = configs
        .iter()
        .filter(|(k, _)| k.starts_with(&format!("ha/device_automation/0x{:016x}/", ieee(REMOTE))))
        .collect();
    ensure!(triggers.len() == 2 && triggers.iter().all(|(_, v)| v["topic"] == format!("test/bridge/feedback/remote/zcl_{:02x}_command", endpoint(REMOTE))));
    ensure!(triggers.iter().any(|(_, v)| {
        v["payload"] == "6"
            && v["value_template"]
                .as_str()
                .unwrap()
                .contains("zcl_04_command")
    }));
    Ok(())
}

pub fn respond(args: &[u8]) -> Result<Vec<Vec<u8>>> {
    let frame = Frame::decode(&args[16..])?;
    ensure!(
        args[1..3] == short(PANEL).to_le_bytes()
            && args[5..7] == BINARY.to_le_bytes()
            && args[7..9] == [2, endpoint(PANEL)]
    );
    let seq = frame.header.sequence;
    let response = |zcl: &[u8]| {
        let mut msg = incoming(PANEL, BINARY, zcl);
        msg[5 + 6] = args[7];
        msg
    };
    Ok(match Payload::decode(frame)? {
        Payload::WriteAttributes(rows) => {
            ensure!(
                rows.len() == 1
                    && rows[0].id == PRESENT
                    && rows[0].value.data_type().id() == 0x10
                    && rows[0].value.as_bytes() == [1]
            );
            vec![response(&[0x18, seq, 4, 0])]
        }
        Payload::ReadAttributes(ids) => {
            ensure!(ids == [PRESENT]);
            // The BACnet priority array retained another commander's OFF.
            let mut replies = vec![response(&[0x18, seq, 1, 0x55, 0, 0, 0x10, 0])];
            // The remote's two distinct Toggle events must both reach MQTT.
            for (n, c, payload) in [(1, 6, &[2][..]), (2, 6, &[2][..]), (3, 8, &[1, 1, 32][..])] {
                let zcl = [&[0x11, n], payload].concat();
                replies.push(incoming(REMOTE, c, &zcl));
            }
            replies
        }
        _ => anyhow::bail!("unexpected panel command"),
    })
}

pub async fn verify(client: &AsyncClient, net: &mut EventLoop, base: &str) -> Result<()> {
    let panel = format!("{base}/panel");
    let remote = format!("{base}/remote");
    client
        .publish(
            format!("{panel}/set/binary_output"),
            QoS::AtLeastOnce,
            false,
            "ON",
        )
        .await?;
    let mut confirmed = false;
    let mut events = Vec::new();
    while !confirmed || events.len() < 3 {
        let Event::Incoming(Incoming::Publish(p)) = net.poll().await? else {
            continue;
        };
        if p.topic == panel {
            let state: Value = serde_json::from_slice(&p.payload)?;
            if let Some(actual) = state.get("binary_output") {
                ensure!(actual == false, "unconfirmed write was published");
                confirmed = true;
            }
        }
        if p.topic == remote {
            let state: Value = serde_json::from_slice(&p.payload)?;
            if let Some(v) = state.get("zcl_04_command") {
                ensure!(!p.retain);
                events.push(v.clone());
            }
        }
    }
    ensure!(
        events
            == [
                json!({"cluster":6,"command":2,"name":"Toggle"}),
                json!({"cluster":6,"command":2,"name":"Toggle"}),
                json!({"cluster":8,"command":1,"name":"Move","move_mode":1,"rate":32})
            ],
        "unexpected remote commands {events:?}"
    );
    Ok(())
}
