//! IAS ACE read-only CIE responses and zone pagination, ZCL R8 §8.3.2.
use std::collections::BTreeMap;
use zigbee_zcl::{Direction, Frame, FrameType, Header, foundation::GeneralCommand};

pub const CLUSTER: u16 = 0x0501;

const PANEL_STATES: &[&str] = &[
    "disarmed",
    "armed_stay",
    "armed_night",
    "armed_away",
    "exit_delay",
    "entry_delay",
    "not_ready",
    "in_alarm",
    "arming_stay",
    "arming_night",
    "arming_away",
];
pub struct PanelStatus {
    pub state: &'static str,
    pub seconds_remaining: u8,
    pub audible_notification: u8,
    pub alarm: Option<&'static str>,
}

impl PanelStatus {
    /// R8 §8.3.2.4.5–6. Notifications are unsolicited; poll responses need a matching transaction.
    pub fn decode(frame: Frame<'_>, awaiting: bool) -> Option<Self> {
        let h = frame.header;
        if h.frame_type != FrameType::ClusterSpecific
            || h.direction != Direction::ServerToClient
            || h.manufacturer.is_some()
            || !(h.command_id == 4 || h.command_id == 5 && awaiting)
        {
            return None;
        }
        let [state, seconds_remaining, audible_notification, alarm] = *frame.payload else {
            return None;
        };
        if !matches!(audible_notification, 0 | 1 | 0x80..=0xff) {
            return None;
        }
        let name = *PANEL_STATES.get(usize::from(state))?;
        const ALARMS: &[&str] = &[
            "none",
            "burglar",
            "fire",
            "emergency",
            "police_panic",
            "fire_panic",
            "emergency_panic",
        ];
        let reason = *ALARMS.get(usize::from(alarm))?;
        Some(Self {
            state: name,
            seconds_remaining,
            audible_notification,
            alarm: (state == 7 || alarm == 0).then_some(reason),
        })
    }
}

pub struct Zone {
    pub ieee: u64,
    pub kind: Option<u16>,
    pub status: Option<u16>,
}

fn response(h: Header, command: u8, payload: &[u8]) -> Vec<u8> {
    let mut wire = Vec::new();
    Frame {
        header: Header {
            direction: Direction::ServerToClient,
            disable_default_response: true,
            command_id: command,
            ..h
        },
        payload,
    }
    .encode_into(&mut wire);
    wire
}

fn error(h: Header, status: u8) -> Vec<u8> {
    response(
        Header {
            frame_type: FrameType::General,
            ..h
        },
        GeneralCommand::DefaultResponse as u8,
        &[h.command_id, status],
    )
}

// Only check the length and UTF-8 framing. The credential stays in the incoming
// slice and is never returned, formatted, published or retained.
fn credential_tail(bytes: &[u8]) -> Option<&[u8]> {
    let (&n, rest) = bytes.split_first()?;
    if n == 255 {
        return None;
    }
    let (credential, tail) = rest.split_at_checked(usize::from(n))?;
    std::str::from_utf8(credential).ok()?;
    Some(tail)
}

/// A borrowed keypad request. Its payload may contain an entered access code.
pub struct DirectoryRequest<'a>(Frame<'a>);

impl<'a> DirectoryRequest<'a> {
    pub fn from_frame(frame: Frame<'a>) -> Option<Self> {
        let h = frame.header;
        (h.frame_type == FrameType::ClusterSpecific
            && h.direction == Direction::ClientToServer
            && h.manufacturer.is_none())
        .then_some(Self(frame))
    }

