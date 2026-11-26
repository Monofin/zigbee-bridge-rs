//! Destination authority at the host's last cancellable unicast boundary.
use super::{Callback, Ezsp, MemberUpdate, ezsp19};
use crate::bridge::addresses::{AddressLease, Authority, valid_pair};
use crate::{coordinator::DEADLINE, interview::incoming};
use anyhow::{Result, ensure};
use std::time::Duration;
use std::{
    collections::BTreeMap,
    fmt,
    sync::{Arc, Mutex},
};
use tokio::time::Instant;

#[derive(Clone, Debug)]
pub(crate) struct Destination {
    ieee: u64,
    short: u16,
    session: Arc<()>,
    assignment: Option<AddressLease>,
    pre_admission: Option<Arc<()>>,
}

impl Destination {
    pub(crate) fn ieee(&self) -> u64 {
        self.ieee
    }
    pub(crate) fn short(&self) -> u16 {
        self.short
    }
    pub(crate) fn same_assignment(&self, other: &Self) -> bool {
        self.ieee == other.ieee
            && self.short == other.short
            && Arc::ptr_eq(&self.session, &other.session)
            && self.assignment == other.assignment
            && match (&self.pre_admission, &other.pre_admission) {
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                (None, None) => true,
                _ => false,
            }
    }

    #[cfg(test)]
    pub(crate) fn fixture(ieee: u64, short: u16) -> Self {
        Self {
            ieee,
            short,
            session: Arc::new(()),
            assignment: None,
            pre_admission: None,
        }
    }
}

#[derive(Debug)]
pub(crate) struct DestinationRetired;

impl fmt::Display for DestinationRetired {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("destination assignment changed before submission or completion")
    }
}
impl std::error::Error for DestinationRetired {}

// The ASH decoder can accept callbacks during another command. Inspect the
// buffered evidence without consuming or reordering it. ACK flow stays in ASH.
fn affects(cb: &Callback, protocol: u8, ieee: u64, short: u16) -> bool {
    match (cb.id, cb.payload.as_slice()) {
        (0x19, status) => {
            // NETWORK_OPENED/CLOSED only changes permit-join.
            if protocol == 19 {
                !matches!(status, [0x18 | 0x19, 0x0c, 0, 0])
            } else {
                !matches!(status, [0x9c | 0x9d])
            }
        }
        (0x7c, [lo, hi]) => u16::from_le_bytes([*lo, *hi]) == short,
        (0x24, _) => cb.member_update().is_some_and(|(peer, node, change)| {
            if change == MemberUpdate::Left {
                peer == ieee && node == short
            } else {
                peer == ieee || node == short
            }
        }),
        (0x45, p) => {
            let normalized;
            let (p, source) = if protocol == 19 {
                let Ok(Some(c)) = ezsp19::callback(0x45, p) else {
                    return true;
                };
                normalized = c;
                (normalized.payload.as_slice(), ezsp19::source_eui(0x45, p))
            } else {
                (p, None)
            };
            let Ok(msg) = incoming(p) else { return true };
            let disagrees =
                |id: u64, node: u16| (id == ieee && node != short) || (node == short && id != ieee);
            if source.is_some_and(|id| valid_pair(id, msg.sender) && disagrees(id, msg.sender)) {
                return true;
            }
            if (msg.aps.profile, msg.aps.src, msg.aps.dst) != (0, 0, 0) {
                return false;
            }
            let pair = match (msg.aps.cluster, msg.body) {
                (0x13, [_, lo, hi, id @ .., _]) => <[u8; 8]>::try_from(id)
                    .ok()
                    .map(|id| (u64::from_le_bytes(id), u16::from_le_bytes([*lo, *hi]))),
                (0x8000 | 0x8001, [_, 0, id @ .., lo, hi]) => <[u8; 8]>::try_from(id)
                    .ok()
                    .map(|id| (u64::from_le_bytes(id), u16::from_le_bytes([*lo, *hi]))),
                _ => None,
            };
            pair.is_some_and(|(id, node)| {
                valid_pair(id, node) && node == msg.sender && disagrees(id, node)
            })
        }
        _ => false,
    }
}

/// Keep retired short-address / sequence tuples apart for the bridge's
/// response window. Replies to peer-initiated requests must echo their TSN.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct WireId {
    short: u16,
    profile: u16,
    cluster: u16,
    local: u8,
    remote: u8,
    seq: u8,
}

