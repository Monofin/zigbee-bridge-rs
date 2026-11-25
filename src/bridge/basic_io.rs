//! Retire reads and read back accepted PresentValue writes on the same member.
use super::{
    Identity, Planned, Read,
    ember::{Pending, submitted},
};
use crate::mqtt::{Command, Operation, Publisher};
use anyhow::{Result, ensure};
use std::collections::BTreeMap;
use tokio::time::Instant;
use zigbee_devices::{
    basic_io::{Channel, PRESENT},
    lighting::Request,
};
use zigbee_zcl::{
    Direction, Frame,
    attributes::{Status, WriteStatus},
    foundation::Payload,
};

pub(super) fn plan(
    channels: &[Channel],
    endpoint: u8,
    seq: u8,
    cmd: &Command,
) -> Result<Option<Vec<Planned>>> {
    let Some((channel, value)) = channels
        .iter()
        .find_map(|c| cmd.properties.get(c.layout.property()).map(|v| (c, v)))
    else {
        return Ok(None);
    };
    ensure!(
        cmd.properties.len() == 1,
        "address one I/O object per request"
    );
    let write = cmd.operation == Operation::Set;
    let mut frame = vec![0x10, seq, if write { 2 } else { 0 }];
    if write {
        frame.extend(channel.write(value)?);
    } else {
        frame.extend(PRESENT.to_le_bytes());
    }
    let cluster = channel.layout.cluster();
    Ok(Some(vec![(
        Request {
            endpoint,
            cluster,
            frame,
        },
        Some(Read::BasicIo { cluster, write }),
    )]))
}

pub(super) fn service_report(channel: &mut Channel, frame: Frame<'_>) {
    if frame.header.direction == Direction::ServerToClient
        && frame.header.manufacturer.is_none()
        && let Ok(Payload::ReportAttributes(records)) = Payload::decode(frame)
        && let Some(r) = records.iter().find(|r| r.id == 0x51)
    {
        channel.service_report(r.value);
    }
}

pub(super) fn receive(
    channel: &Channel,
    name: &str,
    out: &Publisher,
    pending: &mut BTreeMap<u8, Pending>,
    peer: Identity,
    short: u16,
    frame: Frame<'_>,
) -> Result<bool> {
    if frame.header.direction != Direction::ServerToClient || frame.header.manufacturer.is_some() {
        return Ok(false);
    }
    let key = channel.layout.property();
    let seq = frame.header.sequence;
    let op = pending.get(&seq).and_then(|p| match p.read {
        Some(Read::BasicIo { cluster, write })
            if (p.peer, p.short, p.cluster) == (peer, short, cluster)
                && cluster == channel.layout.cluster() =>
        {
            Some(write)
        }
        _ => None,
    });
    let value = match (Payload::decode(frame), op) {
        (Ok(Payload::ReportAttributes(rows)), _) => {
            let mut found = rows.into_iter().filter(|r| r.id == PRESENT);
            let Some(r) = found.next() else {
                return Ok(false);
            };
            if found.next().is_some() {
                return Ok(false);
            }
            channel.decode(r.value)
        }
        (Ok(Payload::ReadAttributesResponse(rows)), Some(false)) => {
            let [r] = rows.as_slice() else {
                return Ok(false);
            };
            if r.id != PRESENT {
                return Ok(false);
            }
            match r.result {
                Status::Success(v) => {
                    let Some(decoded) = channel.decode(v) else {
                        return Ok(false);
                    };
                    pending.remove(&seq);
                    Some(decoded)
                }
                Status::Error(_) => {
                    pending.remove(&seq);
                    return Ok(true);
                }
            }
        }
        (Ok(Payload::WriteAttributesResponse(status)), Some(true)) => {
            match status {
                WriteStatus::Success => {
                    let slot = pending.get_mut(&seq).unwrap();
                    slot.read = Some(Read::Refresh(key));
                    slot.until = Instant::now();
                }
                WriteStatus::Failures(rows) if rows.len() == 1 && rows[0].key == PRESENT => {
                    pending.remove(&seq);
                }
                _ => return Ok(false),
            }
            return Ok(true);
        }
        _ => return Ok(false),
    };
    if let Some(v) = value {
        let mut fields = serde_json::Map::new();
        if let Some(label_key) = channel.label_property() {
            fields.insert(label_key.into(), channel.label(&v).into());
        }
        fields.insert(key.into(), v);
        submitted(out.state_fields(name, key, fields))?;
        return Ok(true);
    }
    Ok(false)
}
