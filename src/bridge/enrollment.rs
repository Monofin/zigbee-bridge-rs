//! Managed IAS CIE commissioning. The radio loop owns every transaction and
//! deadline. Only the durable assignment outlives that loop (ZCL R8 §8.2.2.1.3).
mod ace;
mod table;
use crate::ash::MemberUpdate;
use super::{Bindings, Identity, zdo};
use crate::{ash::{Destination, Ezsp}, interview::Incoming};
use anyhow::Result;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    time::Duration,
};
use table::{Row, Table, UNASSIGNED};
use tokio::time::Instant;
use zigbee_devices::ias::{
    CLUSTER,
    enrollment::{self, Assignment, ownership},
};
use zigbee_zcl::{
    Direction, Frame,
    attributes::WriteStatus,
    cluster::{Command, IasZone},
    foundation::Payload,
};

pub(super) trait Radio {
    async fn node(&mut self, ieee: u64) -> Result<Option<u16>>;
    async fn ieee(&mut self, node: u16) -> Result<Option<u64>>;
    fn destination(&self, ieee: u64, node: u16) -> Option<Destination>;
    fn current(&self, destination: &Destination) -> bool;
    async fn send(&mut self, destination: &Destination, profile: u16, ep: u8, data: &[u8]) -> Result<bool>;
}
impl Radio for Ezsp<'_> {
    async fn node(&mut self, ieee: u64) -> Result<Option<u16>> {
        zdo::lookup_node(self, ieee).await
    }
    async fn ieee(&mut self, node: u16) -> Result<Option<u64>> {
        zdo::lookup_ieee(self, node).await
    }
    fn destination(&self, ieee: u64, node: u16) -> Option<Destination> {
        Ezsp::destination(self, ieee, node).ok()
    }
    fn current(&self, dest: &Destination) -> bool {
        Ezsp::current(self, dest)
    }
    async fn send(&mut self, dest: &Destination, profile: u16, ep: u8, data: &[u8]) -> Result<bool> {
        match zdo::submit_unicast(self, dest, profile, CLUSTER, (1, ep), 0xff, data).await {
            Ok(status) => Ok(status == 0),
            Err(e) if crate::ash::locally_refused(&e) => Ok(false),
            Err(e) => Err(e),
        }
    }
}

const WAIT: Duration = Duration::from_secs(9); // a commissioning sleepy zone may poll every seven seconds
#[derive(Clone, Copy, PartialEq, Eq)]
enum Step {
    Inspect,
    Write,
    Verify,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Blocked {
    Address,
    Adapter,
}
impl Blocked {
    fn state(self) -> &'static str {
        match self {
            Self::Address => "waiting_for_address",
            Self::Adapter => "waiting_for_adapter",
        }
    }
}

struct Job {
    step: Step,
    outstanding: Option<(Destination, u8)>,
    due: Instant,
    tries: u8,
    request: Option<(Destination, u8)>,
    blocked: Option<Blocked>,
    preflight_until: Option<Instant>,
}
impl Job {
    fn new() -> Self {
        Self {
            step: Step::Inspect,
            outstanding: None,
            due: Instant::now(),
            tries: 0,
            request: None,
            blocked: None,
            preflight_until: None,
        }
    }
    fn matches(&self, short: u16, seq: u8) -> bool {
        self.outstanding.as_ref().is_some_and(|(to, tsn)| to.short() == short && *tsn == seq)
    }
    fn block(&mut self, cause: Blocked, now: Instant) {
        // Bound preflight independently of attempts actually accepted for transmission.
        let until = *self.preflight_until.get_or_insert(now + WAIT * 3);
        self.blocked = Some(cause);
        self.due = until.min(now + WAIT);
        self.outstanding = None;
    }

}

