//! IAS Zone commissioning messages, ZCL R8 §8.2.2.1.3 and Table 8-10.
use super::CLUSTER;
use crate::{lighting::Request, standard_controls::request};
use anyhow::Result;
use zigbee_zcl::{
    Direction, Frame, FrameType,
    attributes::{AttributeValue, Status},
    cluster::{Command, IasZone},
    foundation::Payload,
    value::Value,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Assignment {
    pub enrolled: bool,
    pub cie: u64,
    pub id: u8,
}

pub fn inspect(ep: u8, seq: u8) -> Result<Request> {
    foundation(ep, seq, Payload::ReadAttributes(vec![0, 0x10, 0x11]))
}
pub fn write_cie(ep: u8, seq: u8, cie: u64) -> Result<Request> {
    let bytes = cie.to_le_bytes();
    foundation(
        ep,
        seq,
        Payload::WriteAttributes(vec![AttributeValue {
            id: 0x10,
            value: Value::from_bytes(0xf0.try_into()?, &bytes)?,
        }]),
    )
}
fn foundation(ep: u8, seq: u8, payload: Payload<'_>) -> Result<Request> {
    Ok(request(
        ep,
        seq,
        CLUSTER,
        FrameType::General,
        payload.command() as u8,
        &payload.encode()?,
    ))
}
pub fn response(ep: u8, seq: u8, code: u8, id: u8) -> Result<Request> {
    let command = Command::IasZone(IasZone::ZoneEnrollResponse {
        enroll_response_code: code,
        zone_id: id,
    });
    Ok(request(
        ep,
        seq,
        CLUSTER,
        FrameType::ClusterSpecific,
        command.key().command,
        &command.encode()?,
    ))
}

/// Accept exactly the three typed attributes requested by inspect.
pub fn ownership(frame: Frame<'_>) -> Option<Assignment> {
    if frame.header.direction != Direction::ServerToClient || frame.header.manufacturer.is_some() {
        return None;
    }
    let Payload::ReadAttributesResponse(rows) = Payload::decode(frame).ok()? else {
        return None;
    };
    let (mut state, mut cie, mut zone) = (None, None, None);
    for row in rows {
        let Status::Success(v) = row.result else {
            return None;
        };
        match (row.id, v.data_type().id(), v.as_bytes()) {
            (0, 0x30, [v @ 0..=1]) if state.is_none() => state = Some(*v == 1),
            (0x10, 0xf0, b) if cie.is_none() => cie = Some(u64::from_le_bytes(b.try_into().ok()?)),
            (0x11, 0x20, [v]) if zone.is_none() => zone = Some(*v),
            _ => return None,
        }
    }
    Some(Assignment {
        enrolled: state?,
        cie: cie?,
        id: zone?,
    })
}
