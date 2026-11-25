//! Tuya Zigbee Generic Interfaces, TUYA_MCU_SYNC_TIME. The two-byte MCU
//! sequence in the ZCL payload is independent of the ZCL transaction number.
//! [Gateway and device payloads](https://images.tuyacn.com/goat/20200403/8d8832209c91459aae550293100282bf.png).
use serde::Deserialize;
use zigbee_zcl::{Direction, Frame, FrameType, Header};

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimeEpoch {
    Unix,
    Zigbee,
}

#[derive(Debug, PartialEq, Eq)]
pub enum TimeError {
    Malformed,
    Unavailable,
}

impl TimeEpoch {
    pub fn is_request(self, frame: Frame<'_>) -> bool {
        let h = frame.header;
        h.frame_type == FrameType::ClusterSpecific
            && h.direction == Direction::ServerToClient
            && h.manufacturer.is_none()
            && h.command_id == 0x24
    }

    /// Accept the short MCU request or its documented zero-filled timestamp fields.
    pub fn response(
        self,
        frame: Frame<'_>,
        unix: i64,
        utc_offset: Option<i32>,
    ) -> Result<Vec<u8>, TimeError> {
        if !self.is_request(frame) {
            return Err(TimeError::Malformed);
        }
        let ([hi, lo], tail) = frame
            .payload
            .split_first_chunk::<2>()
            .ok_or(TimeError::Malformed)?;
        if !tail.is_empty() && tail != [0; 8] {
            return Err(TimeError::Malformed);
        }
        let origin = match self {
            Self::Unix => 0,
            Self::Zigbee => 946_684_800,
        };
        let utc = unix
            .checked_sub(origin)
            .and_then(|s| u32::try_from(s).ok())
            .ok_or(TimeError::Unavailable)?;
        let local = i64::from(utc) + i64::from(utc_offset.ok_or(TimeError::Unavailable)?);
        let local = u32::try_from(local).map_err(|_| TimeError::Unavailable)?;
        let mut data = [0; 10];
        data[..2].copy_from_slice(&[*hi, *lo]);
        data[2..6].copy_from_slice(&utc.to_be_bytes());
        data[6..].copy_from_slice(&local.to_be_bytes());
        let mut wire = Vec::with_capacity(13);
        Frame {
            header: Header {
                direction: Direction::ClientToServer,
                disable_default_response: true,
                ..frame.header
            },
            payload: &data,
        }
        .encode_into(&mut wire);
        Ok(wire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_both_sequences_and_encodes_utc_and_local_time() {
        let input = Frame::decode(&[0x19, 0xa1, 0x24, 0x04, 0x37]).unwrap();
        let wire = TimeEpoch::Unix
            .response(input, 0x01020304, Some(3600))
            .unwrap();
        assert_eq!(
            wire,
            [0x11, 0xa1, 0x24, 4, 0x37, 1, 2, 3, 4, 1, 2, 0x11, 0x14]
        );
        assert!(!TimeEpoch::Unix.is_request(Frame::decode(&wire).unwrap()));
        let padded = Frame::decode(&[0x09, 3, 0x24, 0x12, 0x34, 0, 0, 0, 0, 0, 0, 0, 0]).unwrap();
        assert_eq!(
            &TimeEpoch::Zigbee
                .response(padded, 946_684_800 + 3600, Some(-3600))
                .unwrap()[3..],
            &[0x12, 0x34, 0, 0, 0x0e, 0x10, 0, 0, 0, 0]
        );
    }

    #[test]
    fn invalid_requests_or_unrepresentable_clocks_do_not_send_time() {
        for bytes in [
            &[0x19, 1, 0x24, 0][..],
            &[0x19, 1, 0x24, 0, 7, 1],
            &[0x11, 1, 0x24, 0, 7],
            &[0x18, 1, 0x24, 0, 7],
            &[0x1d, 0x12, 0x34, 1, 0x24, 0, 7],
        ] {
            let f = Frame::decode(bytes).unwrap();
            assert_eq!(
                TimeEpoch::Unix.response(f, 1, Some(0)),
                Err(TimeError::Malformed)
            );
        }
        let f = Frame::decode(&[0x19, 1, 0x24, 0, 7]).unwrap();
        for (utc, offset) in [(-1, Some(0)), (0, Some(-1)), (i64::MAX, Some(0)), (1, None)] {
            assert_eq!(
                TimeEpoch::Unix.response(f, utc, offset),
                Err(TimeError::Unavailable)
            );
        }
    }
}
