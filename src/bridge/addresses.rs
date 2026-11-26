//! Session-local Zigbee address discovery (ZDO 0x0000/0x0001).
//! Only new discovery requests wait for the previous session's local exchange
//! window at startup. A fresh cold-table command has at most two such windows
//! including discovery; an earlier caller deadline always wins. Zigbee's
//! eight-bit reply ID cannot identify a previous process or a delayed copy
//! arriving after the assumed reply lifetime.
use super::zdo;
use crate::{ash::{Callback, Ezsp}, interview::Incoming, mqtt::ReceivedCommand, registry::Incarnation};
use anyhow::Result;
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};
use tokio::time::Instant;

const WAIT: Duration = Duration::from_secs(8);
const SPACING: Duration = Duration::from_millis(250);
const CAPACITY: usize = 32;
const MAX_AGE: Duration = Duration::from_secs(300);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub(super) enum Query {
    Network(u64),
    Ieee(u16),
}

pub(super) struct Waiting {
    pub(super) peer: super::Identity,
    pub(super) command: ReceivedCommand,
    pub(super) refresh: Option<&'static str>,
    pub(super) until: Option<Instant>,
    pub(super) original: Option<crate::ash::Destination>,
}

struct Probe {
    key: Query,
    until: Instant,
    sequence: Option<u8>,
    issued: u64,
    observations: Vec<(Instant, u64, Callback)>,
}

/// Permission to attribute an effect to exactly one assignment in one session.
/// It carries no persistent or wire identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AddressLease {
    ieee: u64,
    short: u16,
    incarnation: Incarnation,
}

struct NeighborAnswer {
    sequence: u8,
    ieee: u64,
    short: u16,
    until: Instant,
}

struct Assignment {
    lease: AddressLease,
    // Some records a verified pair for this session. The deadline bounds its use for outbound routing.
    routing_until: Option<Instant>,
    usable_until: Instant,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct Revoked {
    pub(crate) ieee: Option<u64>,
    pub(crate) short: Option<u16>,
}

/// Owned by the serving loop. Teardown revokes its handles even if a serial
/// writer or delayed operation still holds a clone of the authority.
#[derive(Default)]
pub(super) struct Addresses {
    authority: Authority,
}

#[derive(Clone)]
pub(crate) struct Authority(Arc<Mutex<State>>);

#[derive(Default)]
struct State {
    by_ieee: BTreeMap<u64, Assignment>,
    by_node: BTreeMap<u16, u64>,
    departed: BTreeSet<u64>,
    disputed: BTreeSet<u16>,
    // An unknown forward probe carries no short address until its reply.
    // Remember which probes predate an invalidation of that particular short.
    retired_at: BTreeMap<u16, (u64, Instant)>,
    revocations: BTreeSet<Revoked>,
    probes: VecDeque<Probe>,
    cooldown: BTreeMap<Query, Instant>,
    // A broadcast may draw different successful replies during one exchange.
    // Remember neighbor answers until the probe deadline, including after its queue drains.
    neighbor_answers: BTreeMap<Query, NeighborAnswer>,
    commands: VecDeque<Waiting>,
    // Keep duplicate detection local to recovered input and its original lease.
    recovered: VecDeque<(AddressLease, Instant, Callback)>,
    issued: u64,
    startup_until: Option<Instant>,
    next_send: Option<Instant>,
    closed: bool,
}

impl Drop for Addresses {
    fn drop(&mut self) {
        self.authority.close();
    }
}

impl std::ops::Deref for Addresses {
    type Target = Authority;
    fn deref(&self) -> &Authority {
        &self.authority
    }
}

impl Addresses {
    /// Restore only durable membership exclusions. Startup does not hydrate
    /// any live address authority from a snapshot.
    pub(super) fn restore_departed(&mut self, ids: impl IntoIterator<Item = u64>) {
        let mut state = self.authority.lock();
        for ieee in ids {
            if ieee != 0 && ieee != u64::MAX {
                if state.by_ieee.contains_key(&ieee) {
                    state.forget_peer(ieee, Instant::now());
                }
                state.departed.insert(ieee);
            }
        }
    }

    pub(super) fn authority(&self) -> Authority {
        self.authority.clone()
    }
}

pub(crate) fn valid_pair(ieee: u64, node: u16) -> bool {
    ieee != 0 && ieee != u64::MAX && (1..0xfff8).contains(&node)
}

// Callers have decoded both normalized incomingMessageHandler payloads.
fn same_aps_frame(a: &Callback, b: &Callback) -> bool {
    a.payload[..12] == b.payload[..12] && a.payload[14..] == b.payload[14..]
}

impl State {
    fn node(&self, ieee: u64, now: Instant) -> Option<u16> {
        self.by_ieee
            .get(&ieee)
            .filter(|a| a.routing_until.is_some_and(|t| t > now))
            .map(|a| a.lease.short)
    }

    fn ieee(&self, node: u16, now: Instant) -> Option<u64> {
        let ieee = *self.by_node.get(&node)?;
        (self.node(ieee, now) == Some(node)).then_some(ieee)
    }


    fn departed(&self, ieee: u64) -> bool {
        self.departed.contains(&ieee)
    }

    fn retire_short(&mut self, node: u16, now: Instant) {
        if (1..0xfff8).contains(&node) {
            self.disputed.insert(node);
            self.retired_at.insert(node, (self.issued, now));
        }
        self.probes.retain(|p| p.key != Query::Ieee(node));
        self.recovered.retain(|(lease, _, _)| lease.short != node);
        self.neighbor_answers.retain(|_, answer| answer.short != node);
    }

    fn forget_peer(&mut self, ieee: u64, now: Instant) {
        let short = self.by_ieee.remove(&ieee).map(|a| a.lease.short);
        if let Some(node) = short {
            self.by_node.remove(&node);
            self.retire_short(node, now);
        }
        self.revocations.insert(Revoked {
            ieee: Some(ieee),
            short,
        });
        self.commands.retain(|w| w.peer.0 != ieee);
        self.probes.retain(|p| p.key != Query::Network(ieee));
    }