pub(super) struct Enrollment {
    table: Table,
    status_samples: BTreeMap<Identity, (Instant, u16, Destination)>,
    jobs: BTreeMap<Identity, Job>,
    cie: u64,
    sequence: u8,
    survey: BTreeMap<Identity, u16>,
    unread_assignments: BTreeSet<Identity>,
    stopped: BTreeMap<Identity, &'static str>,
    waiting_for_ids: BTreeSet<Identity>,
}
impl Enrollment {
    pub(super) fn has_outstanding(&self, ieee: u64) -> bool {
        self.jobs.range((ieee,0)..=(ieee,u8::MAX)).any(|(_,job)|job.outstanding.is_some())
    }
    pub fn open(dir: &Path) -> Result<Self> {
        Ok(Self {
            table: Table::open(dir)?,
            status_samples: BTreeMap::new(),
            jobs: BTreeMap::new(),
            cie: 0,
            sequence: 0,
            survey: BTreeMap::new(),
            unread_assignments: BTreeSet::new(),
            stopped: BTreeMap::new(),
            waiting_for_ids: BTreeSet::new(),
        })
    }
    pub fn start(&mut self, cie: u64, devices: &Bindings) -> Result<()> {
        self.table.attach(cie)?;
        self.cie = cie;
        // An imported alarm may already own a number on this CIE. Learn it
        // before allocating new numbers. These first probes are read-only.
        for (&ieee, (_, endpoints)) in &devices.peers {
            if self.table.left.contains(&ieee) {
                continue;
            }
            for &ep in endpoints.keys() {
                let key = (ieee, ep);
                if !self.table.rows.contains_key(&key)
                    && let Some(profile) = server(devices, key)
                {
                    self.survey.insert(key, profile);
                    self.unread_assignments.insert(key);
                    self.jobs.insert(key, Job::new());
                }
            }
        }
        for (&key, row) in &self.table.rows {
            if row.id == UNASSIGNED
                && !self.table.left.contains(&key.0)
                && server(devices, key) == Some(row.profile)
            {
                self.unread_assignments.insert(key);
            }
            if !self.table.left.contains(&key.0)
                && !row.confirmed
                && server(devices, key) == Some(row.profile)
            {
                self.jobs.insert(key, Job::new());
            }
        }
        Ok(())
    }
    pub fn interviewed(&mut self, ieee: u64, devices: &Bindings) -> Result<()> {
        self.status_samples.retain(|k,_|k.0!=ieee);
        self.stopped.retain(|k, _| k.0 != ieee);
        self.waiting_for_ids.retain(|k| k.0 != ieee);
        // A missing descriptor on a sleepy re-interview cannot release an ID.
        self.jobs.retain(|k, _| k.0 != ieee);
        if let Some((_, eps)) = devices.peers.get(&ieee) {
            for &ep in eps.keys() {
                let key = (ieee, ep);
                if let Some(profile) = server(devices, key) {
                    let id = self.table.rows.get(&key).map_or(UNASSIGNED, |r| r.id);
                    self.survey.remove(&key);
                    if id == UNASSIGNED {
                        self.unread_assignments.insert(key);
                    }
                    self.table.save(
                        key,
                        Row {
                            profile,
                            id,
                            confirmed: false,
                        },
                    )?;
                    self.jobs.insert(key, Job::new());
                }
            }
        }
        self.table.readmit(ieee)?;
        Ok(())
    }
    pub fn membership(
        &mut self,
        ieee: u64,
        short: u16,
        change: MemberUpdate,
        devices: &Bindings,
    ) -> Result<()> {
        self.status_samples.retain(|k,_|k.0!=ieee);
        self.stopped.retain(|k, _| k.0 != ieee);
        self.invalidate_address(Some(ieee), short, |_| false);
        self.jobs.retain(|k, _| k.0 != ieee);
        self.waiting_for_ids.retain(|k| k.0 != ieee);
        if change == MemberUpdate::Left {
            if self.table.rows.keys().any(|k| k.0 == ieee)
                || devices.peers.get(&ieee).is_some_and(|(_, eps)| {
                    eps.keys().any(|&ep| server(devices, (ieee, ep)).is_some())
                })
            {
                self.table.leave(ieee)?;
            }
            self.survey.retain(|k, _| k.0 != ieee);
            self.unread_assignments.retain(|k| k.0 != ieee);
        } else {
            for (&key, _) in self.survey.range((ieee, 0)..=(ieee, u8::MAX)) {
                self.jobs.insert(key, Job::new());
            }
            for (&key, row) in self.table.rows.range((ieee, 0)..=(ieee, u8::MAX)) {
                if server(devices, key) == Some(row.profile) {
                    self.jobs.insert(key, Job::new());
                }
            }
        }
        Ok(())
    }
    /// A conflict retires in-flight addresses, never the persistent CIE assignment.
    pub(super) fn invalidate_address(&mut self, ieee: Option<u64>, short: u16,
        current: impl Fn(&Destination) -> bool) {
        let retired = |d: &Destination| (Some(d.ieee()) == ieee || d.short() == short) && !current(d);
        self.status_samples.retain(|_, sample| !retired(&sample.2));
        for job in self.jobs.values_mut() {
            if job.outstanding.as_ref().is_some_and(|(d, _)| retired(d))
                || job.request.as_ref().is_some_and(|(d, _)| retired(d))
            {
                job.request = None;
                job.step = Step::Inspect;
                job.block(Blocked::Address, Instant::now());
            }
        }
    }

