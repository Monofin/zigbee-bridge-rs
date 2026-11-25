//! ZCL R8 §3.12. Use one instant for every attribute in a transaction.
use super::responses::{self, Outcome};
use jiff::{
    Timestamp,
    tz::{Dst, TimeZone},
};
use zigbee_zcl::{
    Frame,
    foundation::{GeneralCommand, Payload},
};

pub(super) const CLUSTER: u16 = 0x000a;
const EPOCH: i64 = 946_684_800;
const INVALID: u32 = u32::MAX;
const UNSUPPORTED: u8 = 0x86;
const READ_ONLY: u8 = 0x88;
// Leave room for security and routing overhead in a single APS unicast.
const BODY_BUDGET: usize = 64;
fn zigbee_seconds(unix: i64) -> u32 {
    unix.checked_sub(EPOCH)
        .and_then(|s| u32::try_from(s).ok())
        .unwrap_or(INVALID)
}

struct Season {
    standard: i32,
    start: u32,
    end: u32,
    shift: i32,
}

impl Season {
    // ZCL represents one contiguous season. Use the current season in summer
    // and the next season in winter. A change of base offset is not DST.
    fn at(zone: &TimeZone, now: Timestamp) -> Option<Self> {
        let here = zone.to_offset_info(now);
        let mut next = zone.following(now);
        let (start, end) = if here.dst() == Dst::Yes {
            // preceding() is exclusive, including at the first second of DST.
            let after = Timestamp::new(now.as_second(), 999_999_999).ok()?;
            (zone.preceding(after).next()?, next.next()?)
        } else {
            let Some(start) = next.next() else {
                return Some(Self {
                    standard: here.offset().seconds(),
                    start: INVALID,
                    end: INVALID,
                    shift: 0,
                });
            };
            (start, next.next()?)
        };
        let before = Timestamp::from_second(start.timestamp().as_second() - 1).ok()?;
        let winter = zone.to_offset_info(before);
        let standard = winter.offset().seconds();
        if start.dst() != Dst::Yes
            || end.dst() != Dst::No
            || winter.dst() != Dst::No
            || end.offset().seconds() != standard
        {
            return None;
        }
        let first = zigbee_seconds(start.timestamp().as_second());
        // The ZCL end is inclusive. IANA transitions name the first new second.
        let last = zigbee_seconds(end.timestamp().as_second() - 1);
        if first == INVALID || last == INVALID {
            return None;
        }
        Some(Self {
            standard,
            start: first,
            end: last,
            shift: start.offset().seconds() - standard,
        })
    }
}

pub(super) struct Server {
    zone: Option<TimeZone>,
    superseding: bool,
}

impl Server {
    pub(super) fn system() -> Self {
        let zone = TimeZone::try_system().ok().filter(|z| !z.is_unknown());
        Self {
            zone,
            superseding: false,
        }
    }

    pub(super) fn utc_offset_seconds(&self, now: Timestamp) -> Option<i32> {
        self.zone.as_ref().map(|zone| zone.to_offset(now).seconds())
    }

