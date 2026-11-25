//! EZSP 19 wire layouts used by the bridge. The raw session exposes native
//! commands; these adapters isolate the existing coordinator's parameter models.
use super::{Callback, Failure};
use anyhow::{Result, bail, ensure};

const SEND_UNICAST: u16 = 0x34;
const SEND_BROADCAST: u16 = 0x36;
const IMPORT_TRANSIENT_KEY: u16 = 0x111;

pub(super) fn encode(id: u16, args: &[u8]) -> Result<Vec<u8>> {
    match id {
        SEND_UNICAST | SEND_BROADCAST => {
            ensure!(
                args.len() >= 16 && args.len() - 16 == usize::from(args[15]),
                "invalid send payload"
            );
            let mut wire = Vec::with_capacity(args.len() + 4);
            if id == SEND_BROADCAST {
                wire.extend(0xffff_u16.to_le_bytes()); // no source alias
                wire.extend(&args[..2]); // destination
                wire.push(0); // unused alias NWK sequence
                wire.extend(&args[2..14]); // APS and radius
            } else {
                wire.extend(&args[..14]); // type, destination and APS
            }
            wire.extend(u16::from(args[14]).to_le_bytes()); // native message tag
            wire.extend(&args[15..]);
            Ok(wire)
        }
        IMPORT_TRANSIENT_KEY => {
            ensure!(
                args.len() == 25 && args[24] == 0,
                "v19 transient-key import has no flags parameter"
            );
            Ok(args[..24].to_vec())
        }
        0x02 | 0x10 | 0x5a | 0x17 | 0x18 | 0x1e | 0x22 | 0x26 | 0x27 | 0x28 | 0x52 | 0x99 | 0x53 | 0x55 | 0x56 | 0x60
        | 0x61 | 0x75 | 0x68 | 0x6b | 0xf1 | 0x102 | 0x116 => Ok(args.to_vec()),
        _ => bail!("EZSP 19 runtime command {id:#06x} has no reviewed codec"),
    }
}

fn status(bytes: &[u8]) -> Result<(u32, &[u8])> {
    let (head, rest) = bytes
        .split_at_checked(4)
        .ok_or_else(|| anyhow::anyhow!("truncated sl_status_t"))?;
    Ok((u32::from_le_bytes(head.try_into()?), rest))
}

pub(super) fn response(id: u16, body: Vec<u8>) -> Result<Vec<u8>> {
    // These responses have no status or already used native 32-bit status in v13.
    if matches!(id, 0x5a | 0x18 | 0x26 | 0x27 | 0x6b | 0x75 | 0xf1 | 0x111 | 0x116) {
        return Ok(body);
    }
    let (status, rest) = status(&body)?;
    if status != 0 {
        match id {
            0x17 if status == 0x0017 => return Ok(vec![0x93]), // NOT_JOINED on a fresh NCP
            // Failure is an ordinary per-device outcome. Preserve the native
            // reason in diagnostics and give callers their explicit negative result.
            0x60 | 0x61 | SEND_UNICAST | SEND_BROADCAST => {
                eprintln!("EZSP {id:#06x} returned sl_status_t {status:#010x}");
                return Ok(match id {
                    0x60 => 0xffff_u16.to_le_bytes().to_vec(),
                    0x61 => vec![0xff],
                    _ => vec![0xff, 0],
                });
            }
            _ => bail!("EZSP {id:#06x} returned sl_status_t {status:#010x}"),
        }
    }
    let valid = match id {
        0x02 | 0x10 | 0x17 | 0x1e | 0x22 | 0x53 | 0x55 | 0x68 | 0x99 => rest.is_empty(),
        0x56 | SEND_UNICAST | SEND_BROADCAST => rest.len() == 1,
        0x28 => rest.len() == 21,
        0x52 | 0x60 => rest.len() == 2,
        0x61 => rest.len() == 8,
        0x102 => rest
            .get(..4)
            .and_then(|v| v.try_into().ok())
            .is_some_and(|n| usize::try_from(u32::from_le_bytes(n)).ok() == Some(rest.len() - 4)),
        _ => false,
    };
    ensure!(valid, "invalid native EZSP 19 result for {id:#06x}");
    if id == 0x60 {
        return Ok(rest.to_vec());
    }
    let mut normalized = vec![0];
    normalized.extend(rest);
    Ok(normalized)
}