    pub(super) fn peer_seen(&mut self, ieee: u64) {
        // Only the affected peer's verified traffic can rearm dormant work.
        // Other stopped reasons include a foreign CIE and a conflicting zone ID.
        self.stopped.retain(|&key, reason| {
            if key.0 == ieee && matches!(*reason, "waiting_for_address" | "waiting_for_adapter" | "waiting_for_device") {
                self.jobs.entry(key).or_insert_with(Job::new);
                return false;
            }
            true
        });
    }

    pub fn status(&self, key: Identity) -> serde_json::Value {
        let row = self.table.rows.get(&key);
        let state = if let Some(job) = self.jobs.get(&key) {
            job.blocked.map(Blocked::state).unwrap_or(match job.step {
                Step::Inspect => "reading_zone",
                Step::Write => "writing_cie",
                Step::Verify => "verifying",
            })
        } else if let Some(reason) = self.stopped.get(&key) {
            *reason
        } else if self.waiting_for_ids.contains(&key) {
            "waiting_for_existing_ids"
        } else if row.is_some_and(|r| r.confirmed) {
            "confirmed"
        } else {
            "unmanaged"
        };
        let id = row.and_then(|r| (r.id != UNASSIGNED).then_some(r.id));
        serde_json::json!({"state":state,"zone_id":id})
    }
    fn stop(&mut self, key: Identity, reason: &'static str) {
        self.jobs.remove(&key);
        self.stopped.insert(key, reason);
    }
    pub fn deadline(&self) -> Option<Instant> {
        if self.unread_assignments.is_empty() && !self.waiting_for_ids.is_empty() {
            return Some(Instant::now());
        }
        self.jobs.values().map(|j| j.due).min()
    }

