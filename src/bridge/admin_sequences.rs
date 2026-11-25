//! ZDO and ZCL transaction identifiers that may still attract a late reply.
use anyhow::{Result, bail};
use std::{collections::BTreeMap, ops::Range, time::Duration};
use tokio::time::Instant;

// Confirmed replies may repeat while the original 12-second response window
// drains. An ambiguous identifier remains retired for the whole radio session.
const CONFIRMED_HOLD: Duration = Duration::from_secs(12);

#[derive(Clone, Copy)]
pub(in crate::bridge) struct Reservation {
    key: (u16, u8, u16),
    pub first: u8,
    count: u8,
    start: u8,
    width: u8,
}

impl Reservation {
    fn tags(self) -> impl Iterator<Item = (u16, u8, u16, u8)> {
        let (short, endpoint, cluster) = self.key;
        (0..self.count).map(move |n| {
            let offset =
                (u16::from(self.first - self.start) + u16::from(n)) % u16::from(self.width);
            (short, endpoint, cluster, self.start + offset as u8)
        })
    }
}

#[derive(Default)]
pub(in crate::bridge) struct Sequences {
    cursor: u8,
    // A missing deadline reserves the sequence until a definite response or
    // the end of the session. Leave/rejoin does not disambiguate a wire TSN.
    slots: BTreeMap<(u16, u8, u16, u8), Option<Instant>>,
}

impl Sequences {
    pub fn reserve(
        &mut self,
        key: (u16, u8, u16),
        range: Range<u8>,
        count: u8,
    ) -> Result<Reservation> {
        self.reserve_at(key, range, count, Instant::now())
    }

    fn reserve_at(
        &mut self,
        key: (u16, u8, u16),
        range: Range<u8>,
        count: u8,
        now: Instant,
    ) -> Result<Reservation> {
        self.slots.retain(|_, until| until.is_none_or(|t| t > now));
        let width = range.end - range.start;
        assert!(count > 0 && count <= width);
        for n in 0..width {
            let offset = (u16::from(self.cursor) + u16::from(n)) % u16::from(width);
            let batch = Reservation {
                key,
                first: range.start + offset as u8,
                count,
                start: range.start,
                width,
            };
            if batch.tags().all(|tag| !self.slots.contains_key(&tag)) {
                self.slots.extend(batch.tags().map(|tag| (tag, None)));
                self.cursor = ((offset + u16::from(count)) % u16::from(width)) as u8;
                return Ok(batch);
            }
        }
        bail!("no unambiguous transaction sequence available for this peer and cluster")
    }

    pub fn confirmed(&mut self, batch: Reservation) {
        let until = Instant::now() + CONFIRMED_HOLD;
        self.slots
            .extend(batch.tags().map(|tag| (tag, Some(until))));
    }

    pub fn unsent(&mut self, batch: Reservation) {
        for tag in batch.tags() {
            self.slots.remove(&tag);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ambiguous_ids_stay_retired_while_confirmed_slots_become_available() -> Result<()> {
        let mut ids = Sequences::default();
        let key = (0x1234, 2, 0x101);
        let ambiguous = ids.reserve(key, 224..240, 3)?;
        let confirmed = ids.reserve(key, 224..240, 3)?;
        ids.confirmed(confirmed);
        for _ in 0..10 {
            ids.reserve(key, 224..240, 1)?;
        }
        assert!(ids.reserve(key, 224..240, 1).is_err());
        assert!(ids.reserve((0x4321, 2, 0x101), 224..240, 3).is_ok());
        let reopened = ids.reserve_at(key, 224..240, 3, Instant::now() + CONFIRMED_HOLD)?;
        assert_eq!(reopened.first, confirmed.first);
        assert_ne!(reopened.first, ambiguous.first);
        Ok(())
    }
}
