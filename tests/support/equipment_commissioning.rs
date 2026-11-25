//! The NCP returns fresh constraints, then acknowledges a single write and reports its applied value.
use anyhow::{Result, ensure};
use zigbee_zcl::{Frame, foundation::Payload};

#[derive(Default)]
pub enum Replay {
    #[default]
    Idle,
    AwaitingWrite(u16, u8),
    AwaitingReadback(u16, u8),
}

pub fn response(state: &mut Replay, cluster: u16, frame: Frame<'_>) -> Result<Vec<u8>> {
    let seq = frame.header.sequence;
    let (id, attribute_type, requested, confirmed): (u16, u8, &[u8], &[u8]) = match cluster {
        0x0100 => (0x10, 0x21, &[0xc2, 1], &[0xc0, 1]), // 450 requested, applied in motor steps of four.
        0x0301 => (0x10, 0x20, &[36], &[37]),
        0x0200 => (0x21, 0x30, &[1], &[1]),
        _ => anyhow::bail!("unexpected commissioning cluster"),
    };
    match Payload::decode(frame)? {
        Payload::WriteAttributes(rows) => {
            ensure!(
                rows.len() == 1
                    && rows[0].id == id
                    && rows[0].value.data_type().id() == attribute_type
                    && rows[0].value.as_bytes() == requested,
                "unexpected commissioning write"
            );
            ensure!(
                matches!(state,Replay::AwaitingWrite(c,previous) if *c==cluster && *previous!=seq),
                "write did not follow a fresh preflight"
            );
            *state = Replay::AwaitingReadback(cluster, seq);
            Ok(vec![0x18, seq, 4, 0])
        }
        Payload::ReadAttributes(ids) => {
            let mut reply = vec![0x18, seq, 1];
            let mut value = |id: u16, ty: u8, raw: &[u8]| {
                reply.extend(id.to_le_bytes());
                reply.extend([0, ty]);
                reply.extend(raw);
            };
            if ids == [id] {
                ensure!(
                    matches!(state,Replay::AwaitingReadback(c,previous) if *c==cluster && *previous!=seq),
                    "readback did not follow an acknowledged write"
                );
                *state = Replay::Idle;
                value(id, attribute_type, confirmed);
            } else {
                ensure!(
                    matches!(state, Replay::Idle),
                    "overlapping commissioning requests"
                );
                *state = Replay::AwaitingWrite(cluster, seq);
                match cluster {
                    0x0100 => {
                        ensure!(ids == [0, 2, 0x10, 0x11]);
                        value(0, 0x21, &600u16.to_le_bytes());
                        value(2, 0x18, &[1]);
                        value(0x10, 0x21, &390u16.to_le_bytes());
                        value(0x11, 0x30, &[0]);
                    }
                    0x0301 => {
                        ensure!(ids == [0, 1, 0x10, 0x11]);
                        for (a, v) in [(0, 10), (1, 200), (0x10, 20), (0x11, 180)] {
                            value(a, 0x20, &[v]);
                        }
                    }
                    0x0200 => {
                        ensure!(ids == [0x21, 0x12, 3, 4]);
                        value(0x21, 0x30, &[0]);
                        value(0x12, 0x30, &[0]);
                        value(3, 0x29, &(-50i16).to_le_bytes());
                        value(4, 0x29, &500i16.to_le_bytes());
                    }
                    _ => unreachable!(),
                }
            }
            Ok(reply)
        }
        _ => anyhow::bail!("unexpected equipment transaction"),
    }
}
