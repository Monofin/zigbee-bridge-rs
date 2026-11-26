//! Two anonymous electricity meters. Only the first also implements Electrical Measurement.
use super::{endpoint, ieee, incoming, short};
use anyhow::{Result, ensure};
use rumqttc::{AsyncClient, Event, EventLoop, Incoming, QoS};
use serde_json::Value;
use zigbee_bridge_rs::{
    bridge::Bindings,
    interview_log::{
        AttributeResult, Descriptor, Entry, IdentitySource, InterviewReport, Observation,
    },
};
use zigbee_devices::metering::{ELECTRICAL_MEASUREMENT as AC, METERING};
use zigbee_zcl::{Frame, foundation::Payload};

pub const FIRST: u16 = super::cover::DEVICE + 1;
pub const LAST: u16 = FIRST + 1;

fn fields(device: u16, cluster: u16) -> Vec<(u16, u8, Vec<u8>)> {
    match cluster {
        AC if device == FIRST => [
            (0x600, 0x21, 1i16),
            (0x601, 0x21, 10),
            (0x505, 0x21, 2301),
            (0x602, 0x21, 1),
            (0x603, 0x21, 1000),
            (0x508, 0x21, 111),
            (0x604, 0x21, 3),
            (0x605, 0x21, 2),
            (0x50b, 0x29, -7),
        ]
        .into_iter()
        .map(|(id, ty, n)| (id, ty, n.to_le_bytes().to_vec()))
        .collect(),
        METERING => vec![
            (0x300, 0x30, vec![0]),
            (0x301, 0x22, vec![1, 0, 0]),
            (
                0x302,
                0x22,
                vec![if device == FIRST { 10 } else { 100 }, 0, 0],
            ),
            (0, 0x25, vec![13, 0, 0, 0, 0, 0]),
            (0x400, 0x2a, vec![0xfd, 0xff, 0xff]),
        ],
        _ => Vec::new(),
    }
}

pub fn bind(bindings: &mut Bindings) -> Result<()> {
    for device in FIRST..=LAST {
        let ep = endpoint(device);
        let clusters = if device == FIRST {
            vec![AC, METERING]
        } else {
            vec![METERING]
        };
        let mut records = vec![
            Observation::Target {
                ieee: format!("{:016x}", ieee(device)),
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
                    profile: 0x104,
                    device: 0x0053,
                    version: 1,
                    servers: clusters.clone(),
                    clients: vec![],
                },
            },
        ];
        for cluster in clusters {
            for (attribute, data_type, bytes) in fields(device, cluster) {
                records.push(Observation::Attribute {
                    sequence: 3,
                    endpoint: ep,
                    cluster,
                    attribute,
                    result: AttributeResult::Value { data_type, bytes },
                });
            }
        }
        records.push(Observation::Finished);
        let mut report = InterviewReport {
            finished: true,
            entries: records
                .into_iter()
                .map(|observation| Entry {
                    unix_ms: 0,
                    observation,
                })
                .collect(),
        };
        let names = [(ieee(device), format!("meter-{device}"))]
            .into_iter()
            .collect();
        let snapshot = |b: &Bindings| -> Result<_> {
            let snapshot = b.discovery_snapshot("fixture", "ha", None)?;
            let energy = snapshot
                .values()
                .find(|p| {
                    p["state_topic"] == format!("fixture/bridge/feedback/meter-{device}/energy")
                        && p["device_class"] == "energy"
                })
                .ok_or_else(|| anyhow::anyhow!("missing cumulative energy discovery"))?;
            ensure!(
                energy["state_class"] == "total_increasing"
                    && energy["unit_of_measurement"] == "kWh"
            );
            Ok(energy.clone())
        };
        bindings.add(&report, &names)?;
        let identity = snapshot(bindings)?;
        let Observation::Target { short, .. } = &mut report.entries[0].observation else {
            anyhow::bail!("missing meter identity")
        };
        *short = 0x719b;
        bindings.add(&report, &names)?;
        ensure!(
            identity == snapshot(bindings)?,
            "discovery identity changed on re-interview"
        );
    }
    Ok(())
}

pub fn respond(device: u16, args: &[u8]) -> Result<Vec<Vec<u8>>> {
    let cluster = u16::from_le_bytes(args[5..7].try_into()?);
    ensure!(args[1..3] == short(device).to_le_bytes() && args[7..9] == [1, endpoint(device)]);
    let frame = Frame::decode(&args[16..])?;
    let Payload::ReadAttributes(ids) = Payload::decode(frame)? else {
        anyhow::bail!("expected meter read")
    };
    let fields = fields(device, cluster);
    let mut response = vec![8, frame.header.sequence, 1];
    for id in ids.iter().rev() {
        let (_, ty, bytes) = fields
            .iter()
            .find(|(a, _, _)| a == id)
            .ok_or_else(|| anyhow::anyhow!("unsupported meter read"))?;
        response.extend(id.to_le_bytes());
        response.extend([0, *ty]);
        response.extend(bytes);
    }
    let mut replies = vec![incoming(device, cluster, &response)];
    if device == FIRST && cluster == AC && ids.contains(&0x505) {
        // An independent cumulative report changes its divisor after the raw count.
        replies.push(incoming(
            device,
            METERING,
            &[
                8, 94, 10, 0, 0, 0x25, 26, 0, 0, 0, 0, 0, 2, 3, 0x22, 5, 0, 0,
            ],
        ));
    }
    Ok(replies)
}

pub async fn verify(client: &AsyncClient, net: &mut EventLoop, base: &str) -> Result<()> {
    for (device, key, expected) in [
        (FIRST, "energy", 1.3),
        (LAST, "energy", 0.13),
        (FIRST, "power", -10.5),
        (LAST, "power", -30.0),
        (FIRST, "current", 0.111),
        (FIRST, "voltage", 230.1),
    ] {
        let topic = format!("{base}/meter-{device}");
        client
            .publish(
                format!("{topic}/get"),
                QoS::AtLeastOnce,
                false,
                serde_json::to_vec(&serde_json::json!({(key):""}))?,
            )
            .await?;
        let mut observed = false;
        let mut report_seen = key != "voltage";
        while !(observed && report_seen) {
            let Event::Incoming(Incoming::Publish(p)) = net.poll().await? else {
                continue;
            };
            if p.topic == topic {
                let body: Value = serde_json::from_slice(&p.payload)?;
                if let Some(n) = body.get(key) {
                    ensure!(n.as_f64() == Some(expected), "unexpected {key} value {n}");
                    observed = true;
                }
                report_seen |= body["energy"].as_f64() == Some(5.2);
            }
        }
    }
    Ok(())
}
