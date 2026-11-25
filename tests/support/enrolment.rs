//! Successful fixture enrolment on the existing ASH session.
use super::{BUTTON, endpoint, ieee, incoming, short};
use anyhow::{Result, bail, ensure};
pub const NEW: u16 = BUTTON + 1;

pub struct Radio {
    pub(super) profile: u16,
    cie: u64,
    zone: Option<u8>,
    policy: u8,
    tc: u8,
    awaiting_descriptor: bool,
}

fn zdo(cluster: u16, body: &[u8]) -> Vec<u8> {
    let mut msg = incoming(NEW, cluster, body);
    msg[6..8].fill(0);
    msg[10..12].fill(0);
    msg
}
type Reply = (Vec<u8>, Vec<Vec<u8>>);
impl Radio {
    pub fn new(profile: u16) -> Self {
        Self {
            profile,
            cie: 0,
            zone: None,
            policy: 0,
            tc: 0,
            awaiting_descriptor: false,
        }
    }

    pub fn command(&mut self, id: u16, args: &[u8]) -> Result<Option<Reply>> {
        let [lo, hi] = short(NEW).to_le_bytes();
        let mut next = Vec::new();
        let result = match (id, args) {
            // A newly interviewed device can lose its small NCP table entry
            // between Active_EP_rsp and Simple_Desc_req.
            (0x60, eui) if self.awaiting_descriptor && eui == ieee(NEW).to_le_bytes() => u16::MAX.to_le_bytes().to_vec(),
            (0x55, [0, decision]) => {
                self.policy = *decision;
                vec![0]
            }
            (0x55, [5, decision]) => {
                self.tc = *decision;
                vec![0]
            }
            (0x56, [0]) => vec![0, self.policy],
            (0x56, [5]) => vec![0, self.tc],
            (0x111, key) => {
                ensure!(key.len() == 25);
                vec![0; 4]
            }
            (0x6b, []) => vec![],
            (0x22, [duration]) => {
                if *duration != 0 {
                    let eui = ieee(NEW).to_le_bytes();
                    let mut joined = vec![0, 0x90, 1, 0x24, 0, lo, hi];
                    joined.extend(eui);
                    joined.extend([1, 0, 0, 0]);
                    next.push(joined);
                    let mut verified = vec![0, 0x90, 1, 0x9b, 0];
                    verified.extend(eui);
                    verified.push(0x34);
                    next.push(verified);
                    let mut ann = vec![17, lo, hi];
                    ann.extend(eui);
                    ann.push(0x8e);
                    next.push(zdo(0x13, &ann));
                }
                vec![0]
            }
            (0x34, frame) if frame.get(1..3) == Some(&[lo, hi]) => {
                let profile = u16::from_le_bytes(frame[3..5].try_into()?);
                let cluster = u16::from_le_bytes(frame[5..7].try_into()?);
                let payload = &frame[16..];
                let ep = endpoint(NEW);
                if profile == 0 {
                    let data = match cluster {
                        5 => {
                            self.awaiting_descriptor = true;
                            vec![ep]
                        }
                        4 => {
                            self.awaiting_descriptor = false;
                            let [lo, hi] = self.profile.to_le_bytes();
                            // The two profiles assign different IDs to a dimmable light.
                            let device_lo = u8::from(self.profile == 0x0104);
                            vec![ep, lo, hi, device_lo, 1, 1, 3, 6, 0, 8, 0, 0, 5, 0]
                        }
                        _ => bail!("unexpected interview ZDO request"),
                    };
                    let mut body = vec![payload[0], 0, lo, hi, data.len().try_into()?];
                    body.extend(data);
                    // A late response reuses the TSN but concerns a different node.
                    next.push(zdo(0x8000 | cluster, &[payload[0], 0x81, lo ^ 1, hi, 0]));
                    next.push(zdo(0x8000 | cluster, &body));
                } else if cluster == 0x0500 {
                    ensure!(profile == self.profile && frame[7..9] == [1, ep]);
                    self.zone_command(payload, &mut next)?;
                } else {
                    ensure!(
                        profile == self.profile
                            && cluster == 6
                            && frame[8] == ep
                            && payload[2..] == [0, 0, 0],
                        "unexpected read"
                    );
                    let mut reply = incoming(NEW, 6, &[8, payload[1], 1, 0, 0, 0, 0x10, 1]);
                    reply[6..8].copy_from_slice(&profile.to_le_bytes());
                    next.push(reply);
                }
                vec![0, 9]
            }
            _ => return Ok(None),
        };
        Ok(Some((result, next)))
    }

    fn zone_command(&mut self, payload: &[u8], next: &mut Vec<Vec<u8>>) -> Result<()> {
        let mut reply = |body: &[u8]| {
            let mut msg = incoming(NEW, 0x0500, body);
            msg[6..8].copy_from_slice(&self.profile.to_le_bytes());
            next.push(msg);
        };
        match payload {
            [0x10, seq, 0, 2, 0] => {
                let zone = self.zone.ok_or_else(|| anyhow::anyhow!("status poll before enrollment"))?;
                reply(&[0x18, *seq, 1, 2, 0, 0, 0x19, 1, 0]);
                reply(&[0x19, 10, 0, 0, 0, 0, zone, 0, 0]);
            }
            [0x10, seq, 0, 1, 0, 2, 0] => {
                reply(&[0x18, *seq, 1, 1, 0, 0, 0x31, 0x15, 0, 2, 0, 0, 0x19, 0, 0])
            }
            [0x10, seq, 0, 0, 0, 0x10, 0, 0x11, 0] => {
                let mut row = vec![
                    0x18,
                    *seq,
                    1,
                    0,
                    0,
                    0,
                    0x30,
                    u8::from(self.zone.is_some()),
                    0x10,
                    0,
                    0,
                    0xf0,
                ];
                row.extend(self.cie.to_le_bytes());
                row.extend([0x11, 0, 0, 0x20, self.zone.unwrap_or(255)]);
                reply(&row);
                if let Some(zone) = self.zone {
                    reply(&[0x19, 9, 0, 1, 0, 0, zone, 0, 0]);
                }
            }
            [0x10, seq, 2, 0x10, 0, 0xf0, addr @ ..] => {
                ensure!(addr == ieee(0).to_le_bytes() && self.zone.is_none());
                self.cie = ieee(0);
                // One zone asks before the write acknowledgement. The other
                // waits for an unsolicited Enroll Response from its CIE.
                if self.profile == 0x0104 {
                    reply(&[0x19, 0x79, 1, 0x15, 0, 0, 0]);
                }
                reply(&[0x18, *seq, 4, 0]);
            }
            [0x11, seq, 0, 0, id @ 0..=254] => {
                ensure!(self.cie == ieee(0) && self.zone.is_none());
                ensure!(
                    self.profile != 0x0104 || *seq == 0x79,
                    "request sequence was lost"
                );
                self.zone = Some(*id);
            }
            _ => bail!("unexpected IAS frame"),
        }
        Ok(())
    }
}