    fn conflict(&mut self, node: u16, now: Instant) {
        if let Some(&ieee) = self.by_node.get(&node) {
            self.forget_peer(ieee, now);
        } else {
            self.retire_short(node, now);
            self.revocations.insert(Revoked {
                ieee: None,
                short: Some(node),
            });
        }
    }

    fn dispute(&mut self, ieee: u64, node: u16, now: Instant) {
        self.forget_peer(ieee, now);
        self.conflict(node, now);
    }

    fn membership(
        &mut self,
        ieee: u64,
        node: u16,
        admitted: bool,
        known: bool,
        now: Instant,
    ) -> bool {
        // A late leave naming a different assignment cannot retire this member
        // or the replacement which now owns the reported short.
        if self.closed || !valid_pair(ieee, node) || (!admitted && self.contradicts(ieee, node)) {
            return false;
        }
        self.forget_peer(ieee, now);
        self.conflict(node, now);
        if admitted {
            self.departed.remove(&ieee);
            self.learn(ieee, node, now);
        } else if known {
            self.departed.insert(ieee);
        }
        true
    }

    // An adapter hit can serve this operation. It cannot clear a conflict or
    // extend the lifetime of address evidence obtained over the air.
    fn adapter_agrees(&self, ieee: u64, node: u16) -> bool {
        !self.closed
            && valid_pair(ieee, node)
            && !self.departed(ieee)
            && !self.disputed.contains(&node)
            && !self.contradicts(ieee, node)
    }

    fn disputed(&self, node: u16) -> bool {
        self.disputed.contains(&node)
    }

    fn check_adapter(&mut self, ieee: u64, node: u16) -> bool {
        if self.closed {
            return false;
        }
        if valid_pair(ieee, node) && self.contradicts(ieee, node) {
            self.dispute(ieee, node, Instant::now());
            return false;
        }
        if !self.adapter_agrees(ieee, node) {
            return false;
        }
        self.assign(ieee, node, Instant::now());
        true
    }

    fn assign(&mut self, ieee: u64, node: u16, now: Instant) -> &mut Assignment {
        self.by_node.insert(node, ieee);
        let assignment = self.by_ieee.entry(ieee).or_insert_with(|| Assignment {
            lease: AddressLease {
                ieee,
                short: node,
                incarnation: Incarnation::default(),
            },
            routing_until: None,
            usable_until: now + MAX_AGE,
        });
        if assignment.usable_until <= now {
            // Proof acquired after a lapse may authorize new work. An old
            // handle which already expired must stay retired.
            assignment.lease.incarnation = Incarnation::default();
            self.revocations.insert(Revoked {
                ieee: Some(ieee),
                short: Some(node),
            });
        }
        assignment.usable_until = now + MAX_AGE;
        assignment
    }

    fn lease(&self, ieee: u64, short: u16) -> Option<&AddressLease> {
        let current = self.by_ieee.get(&ieee)?;
        (current.usable_until > Instant::now()
            && current.lease.short == short
            && self.by_node.get(&short) == Some(&ieee))
        .then_some(&current.lease)
    }

    fn contradicts(&self, ieee: u64, node: u16) -> bool {
        self.by_node.get(&node).is_some_and(|old| *old != ieee)
            || self
                .by_ieee
                .get(&ieee)
                .is_some_and(|a| a.lease.short != node)
    }

    fn learn(&mut self, ieee: u64, node: u16, now: Instant) -> bool {
        self.neighbor_answers.retain(|_, answer| answer.ieee != ieee && answer.short != node);
        self.learn_route(ieee, node, now)
    }

    fn learn_route(&mut self, ieee: u64, node: u16, now: Instant) -> bool {
        if self.closed || !valid_pair(ieee, node) || self.departed(ieee) {
            return false;
        }
        if self.contradicts(ieee, node) {
            self.dispute(ieee, node, now);
            return false;
        }
        self.disputed.remove(&node);
        self.assign(ieee, node, now).routing_until = Some(now + MAX_AGE);
        self.probes
            .retain(|p| p.key != Query::Network(ieee) && p.key != Query::Ieee(node));
        true
    }

    fn enqueue(&mut self, key: Query, now: Instant) -> bool {
        let earliest = self.startup_until.map_or(now, |until| until.max(now));
        self.enqueue_until(key, now, earliest + WAIT)
    }

    fn enqueue_until(&mut self, key: Query, now: Instant, until: Instant) -> bool {
        self.expire(now);
        if self.closed {
            return false;
        }
        if self.probes.iter().any(|p| p.key == key) {
            return true;
        }
        if self.probes.len() >= CAPACITY || self.cooldown.get(&key).is_some_and(|t| *t > now) {
            return false;
        }
        // Leave capacity for user controls during a burst of unknown senders.
        if matches!(key, Query::Ieee(_))
            && self
                .probes
                .iter()
                .filter(|p| matches!(p.key, Query::Ieee(_)))
                .count()
                >= CAPACITY / 2
        {
            return false;
        }
        self.probes.push_back(Probe {
            key,
            until,
            sequence: None,
            issued: 0,
            observations: Vec::new(),
        });
        true
    }

    fn hold(&mut self, mut work: Waiting, now: Instant) {
        let ieee = work.peer.0;
        if !self.departed(ieee)
            && self.commands.len() < CAPACITY
            && self
                .commands
                .iter()
                .filter(|other| other.peer.0 == ieee)
                .count()
                < CAPACITY / 2
            && self.enqueue(Query::Network(ieee), now)
        {
            let until = self
                .probes
                .iter()
                .find(|p| p.key == Query::Network(ieee))
                .expect("admitted probe")
                .until;
            work.until = Some(work.until.map_or(until, |prior| prior.min(until)));
            self.commands.push_back(work);
        } else {
            eprintln!("MQTT command rejected: address discovery unavailable for {ieee:016x}");
        }
    }