impl WireId {
    fn request(args: &[u8]) -> Option<(Self, bool)> {
        if args.len() < 17 || args.len() - 16 != usize::from(args[15]) {
            return None;
        }
        let profile = u16::from_le_bytes([args[3], args[4]]);
        let cluster = u16::from_le_bytes([args[5], args[6]]);
        let tag = args[14];
        let seq = if profile == 0 {
            if cluster & 0x8000 != 0 {
                return None;
            }
            args[16]
        } else {
            let frame = zigbee_zcl::Frame::decode(&args[16..]).ok()?;
            // A 0xff-tagged application reply has no host pending transaction.
            // Include coordinator-originated foundation reads from interview.
            if tag == 0xff
                && !(frame.header.frame_type == zigbee_zcl::FrameType::General
                    && frame.header.direction == zigbee_zcl::Direction::ClientToServer
                    && matches!(
                        frame.header.command_id,
                        0x00 | 0x02 | 0x06 | 0x08 | 0x0c | 0x11 | 0x13 | 0x15
                    ))
            {
                return None;
            }
            frame.header.sequence
        };
        Some((
            Self {
                short: u16::from_le_bytes([args[1], args[2]]),
                profile,
                cluster,
                local: args[7],
                remote: args[8],
                seq,
            },
            tag != 0xff,
        ))
    }
}

#[derive(Default)]
pub(super) struct RecentRequests {
    entries: BTreeMap<WireId, (Destination, Instant)>,
    next_prune: Option<Instant>,
}

impl RecentRequests {
    fn available(&self, key: &WireId, target: &Destination, unique: bool, now: Instant) -> bool {
        self.entries
            .get(key)
            .is_none_or(|(old, until)| *until <= now || (!unique && target.same_assignment(old)))
    }
    fn record(&mut self, key: WireId, target: &Destination, now: Instant) {
        if self.next_prune.is_none_or(|t| t <= now) {
            self.entries.retain(|_, (_, t)| *t > now);
            self.next_prune = Some(now + Duration::from_secs(1));
        }
        // Covers the twelve-second bind and eight-second application windows.
        self.entries
            .insert(key, (target.clone(), now + Duration::from_secs(16)));
    }
}

#[derive(Debug)]
pub(crate) struct SequenceInUse;
impl fmt::Display for SequenceInUse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("radio transaction sequence is still reserved")
    }
}
impl std::error::Error for SequenceInUse {}

pub(crate) fn locally_refused(error: &anyhow::Error) -> bool {
    error.is::<DestinationRetired>() || error.is::<SequenceInUse>()
}

#[derive(Default)]
pub(super) struct Standalone(Mutex<BTreeMap<(u64, u16), Arc<()>>>);
impl Standalone {
    fn capture(&self, ieee: u64, short: u16) -> Arc<()> {
        self.0
            .lock()
            .unwrap()
            .entry((ieee, short))
            .or_default()
            .clone()
    }
    fn current(&self, t: &Destination) -> bool {
        let Some(original) = &t.pre_admission else {
            return false;
        };
        self.0
            .lock()
            .unwrap()
            .get(&(t.ieee, t.short))
            .is_some_and(|a| Arc::ptr_eq(a, original))
    }
    pub(super) fn observe(&self, cb: &Callback, protocol: u8) {
        self.0
            .lock()
            .unwrap()
            .retain(|&(id, n), _| !affects(cb, protocol, id, n));
    }
}

