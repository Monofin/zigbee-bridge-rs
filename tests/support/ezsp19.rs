//! Native NCP side of the PTY test. Field layouts follow Silicon Labs SDK
//! f67fe2e2c1f1d6abae44d748a94e23786e403490, command-functions.h and
//! ezsp-frame-utilities.c. These helpers do not call the bridge's host codec.
use super::{Lamps, ieee, short};
use anyhow::{Result, bail, ensure};
use std::collections::VecDeque;

#[derive(Default)]
pub struct Native {
    offered: bool,
    negotiated: bool,
}

impl Native {
    pub fn command(
        &mut self,
        bytes: &[u8],
        lamps: &mut Lamps,
        out: &mut VecDeque<Vec<u8>>,
    ) -> Result<()> {
        if !self.offered {
            ensure!(bytes == [0, 0, 0, 13], "expected initial legacy VERSION");
            out.push_back(vec![0, 0x80, 0, 19, 2, 0x02, 0x95]);
            self.offered = true;
            return Ok(());
        }
        if !self.negotiated {
            ensure!(bytes == [1, 0, 1, 0, 0, 19], "host must confirm VERSION 19");
            out.push_back(vec![1, 0x80, 1, 0, 0, 19, 2, 0x02, 0x95]);
            self.negotiated = true;
            return Ok(());
        }
        let [seq, 0, 1, lo, hi, args @ ..] = bytes else {
            bail!("invalid extended request")
        };
        let id = u16::from_le_bytes([*lo, *hi]);
        // Decode the native request before handing common Zigbee behaviour to the lamps.
        let mut request = vec![*seq, 0, 1, *lo, *hi];
        match id {
            0x34 => {
                let (address_aps, tagged) = args
                    .split_at_checked(14)
                    .ok_or_else(|| anyhow::anyhow!("short unicast"))?;
                let [tag, 0, count, data @ ..] = tagged else {
                    bail!("invalid 16-bit host tag")
                };
                ensure!(
                    usize::from(*count) == data.len(),
                    "invalid native unicast length"
                );
                request.extend(address_aps);
                request.extend([*tag, *count]);
                request.extend(data);
            }
            0x36 => {
                let [0xff, 0xff, dst_lo, dst_hi, 0, aps_radius_tag_payload @ ..] = args else {
                    bail!("invalid broadcast alias")
                };
                let (aps_radius, tagged) = aps_radius_tag_payload
                    .split_at_checked(12)
                    .ok_or_else(|| anyhow::anyhow!("short broadcast"))?;
                let [tag, 0, count, data @ ..] = tagged else {
                    bail!("invalid 16-bit broadcast tag")
                };
                ensure!(data.len() == usize::from(*count));
                request.extend([*dst_lo, *dst_hi]);
                request.extend(aps_radius);
                request.extend([*tag, *count]);
                request.extend(data);
            }
            0x111 => {
                ensure!(
                    args.len() == 24,
                    "transient-key import consists of EUI64 and key"
                );
                request.extend(args);
                request.push(0);
            }
            _ => request.extend(args),
        }
        let mut messages = VecDeque::new();
        lamps.command(&request, &mut messages)?;
        for frame in messages {
            let (header, payload) = frame.split_at(5);
            let message_id = u16::from_le_bytes([header[3], header[4]]);
            let mut native = header.to_vec();
            if header[1] == 0x90 {
                callback(message_id, payload, &mut native)?;
            } else {
                ensure!(header[1] == 0x80 && message_id == id && header[0] == *seq);
                match id {
                    0x5a | 0x18 | 0x26 | 0x27 | 0x6b | 0x75 | 0x111 => native.extend(payload),
                    0x60 if payload == [0xff,0xff] => native.extend(0x2du32.to_le_bytes()),
                    0x61 if payload.first() == Some(&1) => native.extend(1u32.to_le_bytes()),
                    0x60 => {
                        native.extend(0u32.to_le_bytes());
                        native.extend(payload);
                    }
                    0x02 | 0x10 | 0x17 | 0x22 | 0x28 | 0x34 | 0x36 | 0x52 | 0x53 | 0x99 | 0x55 | 0x56 | 0x61 | 0x102 => {
                        let (&status, fields) = payload
                            .split_first()
                            .ok_or_else(|| anyhow::anyhow!("missing command status"))?;
                        ensure!(status == 0, "unexpected fixture failure");
                        native.extend(0u32.to_le_bytes());
                        native.extend(fields);
                    }
                    _ => bail!("native result shape not modelled for {id:#x}"),
                }
            }
            out.push_back(native);
        }
        Ok(())
    }
}

fn callback(id: u16, fields: &[u8], out: &mut Vec<u8>) -> Result<()> {
    match id {
        0x45 => {
            let (type_aps, rest) = fields.split_at(12);
            let [lqi, _rssi, lo, hi, _binding, _address, count, data @ ..] = rest else {
                bail!("invalid incoming message")
            };
            ensure!(data.len() == usize::from(*count));
            let sender = u16::from_le_bytes([*lo, *hi]) - short(0);
            out.extend(type_aps);
            out.extend([*lo, *hi]);
            out.extend(ieee(sender).to_le_bytes());
            // Distinct metadata catch accidental reuse of v13 LQI offsets.
            out.extend([0xff, 0xfe, *lqi, (-46i8) as u8]);
            out.extend(0x01020304u32.to_le_bytes());
            out.push(*count);
            out.extend(data);
        }
        0x3f => {
            let (routing, rest) = fields.split_at(14);
            let [tag, status, count, data @ ..] = rest else {
                bail!("invalid sent callback")
            };
            ensure!(*status == 0 && data.len() == usize::from(*count));
            out.extend(0u32.to_le_bytes());
            out.extend(routing);
            out.extend(u16::from(*tag).to_le_bytes());
            out.push(*count);
            out.extend(data);
        }
        0x19 => {
            ensure!(fields == [0x90]);
            out.extend(0x15u32.to_le_bytes());
        }
        0x24 | 0x7c | 0x9b => out.extend(fields),
        _ => bail!("native callback {id:#x} not modelled"),
    }
    Ok(())
}