    fn ready(&mut self, now: Instant) -> Option<Waiting> {
        let index = self
            .commands
            .iter()
            .position(|w| w.until.is_some_and(|t| t > now) && self.node(w.peer.0, now).is_some())?;
        self.commands.remove(index)
    }

    fn deadline(&self) -> Option<Instant> {
        let send = self
            .probes
            .iter()
            .any(|p| p.sequence.is_none())
            .then(|| self.next_send.unwrap_or_else(Instant::now));
        self.probes
            .iter()
            .map(|p| p.until)
            .chain(self.commands.iter().filter_map(|w| w.until))
            .chain(send)
            .min()
    }

    fn expire(&mut self, now: Instant) {
        self.retired_at
            .retain(|_, (_, t)| now.saturating_duration_since(*t) < WAIT);
        self.cooldown.retain(|_, t| *t > now);
        self.neighbor_answers.retain(|_, answer| answer.until > now);
        self.recovered.retain(|(_, until, _)| *until > now);
        self.probes.retain(|p| p.until > now);
        self.commands.retain(|w| {
            if w.until.is_some_and(|t| t <= now) {
                let id = w.peer.0;
                eprintln!("MQTT command expired awaiting address discovery for {id:016x}");
                return false;
            }
            true
        });
    }

    fn next(&mut self, now: Instant) -> Option<(Query, u8)> {
        self.expire(now);
        if self.next_send.is_some_and(|t| t > now) {
            return None;
        }
        let p = self.probes.iter_mut().find(|p| p.sequence.is_none())?;
        self.issued += 1;
        let sequence = self.issued as u8;
        p.sequence = Some(sequence);
        p.issued = self.issued;
        self.next_send = Some(now + SPACING);
        self.cooldown.insert(p.key, now + WAIT * 2);
        Some((p.key, sequence))
    }

    /// R23 §§2.4.3.1.1.2 and 2.4.3.1.2.2 allow a neighbor to answer for
    /// an end device. The address pair in the body belongs to the subject.
    fn response(
        &mut self,
        answer: AddressReply,
        sender: u16,
        source: Option<u64>,
        member: bool,
        now: Instant,
    ) -> Option<(u64, u16)> {
        let AddressReply { sequence, key, ieee, node } = answer;
        if self.closed || !valid_pair(ieee, node) {
            return None;
        }
        let neighbor = sender != node;
        if neighbor {
            if !(1..0xfff8).contains(&sender) || self.disputed(sender) || self.disputed(node)
                || source.is_some_and(|id| id == ieee || !self.adapter_agrees(id, sender)) {
                return None;
            }
        } else if source.is_some_and(|id| id != ieee) {
            return None;
        }
        if let Some(prior) = self.neighbor_answers.get(&key)
            && sequence == prior.sequence && now < prior.until {
            if (ieee, node) != (prior.ieee, prior.short) {
                self.conflict(prior.short, now);
                return None;
            }
            // The child can also answer while the original exchange is open.
            return (!neighbor && member && !self.departed(ieee)).then_some((ieee, node));
        }
        if !member || self.departed(ieee) {
            return None;
        }
        let probe = self.probes.iter().find(|p| {
            p.key == key && p.sequence == Some(sequence) && p.until > now
                && self.retired_at.get(&node).is_none_or(|(cutoff, _)| p.issued > *cutoff)
        })?;
        if neighbor {
            self.neighbor_answers.insert(key, NeighborAnswer { sequence, ieee, short: node, until: probe.until });
        }
        Some((ieee, node))
    }
}

struct AddressReply {
    sequence: u8,
    key: Query,
    ieee: u64,
    node: u16,
}

impl AddressReply {
    fn parse(m: &Incoming<'_>) -> Option<Self> {
        if m.aps.profile != 0 || (m.aps.src, m.aps.dst) != (0, 0) || !matches!(m.kind, 0 | 1) {
            return None;
        }
        let [seq, 0, rest @ ..] = m.body else {
            return None;
        };
        let fields = <&[u8; 10]>::try_from(rest).ok()?;
        let ieee = u64::from_le_bytes(fields[..8].try_into().unwrap());
        let node = u16::from_le_bytes([fields[8], fields[9]]);
        let key = match m.aps.cluster {
            0x8000 => Query::Network(ieee),
            0x8001 => Query::Ieee(node),
            _ => return None,
        };
        Some(Self {
            sequence: *seq,
            key,
            ieee,
            node,
        })
    }
}

impl Default for Authority {
    fn default() -> Self {
        Self::starting(Instant::now())
    }
}

impl Authority {
    fn starting(now: Instant) -> Self {
        // R23 §2.4.2.8.1 requires consecutive eight-bit ZDP transaction numbers.
        // The preceding process may have used any number. Let its local
        // discovery window expire before issuing a new probe; cached adapter
        // lookups, device announcements and unrelated radio work can proceed.
        let until = now + WAIT;
        Self(Arc::new(Mutex::new(State {
            startup_until: Some(until),
            next_send: Some(until),
            ..State::default()
        })))
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.0.lock().expect("address owner poisoned")
    }

    pub(crate) fn close(&self) {
        *self.lock() = State {
            closed: true,
            ..State::default()
        };
    }

    pub(crate) fn lease(&self, ieee: u64, short: u16) -> Option<AddressLease> {
        self.lock().lease(ieee, short).cloned()
    }

    pub(crate) fn is_current(&self, lease: &AddressLease) -> bool {
        self.lock().lease(lease.ieee, lease.short) == Some(lease)
    }

    pub(crate) fn check_adapter(&self, ieee: u64, node: u16) -> bool {
        self.lock().check_adapter(ieee, node)
    }

    pub(crate) fn conflict(&self, node: u16) {
        self.lock().conflict(node, Instant::now());
    }

    pub(crate) fn dispute(&self, ieee: u64, node: u16) {
        self.lock().dispute(ieee, node, Instant::now());
    }

