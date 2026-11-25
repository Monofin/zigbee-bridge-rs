//! TS0041 one-button scene switches. The reviewed manufacturer list comes
//! from the fingerprinted white labels of the pinned Z2M TS0041 definition.
//! Its untagged 0xfd command is vendor-specific even with a standard header.
use crate::Endpoint;
use zigbee_zcl::{Direction, Frame, FrameType};

const MANUFACTURERS: &str = include_str!("../../profiles/ts0041-manufacturers.txt");

#[derive(Clone)]
pub struct Button {
    endpoint: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ButtonAction {
    Single,
    Double,
    Hold,
}

impl ButtonAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Single => "single",
            Self::Double => "double",
            Self::Hold => "hold",
        }
    }
}

impl Button {
    pub fn resolve(manufacturer: &str, model: &str, ep: &Endpoint<'_>) -> Option<Self> {
        (model == "TS0041"
            && MANUFACTURERS.lines().any(|s| s == manufacturer)
            && ep.id == 1
            && ep.profile == 0x0104
            && ep.servers.contains(&0)
            && ep.servers.contains(&6))
        .then_some(Self { endpoint: ep.id })
    }

    /// The caller supplies a frame from this membership on the HA application
    /// endpoint. No last-action cache: two presses can have the same value.
    pub fn action(
        &self,
        profile: u16,
        cluster: u16,
        src_endpoint: u8,
        dest_endpoint: u8,
        frame: Frame<'_>,
    ) -> Option<ButtonAction> {
        if profile != 0x0104
            || cluster != 6
            || src_endpoint != self.endpoint
            || dest_endpoint != 1
            || frame.header.frame_type != FrameType::ClusterSpecific
            || frame.header.direction != Direction::ClientToServer
            || frame.header.manufacturer.is_some()
            || frame.header.command_id != 0xfd
        {
            return None;
        }
        match frame.payload {
            [0] => Some(ButtonAction::Single),
            [1] => Some(ButtonAction::Double),
            [2] => Some(ButtonAction::Hold),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_captured_gestures_and_rejects_other_commands() {
        let ep = Endpoint {
            id: 1,
            profile: 0x104,
            servers: &[0, 1, 6, 0xe000],
            clients: &[0x19, 0xa],
        };
        let button = Button::resolve("_TZ3000_qgwcxxws", "TS0041", &ep).unwrap();
        let action = |bytes: &[u8]| button.action(0x104, 6, 1, 1, Frame::decode(bytes).unwrap());
        assert_eq!(action(&[1, 145, 253, 0]), Some(ButtonAction::Single));
        assert_eq!(action(&[1, 146, 253, 1]), Some(ButtonAction::Double));
        assert_eq!(action(&[1, 148, 253, 2]), Some(ButtonAction::Hold));
        for bytes in [
            &[1, 1, 253, 3][..],
            &[1, 1, 253],
            &[1, 1, 253, 0, 0],
            &[9, 1, 253, 0],
            &[0, 1, 253, 0],
            &[5, 0, 0, 1, 253, 0],
            &[1, 1, 0, 0],
        ] {
            assert_eq!(action(bytes), None);
        }
        let frame = Frame::decode(&[1, 1, 253, 1]).unwrap();
        for (profile, cluster, src, dest) in
            [(0, 6, 1, 1), (260, 8, 1, 1), (260, 6, 2, 1), (260, 6, 1, 0)]
        {
            assert_eq!(button.action(profile, cluster, src, dest, frame), None);
        }
        assert!(Button::resolve("_TZ3000_qgwcxxws", "TS0041A", &ep).is_none());
        assert!(Button::resolve("unknown", "TS0041", &ep).is_none());
        assert!(
            Button::resolve(
                "_TZ3000_qgwcxxws",
                "TS0041",
                &Endpoint {
                    servers: &[0],
                    ..ep
                }
            )
            .is_none()
        );
    }
}
