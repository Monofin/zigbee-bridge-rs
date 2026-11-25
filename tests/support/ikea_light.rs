//! An RGB lamp whose other Level Control Options bits start set.
use super::{COUNT, endpoint, incoming, short};
use anyhow::{Result, bail, ensure};
use rumqttc::{AsyncClient, Event, EventLoop, Incoming, QoS};
use serde_json::{Value, json};
use zigbee_zcl::Frame;

#[derive(Default)]
pub struct Lamp {
    configured: bool,
    config_write: Option<u8>,
}

impl Lamp {
    pub fn command(&mut self, args: &[u8]) -> Result<Vec<Vec<u8>>> {
        let cluster = u16::from_le_bytes(args[5..7].try_into()?);
        ensure!(args[1..3] == short(COUNT).to_le_bytes() && args[7..9] == [1, endpoint(COUNT)]);
        let frame = Frame::decode(&args[16..])?;
        let tag = frame.header.sequence;
        ensure!(args[14] == tag);
        let (cmd, payload) = match (cluster, frame.header.command_id, frame.payload) {
            (8, 0, [15, 0]) => (
                1,
                vec![15, 0, 0, 0x18, if self.configured { 0x83 } else { 0x82 }],
            ),
            (8, 2, [15, 0, 0x18, 0x83, 0, 0x40, 0x20, 255]) => {
                self.configured = true;
                self.config_write=Some(tag);
                (4, vec![0])
            }
            // The stub acknowledges the write but independently reports a
            // different startup value; optimistic publication must never win.
            (8, 0, [15,0,0,0x40]) if self.configured => {
                ensure!(self.config_write.take().is_some_and(|write| write != tag),"reused completed level-config write TSN");
                (1, vec![15,0,0,0x18,0x83,0,0x40,0,0x20,77])
            }
            (8, 0, [0, 0x40]) if self.configured => (1, vec![0, 0x40, 0, 0x20, 77]),
            (0x300, 7, [100, 97, 114, 96, 0, 0, 0, 0]) => {
                (10, vec![3, 0, 0x21, 100, 97, 4, 0, 0x21, 114, 96])
            }
            (0x300, 0, [8, 0, 3, 0, 4, 0]) => {
                (1, vec![3, 0, 0, 0x21, 100, 97, 4, 0, 0, 0x21, 114, 96])
            }
            _ => bail!("unhandled IKEA configuration request"),
        };
        let mut response = vec![8, tag, cmd];
        response.extend(payload);
        Ok(vec![incoming(COUNT, cluster, &response)])
    }
}

pub async fn verify(client: &AsyncClient, net: &mut EventLoop, base: &str) -> Result<()> {
    let topic = format!("{base}/lamp-{COUNT}");
    let set = format!("{topic}/set");
    client
        .publish(
            &set,
            QoS::AtLeastOnce,
            false,
            json!({"level_config":{"current_level_startup":"previous","execute_if_off":true}})
                .to_string(),
        )
        .await?;
    for phase in 0..3 {
        loop {
            let Event::Incoming(Incoming::Publish(p)) = net.poll().await? else {
                continue;
            };
            if p.topic != topic {
                continue;
            }
            let state: Value = serde_json::from_slice(&p.payload)?;
            if phase == 0 {
                let Some(level) = state.get("level_config") else {
                    continue;
                };
                ensure!(
                    level["current_level_startup"] != "previous",
                    "write published an unconfirmed startup level"
                );
                if level != &json!({"execute_if_off":true,"current_level_startup":77}) {
                    continue;
                }
            } else {
                let Some(temp) = state.get("color_temp") else {
                    continue;
                };
                ensure!(
                    temp.as_u64().is_some_and(|n| (248..=252).contains(&n))
                        && state["color_mode"] == "xy"
                );
            }
            break;
        }
        match phase {
            0 => {
                client
                    .publish(
                        &set,
                        QoS::AtLeastOnce,
                        false,
                        br#"{"color_temp":250}"#.as_slice(),
                    )
                    .await?
            }
            1 => {
                client
                    .publish(
                        format!("{topic}/get"),
                        QoS::AtLeastOnce,
                        false,
                        br#"{"color_temp":""}"#.as_slice(),
                    )
                    .await?
            }
            _ => {}
        }
    }
    Ok(())
}