    pub(super) fn membership(
        &self,
        ieee: u64,
        node: u16,
        admitted: bool,
        known: bool,
        now: Instant,
    ) -> bool {
        self.lock().membership(ieee, node, admitted, known, now)
    }

    pub(super) fn take_revocations(&self) -> impl Iterator<Item = Revoked> + use<> {
        std::mem::take(&mut self.lock().revocations).into_iter()
    }

    pub(super) fn departed(&self, ieee: u64) -> bool {
        self.lock().departed(ieee)
    }
    pub(super) fn node(&self, ieee: u64, now: Instant) -> Option<u16> {
        self.lock().node(ieee, now)
    }
    pub(super) fn ieee(&self, node: u16, now: Instant) -> Option<u64> {
        self.lock().ieee(node, now)
    }
    /// Identify an unsolicited observation after an adapter miss. Reopen an
    /// expired lease for the immediate receive path without renewing the route.
    pub(super) fn observation_sender(&self, node: u16, now: Instant) -> Option<u64> {
        let mut state = self.lock();
        let ieee = *state.by_node.get(&node)?;
        let assignment = state.by_ieee.get(&ieee)?;
        assignment.routing_until?;
        if assignment.usable_until <= now {
            state.assign(ieee, node, now);
        }
        Some(ieee)
    }

    pub(super) fn disputed(&self, node: u16) -> bool {
        self.lock().disputed(node)
    }
    pub(super) fn adapter_agrees(&self, ieee: u64, node: u16) -> bool {
        self.lock().adapter_agrees(ieee, node)
    }
    pub(super) fn contradicts(&self, ieee: u64, node: u16) -> bool {
        self.lock().contradicts(ieee, node)
    }
    pub(super) fn learn(&self, ieee: u64, node: u16, now: Instant) -> bool {
        self.lock().learn(ieee, node, now)
    }
    pub(super) fn learn_neighbor(&self, ieee: u64, node: u16, now: Instant) -> bool {
        let mut state = self.lock();
        !state.disputed(node) && state.learn_route(ieee, node, now)
    }
    pub(super) fn enqueue(&self, query: Query, now: Instant) -> bool {
        self.lock().enqueue(query, now)
    }
    /// Held observations share the reverse query's deadline and cancellation. A
    /// leave, conflict, rejected query or session shutdown drops its payloads.
    pub(super) fn hold_observation(&self, node: u16, callback: Callback, now: Instant, observed_at_ms: u64) {
        let mut state = self.lock();
        if state.disputed(node) || !state.enqueue(Query::Ieee(node), now) {
            return;
        }
        let probe = state.probes.iter_mut().find(|p| p.key == Query::Ieee(node))
            .expect("admitted reverse lookup");
        // Exclude last-hop LQI and RSSI when comparing copies of the same APS frame.
        if probe.observations.len() < CAPACITY
            && !probe.observations.iter().any(|(_, _, old)| same_aps_frame(old, &callback)) {
            probe.observations.push((now, observed_at_ms, callback));
        }
    }

    pub(super) fn take_observations(&self, node: u16, now: Instant) -> Vec<(Instant, u64, Callback)> {
        self.lock().probes.iter_mut()
            .find(|p| p.key == Query::Ieee(node) && p.until > now)
            .map(|p| std::mem::take(&mut p.observations)).unwrap_or_default()
    }

    pub(super) fn remember_recovered(&self, ieee: u64, node: u16, callback: &Callback, now: Instant) {
        let mut state = self.lock();
        let Some(lease) = state.lease(ieee, node).cloned() else { return };
        if state.recovered.len() == CAPACITY * (CAPACITY / 2) {
            state.recovered.pop_front();
        }
        state.recovered.push_back((lease, now + WAIT, callback.clone()));
    }

    pub(super) fn already_recovered(&self, ieee: u64, node: u16, callback: &Callback, now: Instant) -> bool {
        let state = self.lock();
        let Some(lease) = state.lease(ieee, node) else { return false };
        state.recovered.iter().any(|(original, until, saved)|
            original == lease && *until > now && same_aps_frame(saved, callback))
    }

    pub(super) fn discovery_until(&self, ieee: u64) -> Option<Instant> {
        self.lock().probes.iter().find(|p| p.key == Query::Network(ieee)).map(|p| p.until)
    }
    pub(super) fn hold(&self, work: Waiting, now: Instant) {
        self.lock().hold(work, now)
    }
    pub(super) fn ready(&self, now: Instant) -> Option<Waiting> {
        self.lock().ready(now)
    }
    pub(super) fn deadline(&self) -> Option<Instant> {
        self.lock().deadline()
    }
    pub(super) fn response(
        &self,
        m: &Incoming<'_>,
        source: Option<u64>,
        known: impl Fn(u64) -> bool,
        now: Instant,
    ) -> Option<(u64, u16)> {
        let answer = AddressReply::parse(m)?;
        let member = known(answer.ieee);
        self.lock().response(answer, m.sender, source, member, now)
    }

    pub(super) async fn resolve_node(&self, link: &mut Ezsp<'_>, ieee: u64) -> Result<Option<u16>> {
        if self.departed(ieee) {
            return Ok(None);
        }
        Ok(match zdo::lookup_node(link, ieee).await? {
            Some(node) => Some(node),
            None => self.node(ieee, Instant::now()),
        })
    }

