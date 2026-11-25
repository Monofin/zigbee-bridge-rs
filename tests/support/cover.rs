//! Check percentage requests and authoritative device feedback across ASH and MQTT.
use super::{endpoint, incoming, short};
use anyhow::{Result, bail, ensure};
use rumqttc::{AsyncClient, Event, EventLoop, Incoming, QoS};
use serde_json::Value;
use zigbee_devices::cover::CLUSTER;
use zigbee_zcl::{Frame, FrameType};

pub const DEVICE: u16 = super::hvac::DEVICE + 1;

pub fn respond(args: &[u8]) -> Result<Vec<Vec<u8>>> {
    let frame = Frame::decode(&args[16..])?;
    let seq = frame.header.sequence;
    ensure!(
        args[1..3] == short(DEVICE).to_le_bytes()
            && args[3..9] == [0x5e, 0xc0, 2, 1, 1, endpoint(DEVICE)]
    );
    let body = match (
        frame.header.frame_type,
        frame.header.command_id,
        frame.payload,
    ) {
        // The mechanism stopped a percentage point away from the requested target.
        (FrameType::ClusterSpecific, 5, [38]) => vec![vec![8, seq, 0x0a, 8, 0, 0x20, 39]],
        (FrameType::General, 0, [9, 0]) => vec![
            vec![8, seq.wrapping_sub(1), 1, 9, 0, 0, 0x20, 9],
            vec![8, seq, 1, 9, 0, 0, 0x20, 72],
        ],
        _ => bail!("unexpected window covering frame"),
    };
    Ok(body
        .into_iter()
        .map(|bytes| {
            let mut cb = incoming(DEVICE, CLUSTER, &bytes);
            cb[6..8].copy_from_slice(&0xc05eu16.to_le_bytes());
            cb
        })
        .collect())
}

pub async fn verify(client: &AsyncClient, net: &mut EventLoop, base: &str) -> Result<()> {
    let state = format!("{base}/Shade");
    for (topic, request, key, expected) in [
        ("set", r#"{"position":62}"#, "position", 61),
        ("get", r#"{"tilt":""}"#, "tilt", 28),
    ] {
        client
            .publish(format!("{state}/{topic}"), QoS::AtLeastOnce, false, request)
            .await?;
        loop {
            let Event::Incoming(Incoming::Publish(p)) = net.poll().await? else {
                continue;
            };
            if p.topic == state {
                let body: Value = serde_json::from_slice(&p.payload)?;
                if let Some(n) = body.get(key) {
                    ensure!(
                        n.as_i64() == Some(expected),
                        "unexpected {key} on cover MQTT: {n}"
                    );
                    break;
                }
            }
        }
    }
    Ok(())
}