    pub(super) async fn tick(&mut self, link: &mut impl Radio) -> Result<()> {
        if self.unread_assignments.is_empty() {
            for key in std::mem::take(&mut self.waiting_for_ids) {
                self.jobs.insert(key, Job::new());
            }
        }
        let now = Instant::now();
        let Some((&key, _)) = self
            .jobs
            .iter()
            .filter(|(_, j)| j.due <= now)
            .min_by_key(|(_, j)| j.due)
        else {
            return Ok(());
        };
        let job = self.jobs.get_mut(&key).unwrap();
        if let Some(cause) = job.blocked && job.preflight_until.is_some_and(|t| now >= t) {
            self.stop(key, cause.state());
            return Ok(());
        }
        if job.tries == 3 {
            eprintln!(
                "IAS endpoint {} did not confirm enrollment after three attempts",
                key.1
            );
            let state = job.blocked.map_or("waiting_for_device", Blocked::state);
            self.stop(key, state);
            return Ok(());
        }
        job.outstanding = None;
        let Some(dest) = link.node(key.0).await?.and_then(|node| link.destination(key.0, node)) else {
            job.block(Blocked::Address, now);
            return Ok(());
        };
        self.send_step(link, key, &dest, Step::Inspect).await
    }
    async fn send_step(
        &mut self,
        link: &mut impl Radio,
        key: Identity,
        dest: &Destination,
        step: Step,
    ) -> Result<()> {
        self.sequence = self.sequence.wrapping_add(1);
        let seq = self.sequence;
        let message = match step {
            Step::Inspect | Step::Verify => enrollment::inspect(key.1, seq)?,
            Step::Write => enrollment::write_cie(key.1, seq, self.cie)?,
        };
        let profile = self
            .survey
            .get(&key)
            .copied()
            .unwrap_or_else(|| self.table.rows[&key].profile);
        let job = self.jobs.get_mut(&key).unwrap();
        job.step = step;
        if link.send(dest, profile, key.1, &message.frame).await? {
            if step == Step::Inspect { job.tries += 1; }
            job.outstanding = Some((dest.clone(), seq));
            job.due = Instant::now() + WAIT;
            job.preflight_until = None;
            job.blocked = None;
        } else {
            job.block(if link.current(dest) { Blocked::Adapter } else { Blocked::Address }, Instant::now());
        }
        Ok(())
    }
    async fn answer(
        &mut self,
        link: &mut impl Radio,
        key: Identity,
        dest: &Destination,
        seq: u8,
        code: u8,
        id: u8,
    ) -> Result<bool> {
        let message = enrollment::response(key.1, seq, code, id)?;
        link.send(dest, self.table.rows[&key].profile, key.1, &message.frame)
            .await
    }
    pub(super) async fn incoming(
        &mut self,
        link: &mut impl Radio,
        devices: &Bindings,
        msg: &Incoming<'_>,
    ) -> Result<()> {
        if !matches!(msg.kind, 0 | 1)
            || msg.aps.dst != 1
            || msg.aps.cluster != CLUSTER
            || !matches!(msg.aps.profile, 0x0104 | 0xc05e)
        {
            return Ok(());
        }
        let Ok(frame) = Frame::decode(msg.body) else {
            return Ok(());
        };
        if frame.header.direction != Direction::ServerToClient
            || frame.header.manufacturer.is_some()
        {
            return Ok(());
        }
        let command = Command::decode(CLUSTER, frame);
        let enroll = matches!(
            command,
            Ok(Command::IasZone(IasZone::ZoneEnrollRequest { .. }))
        );
        let wake = !self.stopped.is_empty()
            && matches!(
                command,
                Ok(Command::IasZone(
                    IasZone::ZoneStatusChangeNotification { .. }
                ))
            );
        let seq = frame.header.sequence;
        if !wake
            && !enroll
            && !self
                .jobs
                .values()
                .any(|j| j.matches(msg.sender, seq))
        {
            return Ok(());
        }
        let Some(ieee) = link.ieee(msg.sender).await? else {
            return Ok(());
        };
        let Some(dest) = link.destination(ieee, msg.sender) else {return Ok(())};
        let key = (ieee, msg.aps.src);
        if self.table.left.contains(&key.0) {
            return Ok(());
        }
        if server(devices, key) != Some(msg.aps.profile) {
            return Ok(());
        }
        // The reverse lookup can itself receive a conflict callback.
        if !link.current(&dest) { return Ok(()); }
        if wake { self.peer_seen(ieee); }
        let matched = self.jobs.get(&key).is_some_and(|j| j.matches(msg.sender, seq)
            && j.outstanding.as_ref().is_some_and(|(to,_)| link.current(to)));
        if let Some(&profile) = self.survey.get(&key) {
            if profile != msg.aps.profile {
                return Ok(());
            }
            if enroll {
                self.stopped.remove(&key);
                self.jobs.entry(key).or_insert_with(Job::new);
                return Ok(());
            }
            if matched && let Some(assigned) = ownership(frame)
            {
                self.surveyed(key, profile, assigned)?;
            }
            return Ok(());
        }
        let Some(&row) = self.table.rows.get(&key) else {
            return Ok(());
        };
        if row.profile != msg.aps.profile {
            return Ok(());
        }
        if enroll {
            self.stopped.remove(&key);
            let job = self.jobs.entry(key).or_insert_with(Job::new);
            if job.step == Step::Verify && row.id != UNASSIGNED {
                self.answer(link, key, &dest, seq, 0, row.id).await?;
            } else {
                job.request = Some((dest,seq));
                if job.blocked == Some(Blocked::Address) { job.due = Instant::now(); }
            }
            return Ok(());
        }
        if !matched {return Ok(());}
        match self.jobs[&key].step {
            Step::Write => {
                match Payload::decode(frame) {
                    Ok(Payload::WriteAttributesResponse(WriteStatus::Success)) => {}
                    Ok(Payload::WriteAttributesResponse(WriteStatus::Failures(_))) => {
                        self.stop(key, "write_rejected");
                        eprintln!("IAS endpoint {} rejected coordinator address", key.1);
                        return Ok(());
                    }
                    _ => return Ok(()),
                }
                let job = self.jobs.get_mut(&key).unwrap();
                job.outstanding = None;
                let response_seq = job.request.as_ref()
                    .filter(|(to, _)| link.current(to))
                    .map_or(seq, |(_, s)| *s);
                if self
                    .answer(link, key, &dest, response_seq, 0, row.id)
                    .await?
                {
                    self.jobs.get_mut(&key).unwrap().request = None;
                    self.send_step(link, key, &dest, Step::Verify).await?;
                } else {
                    let cause = if link.current(&dest) { Blocked::Adapter } else { Blocked::Address };
                    self.jobs.get_mut(&key).unwrap().block(cause, Instant::now());
                }
            }
            Step::Inspect | Step::Verify => {
                let Some(assigned) = ownership(frame) else {
                    return Ok(());
                };
                self.observed_state(link, key, &dest, seq, assigned)
                    .await?;
            }
        }
        Ok(())
    }
    /// Adopt an existing CIE assignment without changing the device.
    fn surveyed(
        &mut self,
        key: Identity,
        profile: u16,
        Assignment {
            enrolled,
            cie: owner,
            id,
        }: Assignment,
    ) -> Result<()> {
        self.jobs.remove(&key);
        if enrolled && owner == self.cie {
            if id == UNASSIGNED || self.table.rows.values().any(|r| r.id == id) {
                self.stop(key, "conflicting_zone_id");
                eprintln!(
                    "Existing IAS zone ID conflict on endpoint {}, new allocations paused",
                    key.1
                );
                return Ok(());
            }
            self.table.save(
                key,
                Row {
                    profile,
                    id,
                    confirmed: true,
                },
            )?;
        } else if enrolled && matches!(owner, 0 | u64::MAX) {
            self.stop(key, "unknown_cie");
            return Ok(());
        }
        self.survey.remove(&key);
        self.unread_assignments.remove(&key);
        Ok(())
    }