    pub(super) async fn tick(&self, link: &mut Ezsp<'_>, now: Instant) -> Result<()> {
        let Some((key, seq)) = self.lock().next(now) else {
            return Ok(());
        };
        let status = match key {
            Query::Network(ieee) => {
                // Broadcast to receivers on when idle; single-device response, start index 0.
                let mut payload = vec![seq];
                payload.extend(ieee.to_le_bytes());
                payload.extend([0, 0]);
                zdo::submit_broadcast(link, 0xfffd, 0x0000, &payload).await?
            }
            Query::Ieee(node) => zdo::probe_ieee(link, node, seq).await?,
        };
        if status != 0 {
            let mut state = self.lock();
            if !state
                .probes
                .iter()
                .any(|p| p.key == key && p.sequence == Some(seq))
            {
                return Ok(());
            }
            state.probes.retain(|p| p.key != key);
            if let Query::Network(ieee) = key {
                state.commands.retain(|w| w.peer.0 != ieee);
            }
            eprintln!("Adapter declined address discovery for {key:?}");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        interview::Aps,
        mqtt::{Command, Operation},
    };

    const LIGHT: u64 = 0x0123_4567_89ab_cdef;
    const SHORT: u16 = 0x1204;

    fn started_book() -> Addresses {
        Addresses {
            authority: Authority(Arc::new(Mutex::new(State::default()))),
        }
    }

    fn job(ieee: u64, name: &str) -> Waiting {
        Waiting {
            peer: (ieee, 1),
            command: Command {
                device: name.into(),
                operation: Operation::Get,
                properties: Default::default(),
            }
            .into(),
            refresh: None,
            until: None,
            original: None,
        }
    }

    fn answer(seq: u8) -> Vec<u8> {
        let mut data = vec![seq, 0];
        data.extend(LIGHT.to_le_bytes());
        data.extend(SHORT.to_le_bytes());
        data
    }

    fn incoming(body: &[u8], cluster: u16, sender: u16) -> Incoming<'_> {
        Incoming {
            kind: 0,
            aps: Aps {
                profile: 0,
                cluster,
                src: 0,
                dst: 0,
            },
            aps_encrypted: false,
            sender,
            last_hop_lqi: 0,
            body,
        }
    }

    #[test]
    fn only_a_correlated_success_for_the_admitted_peer_resolves_a_command() {
        let book = started_book();
        let now = Instant::now();
        book.hold(job(LIGHT, "a"), now);
        book.hold(job(LIGHT, "b"), now);
        let (key, seq) = book.lock().next(now).unwrap();
        assert_eq!(key, Query::Network(LIGHT));
        assert!(book.lock().next(now + SPACING).is_none());
        let bytes = answer(seq);
        let valid = incoming(&bytes, 0x8000, SHORT);
        for (cluster, sender, tsn, status) in [
            (0x8001, SHORT, seq, 0),
            (0x8000, 0xffff, seq, 0),
            (0x8000, SHORT, seq + 1, 0),
            (0x8000, SHORT, seq, 1),
        ] {
            let mut data = answer(tsn);
            data[1] = status;
            assert_eq!(
                book.response(&incoming(&data, cluster, sender), None, |_| true, now),
                None
            );
        }
        assert_eq!(book.response(&valid, None, |_| false, now), None);
        assert_eq!(book.response(&valid, Some(LIGHT + 1), |_| true, now), None);
        assert_eq!(book.response(&valid, None, |_| true, now + WAIT), None);
        assert!(book.ready(now).is_none());
        assert_eq!(book.response(&valid, None, |_| true, now), Some((LIGHT, SHORT)));
        assert!(book.learn(LIGHT, SHORT, now));
        assert_eq!(book.ready(now).unwrap().command.device, "a");
        assert_eq!(book.ready(now).unwrap().command.device, "b");
        assert!(book.ready(now).is_none());
        assert_eq!(book.response(&valid, None, |_| true, now), None);
    }

    #[test]
    fn expiry_cooldown_and_restart_do_not_replay_commands() {
        let book = started_book();
        let now = Instant::now();
        book.hold(job(LIGHT, "first"), now);
        book.lock().next(now).unwrap();
        book.hold(job(LIGHT, "second"), now + SPACING);
        book.lock().expire(now + WAIT);
        assert!(book.lock().commands.is_empty());
        assert!(!book.enqueue(Query::Network(LIGHT), now + WAIT));
        assert!(book.enqueue(Query::Network(LIGHT), now + WAIT * 2));
        book.learn(LIGHT, SHORT, now + WAIT * 2);
        assert!(book.ready(now + WAIT * 2).is_none());
        assert_eq!(started_book().ieee(SHORT, now), None);
    }

    #[test]
    fn usage_never_renews_identity_and_an_adapter_hit_cannot_clear_a_conflict() {
        let now = Instant::now();
        let book = started_book();
        assert!(book.learn(LIGHT, SHORT, now));
        let before = now + MAX_AGE - SPACING;
        assert_eq!(book.node(LIGHT, before), Some(SHORT));
        assert!(book.adapter_agrees(LIGHT, SHORT));
        assert_eq!(book.ieee(SHORT, now + MAX_AGE), None);
        book.conflict(SHORT);
        assert!(!book.adapter_agrees(LIGHT, SHORT));
        assert!(book.learn(LIGHT + 1, SHORT, now + MAX_AGE));
        assert!(!book.adapter_agrees(LIGHT, SHORT));
        assert_eq!(book.node(LIGHT, now + MAX_AGE), None);
    }

    #[test]
    fn a_quiet_sensor_remains_identifiable_without_refreshing_its_outbound_route() {
        let book = started_book();
        let now = Instant::now();
        assert!(book.check_adapter(LIGHT, SHORT));
        assert_eq!(book.observation_sender(SHORT, now), None);
        assert!(book.learn_neighbor(LIGHT, SHORT, now));
        let previous = book.lease(LIGHT, SHORT).unwrap();
        let later = now + Duration::from_secs(3600);
        assert_eq!(book.observation_sender(SHORT, later), Some(LIGHT));
        assert!(!book.is_current(&previous));
        assert!(book.lease(LIGHT, SHORT).is_some());
        assert_eq!(book.node(LIGHT, later), None);
        assert_eq!(book.ieee(SHORT, later), None);
        assert!(book.lock().probes.is_empty());

        let awake = book.lease(LIGHT, SHORT).unwrap();
        assert!(book.membership(LIGHT, SHORT, true, true, later));
        assert!(!book.is_current(&awake));
        assert_eq!(book.observation_sender(SHORT, later), Some(LIGHT));
        assert!(book.membership(LIGHT, SHORT, false, true, later));
        assert_eq!(book.observation_sender(SHORT, later), None);
        assert!(book.membership(LIGHT + 1, SHORT, true, true, later));
        assert_eq!(book.observation_sender(SHORT, later), Some(LIGHT + 1));
        book.conflict(SHORT);
        assert_eq!(book.observation_sender(SHORT, later), None);
        assert!(book.learn(LIGHT + 1, SHORT, later));
        let authority = book.authority();
        drop(book);
        assert_eq!(authority.observation_sender(SHORT, later), None);
        assert_eq!(started_book().observation_sender(SHORT, later), None);
    }

