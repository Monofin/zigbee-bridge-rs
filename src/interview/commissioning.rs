//! Lifetime of the adapter's wildcard key for one permit-join window.
//! Ember clears the whole transient table, so account for overlapping joins.
use super::{Callback, Ezsp, Result, clear_transient_keys, next};
use std::{collections::BTreeMap, time::Duration};
use tokio::time::Instant;

const VERIFY_LIMIT: Duration = Duration::from_secs(60);
const KEY_ESTABLISHMENT: u16 = 0x009b;
const TC_REQUESTER_VERIFY_KEY_SUCCESS: u8 = 0x34;

#[derive(Default)]
pub(crate) struct Commissioning {
    peers: BTreeMap<[u8; 8], bool>,
    deadline: Option<Instant>,
    collecting: bool,
}

impl Commissioning {
    pub fn opened() -> Self {
        Self {
            collecting: true,
            ..Self::default()
        }
    }

    pub fn admissions_closed(&mut self, ieee: Option<[u8; 8]>, now: Instant) {
        if let Some(ieee) = ieee {
            self.peers.entry(ieee).or_insert(false);
        }
        self.deadline = Some(now + VERIFY_LIMIT);
    }

    pub(crate) fn observe(&mut self, cb: &Callback) {
        if !self.collecting {
            return;
        }
        if let Some((peer, _, crate::ash::MemberUpdate::Admission)) = cb.member_update() {
            self.peers.insert(peer.to_le_bytes(), false);
        }
        if let (KEY_ESTABLISHMENT, [ieee @ .., status]) = (cb.id, cb.payload.as_slice())
            && let Ok(peer) = <[u8; 8]>::try_from(ieee)
            && let Some(verified) = self.peers.get_mut(&peer)
        {
            println!("Commissioning peer key status {status:#04x}");
            if *status == TC_REQUESTER_VERIFY_KEY_SUCCESS {
                *verified = true;
            }
        }
    }

    pub(crate) fn admitted(&self,ieee:&[u8;8])->bool { self.collecting && self.peers.contains_key(ieee) }

    fn reason(&self, now: Instant) -> Option<&'static str> {
        let end = self.deadline?;
        if self.peers.is_empty() {
            return Some("join window empty");
        }
        if self.peers.values().all(|v| *v) {
            Some("link key verified")
        } else if now >= end {
            Some("verification timed out")
        } else {
            None
        }
    }

    pub(crate) async fn clean_if_due(&mut self, link: &mut Ezsp<'_>) -> Result<()> {
        let now = Instant::now();
        // Closing permit-join can queue a second accepted peer behind the
        // first peer's verification. Observe buffered joins before clearing.
        if link.has_buffered_callbacks() && self.deadline.is_some_and(|end| now < end) {
            return Ok(());
        }
        if let Some(reason) = self.reason(now) {
            clear_transient_keys(link).await?;
            self.deadline = None;
            self.collecting = false;
            println!("Commissioning cleanup: {reason}.");
        }
        Ok(())
    }

    pub async fn receive(
        &mut self,
        link: &mut Ezsp<'_>,
        until: Instant,
    ) -> Result<Option<Callback>> {
        loop {
            self.clean_if_due(link).await?;
            let end = self.deadline.map_or(until, |d| d.min(until));
            if let Some(cb) = next(link, end).await? {
                self.observe(&cb);
                self.clean_if_due(link).await?;
                return Ok(Some(cb));
            }
            self.clean_if_due(link).await?;
            if Instant::now() >= until {
                return Ok(None);
            }
        }
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn status(peer: u8, value: u8) -> Callback {
        let mut payload = vec![peer; 8];
        payload.push(value);
        Callback {
            id: KEY_ESTABLISHMENT,
            payload,
        }
    }
    fn joined(peer: u8) -> Callback {
        let mut payload = vec![1, 0];
        payload.extend([peer; 8]);
        payload.extend([1, 0, 0, 0]);
        Callback { id: 0x24, payload }
    }
    #[test]
    fn only_this_requesters_verified_key_completes() {
        let now = Instant::now();
        let mut c = Commissioning::opened();
        c.admissions_closed(Some([1; 8]), now);
        for cb in [
            status(2, 0x34),
            status(1, 6),
            status(1, 0x65),
            status(1, 0x33),
        ] {
            c.observe(&cb);
        }
        assert!(c.reason(now).is_none());
        assert_eq!(c.reason(now + VERIFY_LIMIT), Some("verification timed out"));
        c.observe(&status(1, 0x34));
        assert_eq!(c.reason(now), Some("link key verified"));
    }
    #[test]
    fn early_verification_and_overlapping_join() {
        let now = Instant::now();
        let mut c = Commissioning::opened();
        c.observe(&joined(1));
        c.observe(&status(1, 0x34));
        c.observe(&joined(2));
        assert!(c.reason(now).is_none());
        c.admissions_closed(Some([1; 8]), now);
        assert!(c.reason(now).is_none());
        c.observe(&status(2, 0x34));
        assert_eq!(c.reason(now), Some("link key verified"));
    }
}
