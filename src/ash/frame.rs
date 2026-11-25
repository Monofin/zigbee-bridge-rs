//! Silicon Labs ASH v2, UART Gateway Protocol Reference, sections 2–4.
const FLAG: u8 = 0x7e;
const ESCAPE: u8 = 0x7d;
const SUBSTITUTE: u8 = 0x18;
const CANCEL: u8 = 0x1a;
pub(super) const MAX_DATA: usize = 220;
const MAX_BODY: usize = 1 + MAX_DATA + 2;

fn reserved(byte: u8) -> bool {
    matches!(byte, FLAG | ESCAPE | 0x11 | 0x13 | SUBSTITUTE | CANCEL)
}

fn crc(bytes: &[u8]) -> u16 {
    let mut crc = 0xffff_u16;
    for &byte in bytes {
        crc ^= u16::from(byte) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

fn randomize(bytes: &mut [u8]) {
    let mut mask = 0x42;
    for byte in bytes {
        *byte ^= mask;
        mask = (mask >> 1) ^ if mask & 1 != 0 { 0xb8 } else { 0 };
    }
}

/// Control and unrandomized data field for a complete ASH frame.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Frame {
    pub control: u8,
    pub data: Vec<u8>,
}

impl Frame {
    pub fn encode(control: u8, data: &[u8]) -> Vec<u8> {
        let mut body = vec![control];
        body.extend_from_slice(data);
        if control & 0x80 == 0 {
            randomize(&mut body[1..]);
        }
        body.extend_from_slice(&crc(&body).to_be_bytes());
        let mut wire = Vec::with_capacity(body.len() + 2);
        if matches!(control, 0xc0 | 0xc1) {
            wire.push(CANCEL);
        }
        for byte in body {
            if reserved(byte) {
                wire.extend([ESCAPE, byte ^ 0x20]);
            } else {
                wire.push(byte);
            }
        }
        wire.push(FLAG);
        wire
    }

    fn decode(body: &[u8]) -> Option<Self> {
        let (&control, tail) = body.split_first()?;
        let (data, checksum) = tail.split_last_chunk::<2>()?;
        if crc(&body[..body.len() - 2]) != u16::from_be_bytes(*checksum) {
            return None;
        }
        match control {
            0..=0x7f if (3..=MAX_DATA).contains(&data.len()) => (),
            0x80..=0x8f | 0xa0..=0xaf | 0xc0 if data.is_empty() => (),
            0xc1 | 0xc2 if data.len() == 2 => (),
            _ => return None,
        }
        let mut data = data.to_vec();
        if control & 0x80 == 0 {
            randomize(&mut data);
        }
        Some(Self { control, data })
    }
}

/// The maximum allocation is bounded independently of UART chunk boundaries.
#[derive(Default)]
pub(super) struct Decoder {
    body: Vec<u8>,
    escaped: bool,
    discard: bool,
}

impl Decoder {
    /// Empty flags, low-level flow control and cancelled prefixes carry no event.
    pub fn push(&mut self, byte: u8) -> Option<Result<Frame, ()>> {
        match byte {
            FLAG => {
                let event = if self.discard || self.escaped {
                    Some(Err(()))
                } else if self.body.is_empty() {
                    None
                } else {
                    Some(Frame::decode(&self.body).ok_or(()))
                };
                self.clear();
                return event;
            }
            CANCEL => self.clear(),
            SUBSTITUTE => self.discard = true,
            0x11 | 0x13 => (),
            _ if self.discard => (),
            ESCAPE if !self.escaped => self.escaped = true,
            _ => {
                if self.body.len() == MAX_BODY {
                    self.discard = true;
                } else {
                    self.body
                        .push(if self.escaped { byte ^ 0x20 } else { byte });
                }
                self.escaped = false;
            }
        }
        None
    }

    fn clear(&mut self) {
        self.body.clear();
        self.escaped = false;
        self.discard = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn published_vectors_match_both_directions() {
        for (control, data, expected) in [
            (0xc0, &[][..], &[0x1a, 0xc0, 0x38, 0xbc, 0x7e][..]),
            (0xc1, &[2, 2], &[0x1a, 0xc1, 2, 2, 0x9b, 0x7b, 0x7e]),
            (0x81, &[], &[0x81, 0x60, 0x59, 0x7e]),
            (
                0x25,
                &[0, 0, 0, 2],
                &[0x25, 0x42, 0x21, 0xa8, 0x56, 0xa6, 0x09, 0x7e],
            ),
            // The published randomized response example has incorrect CRC octets.
            // CRC-CCITT(ffff, 53 42 a1 a8 56 28 04 82) = 032a.
            (
                0x53,
                &[0, 0x80, 0, 2, 2, 0x11, 0x30],
                &[
                    0x53, 0x42, 0xa1, 0xa8, 0x56, 0x28, 4, 0x82, 0x03, 0x2a, 0x7e,
                ],
            ),
        ] {
            let encoded = Frame::encode(control, data);
            assert_eq!(encoded, expected);
            let mut decoder = Decoder::default();
            let frames: Vec<_> = encoded
                .into_iter()
                .filter_map(|b| decoder.push(b))
                .collect();
            assert_eq!(
                frames,
                [Ok(Frame {
                    control,
                    data: data.to_vec()
                })]
            );
        }
    }

    #[test]
    fn discard_partial_frames_and_recover_at_the_next_boundary() {
        let mut decoder = Decoder::default();
        let mut wire = vec![FLAG, 0x7d, 0x11, CANCEL, 0x03, SUBSTITUTE, 0x00, FLAG];
        wire.extend([0; 1000]);
        wire.extend([FLAG, 0x81, 0x60, 0x58, FLAG]); // bad CRC
        let data: Vec<u8> = (0..MAX_DATA as u8).collect();
        wire.extend(Frame::encode(0, &data));
        let frames: Vec<_> = wire.into_iter().filter_map(|b| decoder.push(b)).collect();
        assert_eq!(
            frames,
            [Err(()), Err(()), Err(()), Ok(Frame { control: 0, data })]
        );
    }
}