    #[test]
    fn a_positive_adapter_lookup_that_disagrees_quarantines_both_addresses() {
        let now = Instant::now();
        let book = started_book();
        book.learn(LIGHT, SHORT, now);
        book.learn(LIGHT + 1, SHORT + 1, now);
        assert!(!book.check_adapter(LIGHT, SHORT + 1));
        assert_eq!(book.ieee(SHORT, now), None);
        assert_eq!(book.ieee(SHORT + 1, now), None);
        assert!(!book.adapter_agrees(LIGHT, SHORT));
        assert!(!book.adapter_agrees(LIGHT + 1, SHORT + 1));
        assert!(book.learn(LIGHT, SHORT + 1, now));
        assert_eq!(book.ieee(SHORT + 1, now), Some(LIGHT));
    }

    #[test]
    fn leave_revokes_held_work_and_unrelated_admission_does_not() {
        let book = started_book();
        let now = Instant::now();
        book.hold(job(LIGHT, "one"), now);
        let (_, seq) = book.lock().next(now).unwrap();
        book.membership(LIGHT + 1, SHORT + 1, true, true, now + SPACING);
        let data = answer(seq);
        let msg = incoming(&data, 0x8000, SHORT);
        assert_eq!(
            book.response(&msg, None, |_| true, now + SPACING),
            Some((LIGHT, SHORT))
        );
        // An admission at the queried short invalidates the earlier answer.
        book.membership(LIGHT + 1, SHORT, true, true, now + SPACING);
        assert_eq!(book.response(&msg, None, |_| true, now + SPACING), None);
        book.membership(LIGHT, SHORT + 2, false, true, now + SPACING);
        assert!(!book.learn(LIGHT, SHORT, now + SPACING));
        assert!(book.lock().commands.is_empty());
        book.membership(LIGHT, SHORT + 2, true, true, now + WAIT);
        assert_eq!(book.node(LIGHT, now + WAIT), Some(SHORT + 2));
        assert!(book.ready(now + WAIT).is_none());
    }

    #[test]
    fn adapter_only_assignment_fences_work_without_authorizing_cache_fallback() {
        let book = started_book();
        let authority = book.authority();
        let now = Instant::now();
        assert!(authority.check_adapter(LIGHT, SHORT));
        let original = book.lease(LIGHT, SHORT).unwrap();
        assert!(book.node(LIGHT, now).is_none());
        assert!(authority.check_adapter(LIGHT, SHORT));
        assert_eq!(book.lease(LIGHT, SHORT).as_ref(), Some(&original));
        assert!(book.learn(LIGHT, SHORT, now));
        assert_eq!(book.lease(LIGHT, SHORT).as_ref(), Some(&original));
        assert_eq!(book.node(LIGHT, now), Some(SHORT));
        book.membership(LIGHT, SHORT, true, true, now);
        assert!(!authority.is_current(&original));
        let replacement = book.lease(LIGHT, SHORT).unwrap();
        assert_ne!(replacement, original);
        assert!(authority.is_current(&replacement));
        drop(book);
        assert!(!authority.is_current(&replacement));
        assert!(!authority.check_adapter(LIGHT, SHORT));
        assert!(!authority.learn(LIGHT, SHORT, now));
        let new_session = started_book();
        assert!(new_session.learn(LIGHT, SHORT, now));
        assert!(!new_session.is_current(&replacement));
    }

    #[test]
    fn conflicting_adapter_observation_retires_both_incumbents_and_aba_never_revives_them() {
        let book = started_book();
        let now = Instant::now();
        book.check_adapter(LIGHT, SHORT);
        book.check_adapter(LIGHT + 1, SHORT + 1);
        let first = book.lease(LIGHT, SHORT).unwrap();
        let second = book.lease(LIGHT + 1, SHORT + 1).unwrap();
        assert!(!book.authority().check_adapter(LIGHT, SHORT + 1));
        assert!(!book.is_current(&first));
        assert!(!book.is_current(&second));
        assert_eq!(
            book.take_revocations().collect::<BTreeSet<_>>(),
            BTreeSet::from([
                Revoked {
                    ieee: Some(LIGHT),
                    short: Some(SHORT)
                },
                Revoked {
                    ieee: Some(LIGHT + 1),
                    short: Some(SHORT + 1)
                },
            ])
        );
        assert!(book.learn(LIGHT + 1, SHORT, now));
        book.membership(LIGHT, SHORT, true, true, now);
        assert!(!book.is_current(&first));
        assert_eq!(book.ieee(SHORT, now), Some(LIGHT));
    }

