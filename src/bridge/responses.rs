//! ZCL R8 §2.5.12.2. Dispatch decides the status; delivery and the request
//! header decide whether that status belongs on the wire.
use crate::{
    ash::Ezsp,
    interview::{Incoming, supports_cluster},
};
use anyhow::Result;
use zigbee_zcl::{
    Direction, Frame, FrameType, Header,
    foundation::{GeneralCommand, Payload},
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Outcome {
    Success,
    Failure,
    Malformed,
    UnsupportedCommand,
    UnsupportedCluster,
}

impl Outcome {
    pub(super) fn initial(cluster: u16, frame: Frame<'_>) -> Self {
        if !supports_cluster(cluster, frame.header.direction) {
            return Self::UnsupportedCluster;
        }
        use GeneralCommand::*;
        match frame.header.general_command() {
            Some(
                ReportAttributes
                | ReadAttributesResponse
                | WriteAttributesResponse
                | ConfigureReportingResponse
                | ReadReportingConfigurationResponse,
            ) if frame.header.manufacturer.is_none() => {
                if Payload::decode(frame).is_ok() {
                    Self::Success
                } else {
                    Self::Malformed
                }
            }
            _ => Self::UnsupportedCommand,
        }
    }

    fn default_reply(self, kind: u8, frame: Frame<'_>) -> Option<Vec<u8>> {
        if !matches!(kind, 0 | 1)
            || matches!(
                frame.header.general_command(),
                Some(GeneralCommand::DefaultResponse | GeneralCommand::WriteAttributesNoResponse)
            )
        {
            return None;
        }
        let status = match self {
            Self::Success if frame.header.disable_default_response => return None,
            Self::Success => 0,
            Self::Failure => 1,
            Self::Malformed => 0x80,
            Self::UnsupportedCommand => 0x81,
            Self::UnsupportedCluster => 0xc3,
        };
        Some(reply(
            frame.header,
            GeneralCommand::DefaultResponse,
            &[frame.header.command_id, status],
        ))
    }
}

pub(super) fn reply(request: Header, command: GeneralCommand, payload: &[u8]) -> Vec<u8> {
    let direction = match request.direction {
        Direction::ClientToServer => Direction::ServerToClient,
        Direction::ServerToClient => Direction::ClientToServer,
    };
    let mut wire = Vec::new();
    Frame {
        header: Header {
            frame_type: FrameType::General,
            direction,
            command_id: command as u8,
            disable_default_response: true,
            ..request
        },
        payload,
    }
    .encode_into(&mut wire);
    wire
}

pub(super) async fn send(link: &mut Ezsp<'_>, target: &crate::ash::Destination, msg: &Incoming<'_>, wire: &[u8]) -> Result<()> {
    let aps = &msg.aps;
    let status = if msg.aps_encrypted {
        super::zdo::submit_protected_unicast(link, target, aps.profile, aps.cluster, (aps.dst,aps.src), 0xff, wire).await?
    } else { super::zdo::submit_unicast(
        link,
        target,
        aps.profile,
        aps.cluster,
        (aps.dst, aps.src),
        0xff,
        wire,
    )
    .await? };
    if status != 0 {
        eprintln!("ZCL response submission failed: Ember {status:#04x}");
    }
    Ok(())
}

pub(super) async fn finish(
    link: &mut Ezsp<'_>,
    target: &crate::ash::Destination,
    msg: &Incoming<'_>,
    frame: Frame<'_>,
    outcome: Outcome,
) -> Result<()> {
    if let Some(wire) = outcome.default_reply(msg.kind, frame) {
        send(link, target, msg, &wire).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn status_and_delivery_determine_default_response() {
        let f = Frame::decode(&[0x0c, 0x34, 0x12, 70, 10, 1, 0, 0x10, 1]).unwrap();
        assert_eq!(
            Outcome::Success.default_reply(0, f).unwrap(),
            [0x14, 0x34, 0x12, 70, 11, 10, 0]
        );
        for kind in [2, 3, 4, 5] {
            assert!(Outcome::Malformed.default_reply(kind, f).is_none());
        }
        let disabled = Frame::decode(&[0x18, 70, 10, 1]).unwrap();
        let invalid = Outcome::initial(6, disabled);
        assert_eq!(
            invalid.default_reply(0, disabled).unwrap(),
            [0x10, 70, 11, 10, 0x80]
        );
        assert!(Outcome::Success.default_reply(0, disabled).is_none());
        let unknown = Frame::decode(&[0x11, 9, 0xfb]).unwrap();
        for (cluster, status) in [(6, 0x81), (0xffff, 0xc3)] {
            let outcome = Outcome::initial(cluster, unknown);
            assert_eq!(outcome.default_reply(0, unknown).unwrap(), [0x18, 9, 11, 0xfb, status]);
        }
        for cmd in [5, 11] {
            let bytes = [0, 70, cmd];
            assert!(
                Outcome::UnsupportedCluster
                    .default_reply(0, Frame::decode(&bytes).unwrap())
                    .is_none()
            );
        }
    }
}