    /// Serve a read-only CIE. Callers must verify the sender and protect the reply.
    pub fn reply(self, zones: &BTreeMap<u8, Zone>) -> Vec<u8> {
        let f = self.0;
        let h = f.header;
        let malformed = || error(h, 0x80);
        match (h.command_id, f.payload) {
            (0, [mode @ 0..=3, tail @ ..]) if matches!(credential_tail(tail), Some([_])) => {
                // This CIE accepts no arming credential and owns no alarm actuator.
                response(h, 0, &[if *mode == 0 { 4 } else { 5 }])
            }
            (1, [n, tail @ ..]) => {
                let n = usize::from(*n);
                let Some((ids, rest)) = tail.split_at_checked(n) else {
                    return malformed();
                };
                if credential_tail(rest) != Some(&[]) {
                    return malformed();
                }
                let mut result = vec![n as u8];
                result.extend(ids.iter().map(|id| {
                    if *id == 255 {
                        3
                    } else if zones.contains_key(id) {
                        2
                    } else {
                        4
                    }
                }));
                response(h, 7, &result)
            }
            (2..=4, []) | (7..=8, []) => error(h, 1), // No authoritative panel or bypass state.
            (5, []) => {
                let mut map = [0; 32];
                for (&id, _) in zones.range(..u8::MAX) {
                    map[usize::from(id) / 8] |= 1 << (id % 8);
                }
                response(h, 1, &map)
            }
            (6, [id]) => {
                let (kind, ieee) = match zones.get(id).filter(|_| *id != u8::MAX) {
                    Some(Zone {
                        ieee,
                        kind: Some(kind),
                        ..
                    }) => (*kind, *ieee),
                    Some(_) => return error(h, 1),
                    None => (u16::MAX, u64::MAX),
                };
                let mut body = vec![*id];
                body.extend(kind.to_le_bytes());
                body.extend(ieee.to_le_bytes());
                body.push(0); // No zone label has been configured on this CIE.
                response(h, 2, &body)
            }
            (9, [start, max, flag @ 0..=1, lo, hi]) => {
                let mask = u16::from_le_bytes([*lo, *hi]);
                let limit = usize::from(*max).min(8); // ≤29 ZCL bytes even with APS protection.
                let mut body = vec![1, 0];
                if *flag == 1 && mask == 0 {
                    return response(h, 8, &body);
                }
                for (&id, zone) in zones.range(*start..u8::MAX) {
                    if *flag == 1 {
                        match zone.status {
                            Some(status) if status & mask == 0 => continue,
                            None => return error(h, 1),
                            Some(_) => {}
                        }
                    }
                    if usize::from(body[1]) == limit {
                        body[0] = 0;
                        break;
                    }
                    let Some(status) = zone.status else {
                        return error(h, 1);
                    };
                    body.push(id);
                    body.extend(status.to_le_bytes());
                    body[1] += 1;
                }
                response(h, 8, &body)
            }
            (0..=9, _) => malformed(),
            _ => error(h, 0x81),
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn answer(body: &[u8], zones: &BTreeMap<u8, Zone>) -> Vec<u8> {
        DirectoryRequest::from_frame(Frame::decode(body).unwrap())
            .unwrap()
            .reply(zones)
    }
    #[test]
    fn genuine_panel_messages_preserve_alarm_kind_and_delay() {
        let report =
            PanelStatus::decode(Frame::decode(&[0x19, 4, 4, 7, 11, 0x80, 2]).unwrap(), false)
                .unwrap();
        assert_eq!(
            (
                report.state,
                report.seconds_remaining,
                report.audible_notification,
                report.alarm
            ),
            ("in_alarm", 11, 0x80, Some("fire"))
        );
        for encoded in [
            [0x19, 4, 5, 7, 11, 1, 2],
            [0x19, 4, 4, 2, 0, 3, 0],
            [0x19, 4, 4, 7, 11, 1, 9],
        ] {
            assert!(PanelStatus::decode(Frame::decode(&encoded).unwrap(), false).is_none());
        }
    }

    #[test]
    fn only_standard_client_cluster_commands_reach_the_directory() {
        for bytes in [&[0x18, 1, 5][..], &[0x19, 1, 5], &[0x15, 1, 0, 1, 5]] {
            assert!(DirectoryRequest::from_frame(Frame::decode(bytes).unwrap()).is_none());
        }
    }

    #[test]
    fn maps_paginates_and_marks_unknown_observations() {
        let zones = BTreeMap::from([
            (
                0,
                Zone {
                    ieee: 42,
                    kind: Some(0x15),
                    status: Some(1),
                },
            ),
            (
                16,
                Zone {
                    ieee: 77,
                    kind: Some(0x0d),
                    status: Some(2),
                },
            ),
            (
                254,
                Zone {
                    ieee: 99,
                    kind: None,
                    status: Some(0),
                },
            ),
        ]);
        let map = answer(&[0x11, 31, 5], &zones);
        assert_eq!(&map[..3], [0x19, 31, 1]);
        assert_eq!(map.len(), 35);
        assert_eq!((map[3], map[5], map[34]), (1, 1, 0x40));
        assert_eq!(
            &answer(&[0x11, 31, 6, 16], &zones)[3..],
            &[16, 13, 0, 77, 0, 0, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(
            &answer(&[0x11, 31, 6, 32], &zones)[3..],
            &[32, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 0]
        );
        assert_eq!(&answer(&[0x11, 31, 6, 254], &zones)[2..], &[11, 6, 1]);
        assert_eq!(
            &answer(&[0x11, 31, 9, 0, 1, 1, 3, 0], &zones)[2..],
            &[8, 0, 1, 0, 1, 0]
        );
        assert_eq!(
            &answer(&[0x11, 31, 9, 1, 2, 1, 3, 0], &zones)[2..],
            &[8, 1, 1, 16, 2, 0]
        );
        let mut zones = zones;
        zones.get_mut(&16).unwrap().status = None;
        assert_eq!(
            &answer(&[0x11, 31, 9, 0, 1, 0, 0, 0], &zones)[2..],
            &[8, 0, 1, 0, 1, 0]
        );
        assert_eq!(
            &answer(&[0x11, 31, 9, 0, 1, 1, 0, 0], &zones)[2..],
            &[8, 1, 0]
        );
        assert_eq!(
            &answer(&[0x11, 31, 9, 1, 8, 0, 0, 0], &zones)[2..],
            &[11, 9, 1]
        );
    }
    #[test]
    fn refuses_alarm_changes_and_never_echoes_codes() {
        let zones = BTreeMap::from([(
            0,
            Zone {
                ieee: 1,
                kind: Some(13),
                status: None,
            },
        )]);
        assert_eq!(
            answer(&[1, 12, 0, 3, 4, b'T', b'E', b'S', b'T', 0], &zones),
            [0x19, 12, 0, 5]
        );
        assert_eq!(answer(&[1, 12, 0, 0, 0, 255], &zones), [0x19, 12, 0, 4]);
        assert_eq!(
            answer(&[1, 12, 1, 3, 0, 2, 255, 0], &zones),
            [0x19, 12, 7, 3, 2, 4, 3]
        );
        for malformed in [
            vec![0, 4, 0, 0],
            vec![0, 0, 1],
            vec![0, 0, 255],
            vec![1, 8],
            vec![1, 0, 1, 255],
            vec![5, 1],
            vec![9, 0, 1, 2, 0, 0],
        ] {
            let mut raw = vec![1, 88];
            raw.extend(&malformed);
            assert_eq!(answer(&raw, &zones), [0x18, 88, 11, malformed[0], 0x80]);
        }
        for cmd in [2, 3, 4, 7, 8] {
            assert_eq!(answer(&[1, 12, cmd], &zones), [0x18, 12, 11, cmd, 1]);
        }
    }
}