    #[test]
    fn a_short_only_conflict_rejects_late_answers_for_that_short_only() {
        let book = started_book();
        let now = Instant::now();
        book.hold(job(LIGHT, "first"), now);
        book.hold(job(LIGHT + 1, "unrelated"), now);
        let (_, first) = book.lock().next(now).unwrap();
        let (_, second) = book.lock().next(now + SPACING).unwrap();
        // An unknown short cannot tell us which pending forward query might return it.
        book.conflict(SHORT);
        let data = answer(first);
        assert_eq!(
            book.response(
                &incoming(&data, 0x8000, SHORT),
                None,
                |_| true,
                now + SPACING
            ),
            None
        );
        let mut data = answer(second);
        data[2..10].copy_from_slice(&(LIGHT + 1).to_le_bytes());
        data[10..12].copy_from_slice(&(SHORT + 1).to_le_bytes());
        let reply = incoming(&data, 0x8000, SHORT + 1);
        assert_eq!(
            book.response(&reply, None, |_| true, now + SPACING),
            Some((LIGHT + 1, SHORT + 1))
        );
        assert!(book.learn(LIGHT + 1, SHORT + 1, now + SPACING));
        assert_eq!(
            book.ready(now + SPACING).unwrap().command.device,
            "unrelated"
        );
        assert!(book.ready(now + SPACING).is_none());
        // The first peer may still answer with a new, uncontested address.
        let mut data = answer(first);
        data[10..12].copy_from_slice(&(SHORT + 2).to_le_bytes());
        assert_eq!(
            book.response(
                &incoming(&data, 0x8000, SHORT + 2),
                None,
                |_| true,
                now + SPACING
            ),
            Some((LIGHT, SHORT + 2))
        );
    }

    #[test]
    fn one_unresolved_peer_cannot_consume_the_entire_command_queue() {
        let book = started_book();
        let now = Instant::now();
        for _ in 0..CAPACITY {
            book.hold(job(LIGHT, "busy"), now);
        }
        book.hold(job(LIGHT + 1, "independent"), now);
        book.learn(LIGHT + 1, SHORT + 1, now);
        assert_eq!(book.ready(now).unwrap().command.device, "independent");
    }

    #[test]
    fn a_departure_at_an_old_short_preserves_both_current_assignments() {
        let book = started_book();
        let now = Instant::now();
        book.membership(LIGHT, SHORT, true, true, now);
        book.membership(LIGHT, SHORT + 1, true, true, now);
        book.membership(LIGHT + 1, SHORT, true, true, now);
        let owner = book.lease(LIGHT + 1, SHORT).unwrap();
        let moved = book.lease(LIGHT, SHORT + 1).unwrap();
        book.take_revocations().for_each(drop);
        assert!(!book.membership(LIGHT, SHORT, false, true, now));
        assert!(book.is_current(&owner));
        assert!(book.is_current(&moved));
        assert!(book.take_revocations().next().is_none());
    }

    #[test]
    fn a_new_observation_after_expiry_cannot_revive_an_expired_handle() {
        let book = started_book();
        let now = Instant::now();
        book.learn(LIGHT, SHORT, now);
        let original = book.lease(LIGHT, SHORT).unwrap();
        book.learn(LIGHT, SHORT, now + MAX_AGE);
        assert!(!book.is_current(&original));
        assert!(book.lease(LIGHT, SHORT).is_some());
    }

    #[test]
    fn restored_departure_stays_in_effect_until_a_confirmed_admission() {
        let mut book = started_book();
        let now = Instant::now();
        book.restore_departed([LIGHT]);
        assert!(!book.check_adapter(LIGHT, SHORT));
        assert!(!book.learn(LIGHT, SHORT, now));
        assert!(book.lease(LIGHT, SHORT).is_none());
        assert!(book.membership(LIGHT, SHORT, true, true, now));
        assert!(!book.departed(LIGHT));
        assert!(book.lease(LIGHT, SHORT).is_some());
    }

    #[test]
    fn neighbor_replies_resolve_the_named_peer_and_preserve_the_responders_assignment() {
        let now = Instant::now();
        for key in [Query::Network(LIGHT), Query::Ieee(SHORT)] {
            let book = started_book();
            assert!(book.learn(LIGHT + 1, SHORT + 1, now));
            assert!(book.enqueue(key, now));
            let (_, seq) = book.lock().next(now).unwrap();
            let data = answer(seq);
            let cluster = match key { Query::Network(_) => 0x8000, Query::Ieee(_) => 0x8001 };
            let proxy = incoming(&data, cluster, SHORT + 1);
            for bad_source in [LIGHT, LIGHT + 2, u64::MAX] {
                assert_eq!(book.response(&proxy, Some(bad_source), |_| true, now), None);
            }
            assert_eq!(book.response(&proxy, None, |_| false, now), None);
            assert_eq!(book.response(&proxy, Some(LIGHT + 1), |_| true, now + WAIT), None);
            let (ieee, short) = book.response(&proxy, Some(LIGHT + 1), |_| true, now).unwrap();
            assert_eq!((ieee, short), (LIGHT, SHORT));
            assert!(book.learn_neighbor(ieee, short, now));
            assert_eq!(book.node(LIGHT, now), Some(SHORT));
            assert_eq!(book.node(LIGHT + 1, now), Some(SHORT + 1));
            assert_eq!(book.response(&proxy, None, |_| true, now), None);
        }
    }

    #[test]
    fn conflicting_neighbor_answers_retire_the_provisional_assignment() {
        let book = started_book();
        let now = Instant::now();
        book.hold(job(LIGHT, "lamp"), now);
        let (_, seq) = book.lock().next(now).unwrap();
        let data = answer(seq);
        let proxy = incoming(&data, 0x8000, SHORT + 1);
        let (ieee, short) = book.response(&proxy, None, |_| true, now).unwrap();
        assert!(book.learn_neighbor(ieee, short, now));
        let lease = book.lease(ieee, short).unwrap();
        let mut other = data.clone();
        other[10..].copy_from_slice(&(SHORT + 2).to_le_bytes());
        assert_eq!(book.response(&incoming(&other, 0x8000, SHORT + 3), None, |_| true, now), None);
        assert!(!book.is_current(&lease));
        assert!(book.disputed(SHORT));
        assert!(book.ready(now).is_none());
        assert!(!book.learn_neighbor(ieee, short, now));
    }