/// The long-address field is defined only when the APS source-EUI64 bit is set.
pub(super) fn source_eui(id: u16, payload: &[u8]) -> Option<u64> {
    if id != 0x45 || u16::from_le_bytes(payload.get(7..9)?.try_into().ok()?) & 0x0400 == 0 {
        return None;
    }
    Some(u64::from_le_bytes(payload.get(14..22)?.try_into().ok()?))
}

/// Hand the runtime the fields it uses. Foreign 16-bit message tags cannot
/// complete an operation submitted by the bridge's byte-sized tag allocator.
pub(super) fn callback(id: u16, p: &[u8]) -> Result<Option<Callback>, Failure> {
    let payload = match id {
        0x19 => {
            let native = u32::from_le_bytes(p.try_into().map_err(|_| Failure::InvalidResponse)?);
            let code = match native {
                0x0015 => 0x90,
                0x0016 => 0x91,
                0x0c18 => 0x9c,
                0x0c19 => 0x9d,
                _ => return Err(Failure::StackStatus(native)),
            };
            vec![code]
        }
        0x3f => {
            // status32, type, dest16, APS[11], tag16, length, message
            if p.len() < 21 || p.len() - 21 < usize::from(p[20]) {
                return Err(Failure::InvalidResponse);
            }
            let tag = u16::from_le_bytes([p[18], p[19]]);
            let Ok(tag) = u8::try_from(tag) else {
                return Ok(None);
            };
            let native = u32::from_le_bytes(p[..4].try_into().unwrap());
            let mut fields = p[4..18].to_vec();
            fields.push(tag);
            if native != 0 {
                eprintln!("EZSP message delivery returned sl_status_t {native:#010x}")
            }
            fields.push(if native == 0 { 0 } else { 0xff });
            fields.extend(&p[20..]);
            fields
        }
        0x45 => {
            // type, APS[11], short16, long[8], binding, address, LQI, RSSI,
            // timestamp32, length, message. Length excludes optional trailing data.
            if p.len() < 31 || p.len() - 31 < usize::from(p[30]) {
                return Err(Failure::InvalidResponse);
            }
            let mut fields = p[..12].to_vec();
            fields.extend(&p[24..26]); // LQI, RSSI
            fields.extend(&p[12..14]); // sender short
            fields.extend(&p[22..24]); // binding and address indices
            fields.extend(&p[30..]);
            fields
        }
        _ => p.to_vec(),
    };
    Ok(Some(Callback { id, payload }))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Independently laid out from Silicon Labs SDK 2026.6.2 generated codecs.
    #[test]
    fn unicast_and_ordinary_broadcast_keep_aps_and_zcl_sequences_distinct() -> Result<()> {
        let uc = [
            0, 0x34, 0x12, 4, 1, 6, 0, 1, 11, 0x40, 1, 0, 0, 0x77, 0xfe, 3, 0x11, 0x29, 1,
        ];
        assert_eq!(
            encode(SEND_UNICAST, &uc)?,
            [
                0, 0x34, 0x12, 4, 1, 6, 0, 1, 11, 0x40, 1, 0, 0, 0x77, 0xfe, 0, 3, 0x11, 0x29, 1
            ]
        );
        let bc = [
            0xfc, 0xff, 0, 0, 0x36, 0, 0, 0, 0, 0, 0, 0, 0x22, 0, 0x89, 3, 0x56, 0, 1,
        ];
        assert_eq!(
            encode(SEND_BROADCAST, &bc)?,
            [
                0xff, 0xff, 0xfc, 0xff, 0, 0, 0, 0x36, 0, 0, 0, 0, 0, 0, 0, 0x22, 0, 0x89, 0, 3,
                0x56, 0, 1
            ]
        );
        assert!(encode(SEND_UNICAST, &uc[..uc.len() - 1]).is_err());
        assert_eq!(response(SEND_UNICAST, vec![0, 0, 0, 0, 0x77])?, [0, 0x77]);
        // A nonzero native failure with a zero low byte still fails.
        assert_ne!(response(SEND_UNICAST, vec![0, 0x0c, 0, 0, 0x77])?[0], 0);
        Ok(())
    }

    #[test]
    fn identity_and_commissioning_preserve_each_native_result_shape() -> Result<()> {
        let flags = [0x33; 24];
        let mut old = flags.to_vec();
        old.push(0);
        assert_eq!(encode(IMPORT_TRANSIENT_KEY, &old)?, flags);
        old[24] = 1;
        assert!(encode(IMPORT_TRANSIENT_KEY, &old).is_err());
        assert_eq!(response(0x60, vec![0, 0, 0, 0, 0x34, 0x12])?, [0x34, 0x12]);
        assert_eq!(
            response(0x60, vec![0, 0x0c, 0, 0, 0x34, 0x12])?,
            [0xff, 0xff]
        );
        assert!(response(0x60, vec![0, 0, 0, 0, 1]).is_err());
        assert_eq!(
            response(0x61, vec![0, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8])?,
            [0, 1, 2, 3, 4, 5, 6, 7, 8]
        );
        let token = [
            0, 0, 0, 0, 16, 0, 0, 0, 0x12, 0x34, 0, 20, 2, 1, 0, 0, 8, 7, 6, 5, 4, 3, 2, 1,
        ];
        assert_eq!(response(0x102, token.to_vec())?, token[3..]);
        assert!(response(0x102, token[..23].to_vec()).is_err());
        assert_eq!(response(0x17, vec![0x17, 0, 0, 0])?, [0x93]);
        for (id, size) in [
            (0x02, 0),
            (0x17, 0),
            (0x1e, 0),
            (0x22, 0),
            (0x28, 21),
            (0x52, 2),
            (0x99, 0),
            (0x53, 0),
            (0x55, 0),
            (0x56, 1),
            (0x68, 0),
        ] {
            assert_eq!(response(id, vec![0; size + 4])?, vec![0; size + 1]);
            assert!(response(id, vec![0, 1, 0, 0]).is_err());
        }
        let key_info = vec![0, 0, 0, 0, 1, 0, 2, 0, 0x44, 0x33, 0x22, 0x11];
        assert_eq!(response(0x116, key_info.clone())?, key_info);
        Ok(())
    }

    #[test]
    fn incoming_message_preserves_length_source_lqi_and_application_data() {
        let native = [
            0, 4, 1, 8, 0, 11, 1, 0x40, 1, 0, 0, 0x5a, 0x34, 0x12, 8, 7, 6, 5, 4, 3, 2, 1, 0xff,
            0x71, 0xb4, 0xd8, 4, 3, 2, 1, 3, 0x18, 0x48, 0x0a,
        ];
        assert_eq!(source_eui(0x45, &native), None);
        let mut with_source = native;
        with_source[8] |= 4;
        assert_eq!(source_eui(0x45, &with_source), Some(0x0102030405060708));
        let cb = callback(0x45, &native).unwrap().unwrap();
        assert_eq!(
            cb.payload,
            [
                0, 4, 1, 8, 0, 11, 1, 0x40, 1, 0, 0, 0x5a, 0xb4, 0xd8, 0x34, 0x12, 0xff, 0x71, 3,
                0x18, 0x48, 0x0a
            ]
        );
        assert!(callback(0x45, &native[..33]).is_err());
        let parsed = crate::interview::incoming(&cb.payload).unwrap();
        assert_eq!(
            (parsed.sender, parsed.last_hop_lqi, parsed.body),
            (0x1234, 180, [0x18, 0x48, 0x0a].as_slice())
        );
    }

    #[test]
    fn sent_callback_matches_full_width_tags_and_full_width_status() {
        let mut sent = [
            0, 0, 0, 0, 0, 0x34, 0x12, 4, 1, 8, 0, 1, 11, 0x40, 1, 0, 0, 0x5a, 0x42, 0, 0,
        ];
        assert_eq!(
            callback(0x3f, &sent).unwrap().unwrap().payload,
            [
                0, 0x34, 0x12, 4, 1, 8, 0, 1, 11, 0x40, 1, 0, 0, 0x5a, 0x42, 0, 0
            ]
        );
        sent[19] = 1;
        assert!(callback(0x3f, &sent).unwrap().is_none());
        sent[19] = 0;
        sent[1] = 12;
        assert_ne!(callback(0x3f, &sent).unwrap().unwrap().payload[15], 0);
        for (native, old) in [
            (0x15u32, 0x90),
            (0x16, 0x91),
            (0x0c18, 0x9c),
            (0x0c19, 0x9d),
        ] {
            assert_eq!(
                callback(0x19, &native.to_le_bytes())
                    .unwrap()
                    .unwrap()
                    .payload,
                [old]
            );
        }
        assert!(matches!(
            callback(0x19, &[0x15, 1, 0, 0]),
            Err(Failure::StackStatus(0x115))
        ));
    }
}
