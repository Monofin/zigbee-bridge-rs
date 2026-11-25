//! Dispatch only incoming unicasts on the local OTA server endpoint. The
//! radio owner establishes the sender's IEEE identity before invoking this.
use crate::ota::{MALFORMED, Response, Server};
use anyhow::Result;
use zigbee_zcl::{
    Direction, Frame, FrameType, Header, PayloadError, attributes::DefaultResponse,
    cluster::Command, foundation::Payload,
};

pub fn reply(
    server: &mut Server,
    ieee: u64,
    endpoint: u8,
    bytes: &[u8],
) -> Result<Option<Vec<u8>>> {
    let Ok(frame) = Frame::decode(bytes) else {
        return Ok(None);
    };
    let h = frame.header;
    if h.frame_type != FrameType::ClusterSpecific
        || h.direction != Direction::ClientToServer
        || h.manufacturer.is_some()
    {
        return Ok(None);
    }
    // R8 §11.13.6.2.1 requires processing the known fields when later
    // field-control bits introduce more data at the end of a block request.
    let command = if h.command_id == 3
        && frame
            .payload
            .first()
            .is_some_and(|control| control & !3 != 0)
    {
        Command::decode_prefix(0x0019, frame).map(|(command, _)| command)
    } else {
        Command::decode(0x0019, frame)
    };
    let response = match command {
        Ok(Command::Ota(command)) => server.respond(ieee, endpoint, command)?,
        Err(PayloadError::UnsupportedClusterCommand(_)) => Response::Default(0x81),
        _ => Response::Default(MALFORMED),
    };
    let (frame_type, command_id, payload) = match response {
        Response::Ota(cmd) => {
            let cmd = Command::Ota(cmd);
            (FrameType::ClusterSpecific, cmd.key().command, cmd.encode()?)
        }
        Response::Default(0) if h.disable_default_response => return Ok(None),
        Response::Default(status) => (
            FrameType::General,
            0x0b,
            Payload::DefaultResponse(DefaultResponse {
                command_id: h.command_id,
                status,
            })
            .encode()?,
        ),
    };
    let response = Frame {
        header: Header {
            frame_type,
            manufacturer: None,
            direction: Direction::ServerToClient,
            disable_default_response: true,
            sequence: h.sequence,
            command_id,
        },
        payload: &payload,
    };
    let mut out = Vec::new();
    response.encode_into(&mut out);
    Ok(Some(out))
}