    #[test]
    fn a_conflicting_reverse_answer_for_an_unregistered_device_retires_the_route() {
        let now = Instant::now();
        let book = started_book();
        assert!(book.enqueue(Query::Ieee(SHORT), now));
        let (_, seq) = book.lock().next(now).unwrap();
        let mut body = answer(seq);
        let response = incoming(&body, 0x8001, SHORT + 1);
        assert_eq!(book.response(&response, None, |_| true, now), Some((LIGHT, SHORT)));
        assert!(book.learn_neighbor(LIGHT, SHORT, now));
        body[2..10].copy_from_slice(&(LIGHT + 1).to_le_bytes());
        assert_eq!(book.response(&incoming(&body, 0x8001, SHORT + 2), None, |_| false, now), None);
        assert!(book.disputed(SHORT));
        assert!(book.ieee(SHORT, now).is_none());
    }

    #[test]
    fn direct_confirmation_supersedes_further_neighbor_answers_to_the_same_query() {
        let now = Instant::now();
        let book = started_book();
        assert!(book.enqueue(Query::Network(LIGHT), now));
        let (_, seq) = book.lock().next(now).unwrap();
        let mut body = answer(seq);
        assert_eq!(book.response(&incoming(&body, 0x8000, SHORT + 1), None, |_| true, now), Some((LIGHT, SHORT)));
        assert!(book.learn_neighbor(LIGHT, SHORT, now));
        assert_eq!(book.response(&incoming(&body, 0x8000, SHORT), Some(LIGHT), |_| true, now), Some((LIGHT, SHORT)));
        assert!(book.learn(LIGHT, SHORT, now));
        body[10..].copy_from_slice(&(SHORT + 2).to_le_bytes());
        assert_eq!(book.response(&incoming(&body, 0x8000, SHORT + 1), None, |_| true, now), None);
        assert_eq!(book.node(LIGHT, now), Some(SHORT));
    }

    #[test]
    fn restart_quarantines_wire_numbers_but_keeps_the_original_deadline_and_current_evidence() {
        let now = Instant::now();
        let book = Addresses {
            authority: Authority::starting(now),
        };
        let bytes = answer(1);
        let incoming = incoming(&bytes, 0x8000, SHORT);
        let mut expiring = job(LIGHT, "expiring");
        expiring.until = Some(now + SPACING);
        book.hold(expiring, now);
        book.hold(job(LIGHT, "fresh"), now);
        assert_eq!(book.deadline(), Some(now + SPACING));
        assert!(book.lock().next(now + WAIT - SPACING).is_none());
        assert_eq!(
            book.response(&incoming, None, |_| true, now + WAIT - SPACING),
            None
        );
        // Independent direct evidence remains usable immediately after attach.
        assert!(book.learn(LIGHT + 1, SHORT + 1, now));
        assert_eq!(book.node(LIGHT + 1, now), Some(SHORT + 1));
        let (query, seq) = book.lock().next(now + WAIT).unwrap();
        assert_eq!((query, seq), (Query::Network(LIGHT), 1));
        assert_eq!(
            book.response(&incoming, None, |_| true, now + WAIT),
            Some((LIGHT, SHORT))
        );
        book.learn(LIGHT, SHORT, now + WAIT);
        assert_eq!(book.ready(now + WAIT).unwrap().command.device, "fresh");
        assert!(book.ready(now + WAIT).is_none());
    }

    #[test]
    fn discovery_numbers_increment_across_idle_and_wrap_without_skipping_with_the_clock() {
        let book = started_book();
        let start = Instant::now();
        let mut expected = 1u8;
        for n in 1..=260 {
            let now = start + WAIT * n;
            assert!(book.enqueue(Query::Ieee(n as u16), now));
            let (_, seq) = book.lock().next(now).unwrap();
            assert_eq!(seq, expected);
            expected = expected.wrapping_add(1);
        }
    }

    #[test]
    fn conflict_and_submission_in_one_clock_tick_respect_their_observed_order() {
        let now = Instant::now();
        for conflict_first in [true, false] {
            let book = started_book();
            // An ordinary adapter miss has no authority to clear this short.
            book.learn(LIGHT + 1, SHORT, now);
            book.enqueue(Query::Network(LIGHT), now);
            if conflict_first {
                book.lock().conflict(SHORT, now);
            }
            let (_, seq) = book.lock().next(now).unwrap();
            if !conflict_first {
                book.lock().conflict(SHORT, now);
            }
            assert_eq!(book.node(LIGHT + 1, now), None);
            assert!(!book.adapter_agrees(LIGHT + 1, SHORT));
            let bytes = answer(seq);
            assert_eq!(
                book.response(&incoming(&bytes, 0x8000, SHORT), None, |_| true, now),
                conflict_first.then_some((LIGHT, SHORT))
            );
        }
    }

    #[test]
    fn held_input_expires_with_its_query_and_cannot_cross_a_reassignment() {
        let now = Instant::now();
        let sample = Callback { id: 0x45, payload: vec![0; 20] };
        for change in ["conflict", "leave", "admission", "expiry", "shutdown"] {
            let book = started_book();
            book.hold_observation(SHORT, sample.clone(), now, 1000);
            match change {
                "conflict" => book.conflict(SHORT),
                "leave" => assert!(book.membership(LIGHT, SHORT, false, true, now)),
                "admission" => assert!(book.membership(LIGHT + 1, SHORT, true, true, now)),
                "expiry" => book.lock().expire(now + WAIT),
                _ => book.close(),
            }
            assert!(book.take_observations(SHORT, now + WAIT / 2).is_empty(), "{change}");
        }
    }

    #[test]
    fn repeated_reverse_misses_share_one_probe_and_leave_room_for_controls() {
        let now = Instant::now();
        let book = started_book();
        for n in 1..=CAPACITY as u16 / 2 {
            assert!(book.enqueue(Query::Ieee(n), now));
            assert!(book.enqueue(Query::Ieee(n), now));
        }
        assert!(!book.enqueue(Query::Ieee(0x4000), now));
        assert!(book.enqueue(Query::Network(LIGHT), now));
        assert!(book.lock().next(now).is_some());
        assert!(book.lock().next(now).is_none());
        assert!(book.lock().next(now + SPACING).is_some());
    }
}