impl Ezsp<'_> {
    pub(crate) fn manage_addresses(&mut self, authority: Authority) {
        if self.authority.replace(authority).is_some() {
            self.incarnation = Arc::new(());
            self.standalone = Standalone::default();
        }
    }

    pub(crate) fn adapter_mapping(&self, ieee: u64, short: u16) -> bool {
        if !valid_pair(ieee, short) {
            return false;
        }
        self.standalone
            .0
            .lock()
            .unwrap()
            .retain(|&(id, n), _| (ieee == id) == (short == n));
        self.authority
            .as_ref()
            .is_none_or(|a| a.check_adapter(ieee, short))
    }

    pub(crate) fn destination(&self, ieee: u64, short: u16) -> Result<Destination> {
        ensure!(valid_pair(ieee, short), DestinationRetired);
        let assignment = match &self.authority {
            Some(authority) => Some(authority.lease(ieee, short).ok_or(DestinationRetired)?),
            None => None,
        };
        let pre_admission = self
            .authority
            .is_none()
            .then(|| self.standalone.capture(ieee, short));
        Ok(Destination {
            ieee,
            short,
            assignment,
            session: self.incarnation.clone(),
            pre_admission,
        })
    }

    pub(crate) fn adopt(&self, original: &Destination) -> Result<Destination> {
        ensure!(
            Arc::ptr_eq(&original.session, &self.incarnation) && self.standalone.current(original),
            DestinationRetired
        );
        let target = self.destination(original.ieee, original.short)?;
        ensure!(self.current(&target), DestinationRetired);
        Ok(target)
    }

    pub(crate) fn current(&self, target: &Destination) -> bool {
        Arc::ptr_eq(&target.session, &self.incarnation)
            && match (&self.authority, &target.assignment) {
                (Some(a), Some(lease)) => a.is_current(lease),
                (None, None) => self.standalone.current(target),
                _ => false,
            }
            && !self
                .link
                .callbacks
                .iter()
                .any(|c| affects(c, self.protocol, target.ieee, target.short))
    }

    pub(crate) fn application_sequence_free(
        &self,
        target: &Destination,
        profile: u16,
        cluster: u16,
        ep: (u8, u8),
        seq: u8,
    ) -> bool {
        let key = WireId {
            short: target.short,
            profile,
            cluster,
            local: ep.0,
            remote: ep.1,
            seq,
        };
        self.recent_requests
            .available(&key, target, true, Instant::now())
    }

    pub(crate) async fn submit_to(&mut self, target: &Destination, args: &[u8]) -> Result<Vec<u8>> {
        ensure!(
            args.first() == Some(&0) && args.get(1..3) == Some(&target.short.to_le_bytes()),
            "unicast destination differs from planned peer"
        );
        ensure!(self.current(target), DestinationRetired);
        // exchange() submits its first serial frame synchronously before its
        // first suspension. Only this task decodes ASH input in this session.
        let wire = WireId::request(args);
        if let Some((key, unique)) = wire {
            ensure!(
                self.recent_requests
                    .available(&key, target, unique, Instant::now()),
                SequenceInUse
            );
        }
        let result = self.runtime_checked(0x34, args, DEADLINE).await;
        if let Some((key, _)) = wire
            && !matches!(result.as_deref(),Ok([status,_]) if *status!=0)
        {
            self.recent_requests.record(key, target, Instant::now());
        }
        result
    }

    pub(crate) async fn probe_ieee(&mut self, short: u16, args: &[u8]) -> Result<Vec<u8>> {
        let [lo, hi] = short.to_le_bytes();
        ensure!(
            (1..0xfff8).contains(&short)
                && matches!(args,
            [0,l,h,0,0,1,0,0,0,_,_,0,0,_,_,5,_,ql,qh,0,0] if (*l,*h,*ql,*qh)==(lo,hi,lo,hi)),
            "invalid IEEE address discovery request"
        );
        self.runtime_checked(0x34, args, DEADLINE).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn late_reply_window_survives_same_short_reassignment_without_blocking_other_peers() {
        let old = Destination::fixture(1, 0x1234);
        let replacement = Destination::fixture(1, 0x1234);
        let key = WireId {
            short: old.short(),
            profile: 0x104,
            cluster: 6,
            local: 1,
            remote: 4,
            seq: 17,
        };
        let mut recent = RecentRequests::default();
        let now = Instant::now();
        recent.record(key, &old, now);

        assert!(!recent.available(&key, &old, true, now));
        assert!(!recent.available(&key, &replacement, false, now));
        assert!(recent.available(
            &WireId {
                short: 0x1235,
                ..key
            },
            &replacement,
            true,
            now
        ));
        assert!(recent.available(&key, &replacement, true, now + Duration::from_secs(16)));
    }

    #[test]
    fn only_a_membership_change_retires_a_buffered_destination() {
        let mut payload = vec![1, 2];
        payload.extend(11u64.to_le_bytes());
        for (update, decision, change) in [
            (3, 2, None), (3, 3, None), (1, 2, None), (0xff, 0, None),
            (0, 3, Some(MemberUpdate::SecuredRejoin)),
            (3, 0, Some(MemberUpdate::Admission)), (2, 3, Some(MemberUpdate::Left)),
        ] {
            let mut wire = payload.clone();
            wire.extend([update, decision, 0, 0]);
            let cb = Callback { id: 0x24, payload: wire };
            assert_eq!(cb.member_update().map(|(_, _, event)| event), change);
            assert_eq!(affects(&cb, 19, 11, 0x0201), change.is_some());
        }
    }

    #[test]
    fn buffered_leave_spares_both_current_owners_after_a_move() {
        let mut payload = vec![1, 2];
        payload.extend(11u64.to_le_bytes());
        payload.extend([2, 0, 0, 0]);
        let leave = Callback { id: 0x24, payload };
        assert!(affects(&leave, 13, 11, 0x0201));
        assert!(!affects(&leave, 13, 22, 0x0201));
        assert!(!affects(&leave, 13, 11, 0x0202));
    }

    #[test]
    fn application_replies_can_echo_a_reserved_request_sequence() {
        let mut args = [
            0, 1, 2, 4, 1, 6, 0, 1, 1, 0, 0, 0, 0, 0, 0xff, 5, 0x18, 1, 0x0b, 1, 0,
        ];
        assert!(WireId::request(&args).is_none());
        args[16] = 0;
        args[18] = 0;
        assert_eq!(WireId::request(&args).unwrap().0.seq, 1);
    }
}