    pub(super) fn handle(
        &mut self,
        frame: Frame<'_>,
        now: Timestamp,
    ) -> Result<Option<Vec<u8>>, Outcome> {
        use GeneralCommand as Cmd;
        match frame.header.general_command() {
            Some(
                Cmd::ReadAttributes
                | Cmd::DiscoverAttributes
                | Cmd::WriteAttributes
                | Cmd::WriteAttributesUndivided
                | Cmd::WriteAttributesNoResponse,
            ) => {}
            _ => return Err(Outcome::UnsupportedCommand),
        }
        let payload = Payload::decode(frame).map_err(|_| Outcome::Malformed)?;
        let unix = now.as_second();
        let utc = zigbee_seconds(unix);
        let season = self.zone.as_ref().and_then(|z| Season::at(z, now));
        let local = self.utc_offset_seconds(now).map(|offset| zigbee_seconds(unix + i64::from(offset)));
        let status = 1 | (u32::from(season.is_some()) << 2) | (u32::from(self.superseding) << 3);
        // Signed offsets are sent in two's complement. This same table drives
        // reads, discovery and the set of attributes recognized on writes.
        let attrs: [(u16, u8, Option<u32>); 9] = [
            (0, 0xe2, Some(utc)),
            (1, 0x18, Some(status)),
            (2, 0x2b, season.as_ref().map(|s| s.standard as u32)),
            (3, 0x23, season.as_ref().map(|s| s.start)),
            (4, 0x23, season.as_ref().map(|s| s.end)),
            (5, 0x2b, season.as_ref().map(|s| s.shift as u32)),
            (
                6,
                0x23,
                season
                    .as_ref()
                    .map(|s| zigbee_seconds(unix + i64::from(s.standard))),
            ),
            (7, 0x23, local),
            (0xfffd, 0x21, Some(2)),
        ];
        let supported = |id: u16| attrs.iter().find(|a| a.0 == id && a.2.is_some());
        let mut body = Vec::new();
        let command = match payload {
            Payload::ReadAttributes(ids) => {
                for id in ids {
                    if body.len() + 3 > BODY_BUDGET {
                        break;
                    }
                    body.extend(id.to_le_bytes());
                    if let Some(&(_, ty, Some(value))) = supported(id) {
                        let size = match ty {
                            0x18 => 1,
                            0x21 => 2,
                            _ => 4,
                        };
                        if body.len() + 2 + size <= BODY_BUDGET {
                            body.extend([0, ty]);
                            body.extend(&value.to_le_bytes()[..size]);
                        } else {
                            body.push(0x89);
                        }
                    } else {
                        body.push(UNSUPPORTED);
                    }
                }
                Cmd::ReadAttributesResponse
            }
            Payload::DiscoverAttributes(req) => {
                let mut attrs = attrs
                    .iter()
                    .filter(|(id, _, value)| *id >= req.start && value.is_some());
                body.push(0);
                for &(id, ty, _) in attrs.by_ref().take(usize::from(req.maximum)) {
                    body.extend(id.to_le_bytes());
                    body.push(ty);
                }
                body[0] = u8::from(attrs.next().is_none());
                Cmd::DiscoverAttributesResponse
            }
            Payload::WriteAttributes(items)
            | Payload::WriteAttributesUndivided(items)
            | Payload::WriteAttributesNoResponse(items) => {
                let mut superseding = self.superseding;
                for item in items {
                    let status = match item.id {
                        1 if item.value.data_type().id() != 0x18 => 0x8d,
                        1 => {
                            superseding = item.value.as_bytes()[0] & 8 != 0;
                            continue;
                        }
                        id if supported(id).is_some() => READ_ONLY,
                        _ => UNSUPPORTED,
                    };
                    body.push(status);
                    body.extend(item.id.to_le_bytes());
                }
                if body.is_empty()
                    || frame.header.general_command() != Some(Cmd::WriteAttributesUndivided)
                {
                    self.superseding = superseding;
                }
                if frame.header.general_command() == Some(Cmd::WriteAttributesNoResponse) {
                    return Ok(None);
                }
                if body.is_empty() {
                    body.push(0);
                }
                Cmd::WriteAttributesResponse
            }
            _ => unreachable!(),
        };
        Ok(Some(responses::reply(frame.header, command, &body)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zigbee_zcl::attributes::Status;
    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }
    fn read(server: &mut Server, now: Timestamp, ids: &[u16]) -> Vec<Option<(u8, u32)>> {
        let mut request = vec![0x10, 0xa4, 0];
        for id in ids {
            request.extend(id.to_le_bytes());
        }
        let wire = server
            .handle(Frame::decode(&request).unwrap(), now)
            .ok()
            .unwrap()
            .unwrap();
        assert_eq!(wire[..3], [0x18, 0xa4, 1]);
        let Payload::ReadAttributesResponse(attrs) =
            Payload::decode(Frame::decode(&wire).unwrap()).unwrap()
        else {
            panic!()
        };
        assert_eq!(attrs.iter().map(|a| a.id).collect::<Vec<_>>(), ids);
        attrs
            .into_iter()
            .map(|a| match a.result {
                Status::Success(v) => {
                    let mut n = [0; 4];
                    n[..v.as_bytes().len()].copy_from_slice(v.as_bytes());
                    Some((v.data_type().id(), u32::from_le_bytes(n)))
                }
                Status::Error(e) => {
                    assert_eq!(e.get(), UNSUPPORTED);
                    None
                }
            })
            .collect()
    }
    #[test]
    fn zone_attributes_reconstruct_local_time_at_both_boundaries() {
        for (zone, start, end, base, shift) in [
            (
                "Europe/London",
                "2026-03-29T01:00:00Z",
                "2026-10-25T01:00:00Z",
                0,
                3600,
            ),
            (
                "Australia/Lord_Howe",
                "2025-10-04T15:30:00Z",
                "2026-04-04T15:00:00Z",
                37800,
                1800,
            ),
        ] {
            let mut server = Server {
                zone: Some(TimeZone::get(zone).unwrap()),
                superseding: false,
            };
            let first = ts(start).as_second();
            let last = ts(end).as_second() - 1;
            for unix in [first - 1, first, last, last + 1] {
                let values = read(
                    &mut server,
                    Timestamp::from_second(unix).unwrap(),
                    &[0, 1, 2, 3, 4, 5, 6, 7],
                );
                let vals = values.into_iter().map(|v| v.unwrap().1).collect::<Vec<_>>();
                let utc = zigbee_seconds(unix);
                let summer = (first..=last).contains(&unix);
                assert_eq!(
                    (vals[0], vals[1], vals[2], vals[5], vals[6]),
                    (utc, 5, base, shift, utc + base)
                );
                if summer {
                    assert_eq!(
                        (vals[3], vals[4]),
                        (zigbee_seconds(first), zigbee_seconds(last))
                    );
                }
                assert_eq!((vals[3]..=vals[4]).contains(&utc), summer);
                assert_eq!(vals[7], utc + base + if summer { shift } else { 0 });
            }
        }
    }
    #[test]
    fn fixed_offset_epoch_unknown_attributes_and_atomic_writes() {
        let epoch = ts("2000-01-01T00:00:00Z");
        let mut clock = Server {
            zone: Some(TimeZone::UTC),
            superseding: false,
        };
        assert_eq!(
            read(&mut clock, epoch, &[0, 2, 7, 3, 4, 5, 8, 0xfffd]),
            vec![
                Some((0xe2, 0)),
                Some((0x2b, 0)),
                Some((0x23, 0)),
                Some((0x23, INVALID)),
                Some((0x23, INVALID)),
                Some((0x2b, 0)),
                None,
                Some((0x21, 2))
            ]
        );
        assert_eq!(zigbee_seconds(EPOCH - 1), INVALID);
        let mut send = |bytes: &[u8]| {
            clock
                .handle(Frame::decode(bytes).unwrap(), epoch)
                .ok()
                .unwrap()
        };
        assert_eq!(
            send(&[0, 1, 3, 1, 0, 0x18, 8, 0, 0, 0xe2, 0, 0, 0, 0]).unwrap(),
            [0x18, 1, 4, READ_ONLY, 0, 0]
        );
        assert_eq!(
            send(&[0, 2, 0, 1, 0]).unwrap(),
            [0x18, 2, 1, 1, 0, 0, 0x18, 5]
        );
        assert!(send(&[0, 3, 5, 1, 0, 0x18, 0xff]).is_none());
        assert_eq!(
            send(&[0, 4, 0, 1, 0]).unwrap(),
            [0x18, 4, 1, 1, 0, 0, 0x18, 13]
        );
        assert_eq!(
            send(&[0, 5, 12, 6, 0, 2]).unwrap(),
            [0x18, 5, 13, 0, 6, 0, 0x23, 7, 0, 0x23]
        );
    }
}