    async fn observed_state(
        &mut self,
        link: &mut impl Radio,
        key: Identity,
        dest: &Destination,
        seq: u8,
        Assignment {
            enrolled,
            cie: owner,
            id: reported,
        }: Assignment,
    ) -> Result<()> {
        let row = self.table.rows[&key];
        let job = &self.jobs[&key];
        let step = job.step;
        let request_seq = job.request.as_ref().filter(|(to, _)| link.current(to)).map(|(_, seq)| *seq);
        if owner != 0 && owner != u64::MAX && owner != self.cie || enrolled && owner != self.cie {
            self.table.save(
                key,
                Row {
                    confirmed: false,
                    ..row
                },
            )?;
            if owner != self.cie && !matches!(owner, 0 | u64::MAX) {
                self.unread_assignments.remove(&key);
            }
            self.stop(key, "different_cie");
            eprintln!("IAS endpoint {} already names another control panel", key.1);
            return Ok(());
        }
        if enrolled {
            if reported == UNASSIGNED || self.table.reserve(key, Some(reported))? != Some(reported)
            {
                self.table.save(
                    key,
                    Row {
                        confirmed: false,
                        ..row
                    },
                )?;
                self.unread_assignments.insert(key);
                self.stop(key, "conflicting_zone_id");
                eprintln!("IAS endpoint {} has a conflicting zone ID", key.1);
                return Ok(());
            }
            self.unread_assignments.remove(&key);
            self.table.save(
                key,
                Row {
                    id: reported,
                    confirmed: true,
                    ..row
                },
            )?;
            if let Some(s) = request_seq {
                self.answer(link, key, dest, s, 0, reported).await?;
            }
            self.jobs.remove(&key);
        } else if step == Step::Inspect {
            self.unread_assignments.remove(&key);
            self.table.save(
                key,
                Row {
                    confirmed: false,
                    ..row
                },
            )?;
            if row.id == UNASSIGNED && !self.unread_assignments.is_empty() {
                self.jobs.remove(&key);
                self.waiting_for_ids.insert(key);
                eprintln!("Waiting for existing IAS zone IDs before allocating a new one");
            } else if self.table.reserve(key, None)?.is_none() {
                self.answer(
                    link,
                    key,
                    dest,
                    request_seq.unwrap_or(seq),
                    3,
                    UNASSIGNED,
                )
                .await?;
                self.stop(key, "zone_table_full");
            } else {
                self.send_step(link, key, dest, Step::Write).await?;
            }
        }
        Ok(())
    }
}
pub(super) fn server(devices: &Bindings, (ieee, ep): Identity) -> Option<u16> {
    let d = devices.peers.get(&ieee)?.1.get(&ep)?;
    (super::standard::application(ep, d) && d.servers.contains(&CLUSTER)).then_some(d.profile)
}
#[cfg(test)]
mod tests;
