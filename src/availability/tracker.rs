//! Each radio session owns its timers and at most one active probe. Confirmed
//! inbound traffic supersedes a probe, even if its reply arrives much later.
use super::{Catalogue, Settings};
use crate::ash::Destination;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    hash::{BuildHasher, RandomState},
    io::ErrorKind,
    path::{Path, PathBuf},
    thread::JoinHandle,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::time::Instant;
const PROBE_LIMIT: Duration = Duration::from_secs(8);
const PROBE_SPACING: Duration = Duration::from_secs(2);
const CHECKPOINT: Duration = Duration::from_secs(60);

#[derive(Serialize, Deserialize, Clone, Copy)]
struct Sighting {
    last_ms: Option<u64>,
    active: bool,
    departed: bool,
}
struct Peer {
    name: String,
    settings: Settings,
    online: bool,
    retry: Option<Destination>,
    multiplier: f64,
    suspended: bool,
    departed: bool,
    due: Option<Instant>,
    endpoint: u8,
    profile: u16,
}
pub(crate) struct Device<'a> {
    pub ieee: u64,
    pub name: &'a str,
    pub endpoint: u8,
    pub profile: u16,
    pub mains: Option<bool>,
    pub receiver_on: Option<bool>,
}
pub(crate) struct Probe {
    pub ieee: u64,
    pub endpoint: u8,
    pub profile: u16,
}
struct ActiveProbe {
    ieee: u64,
    deadline: Instant,
    submitted: Option<Destination>,
}
pub(crate) struct Tracker {
    peers: BTreeMap<u64, Peer>,
    active: BTreeSet<(Instant, u64)>,
    startup_only: BTreeMap<u64, Probe>,
    passive: BTreeSet<(Instant, u64)>,
    probing: Option<ActiveProbe>,
    next_probe: Instant,
    dirty: BTreeSet<u64>,
    publish_retry: Option<Instant>,
    saved: BTreeMap<u64, Sighting>,
    directory: Option<PathBuf>,
    save_due: Option<Instant>,
    writing: Option<JoinHandle<Result<()>>>,
    rng: RandomState,
}
impl Tracker {
    pub fn new(c: &Catalogue, now: Instant) -> Result<Self> {
        let saved = if let Some(dir) = &c.directory {
            match crate::store::open_regular(
                &dir.join("availability-seen.json"),
                File::options().read(true),
            ) {
                Ok(f) => serde_json::from_reader(f)?,
                Err(e) if e.kind() == ErrorKind::NotFound => BTreeMap::new(),
                Err(e) => return Err(e.into()),
            }
        } else {
            BTreeMap::new()
        };
        Ok(Self {
            peers: BTreeMap::new(),
            active: BTreeSet::new(),
            startup_only: BTreeMap::new(),
            passive: BTreeSet::new(),
            probing: None,
            next_probe: now,
            dirty: BTreeSet::new(),
            publish_retry: None,
            saved,
            directory: c.directory.clone(),
            save_due: None,
            writing: None,
            rng: RandomState::new(),
        })
    }
    pub fn register(&mut self, c: &Catalogue, device: Device<'_>, now: Instant, wall_ms: u64) {
        let Device {
            ieee: id,
            name,
            endpoint,
            profile,
            mains, receiver_on,
        } = device;
        let prior = self.saved.get(&id).copied();
        let walk = mains.is_none() && c.walk_enabled(id, receiver_on)
            && !prior.is_some_and(|s| s.departed);
        let Some(settings) = c.settings(
            id, receiver_on,
            mains.or_else(|| prior.map(|p| p.active)).unwrap_or(false),
        ) else {
            if walk { self.startup_only.insert(id, Probe { ieee: id, endpoint, profile }); }
            return;
        };
        let departed = prior.is_some_and(|p| p.departed);
        let stamp = match prior {
            Some(p) => p.last_ms,
            None => c.devices.get(&id).and_then(|m| m.last_seen),
        };
        let elapsed = stamp
            .and_then(|ms| wall_ms.checked_sub(ms))
            .map(Duration::from_millis);
        let remaining = elapsed
            .and_then(|age| settings.timeout.checked_sub(age))
            .filter(|d| !d.is_zero());
        let online = remaining.is_some();
        self.finish_probe(id, now);
        self.clear_due(id);
        self.peers.insert(
            id,
            Peer {
                name: name.into(),
                settings,
                online,
                retry: None,
                multiplier: 1.,
                suspended: false,
                departed,
                due: None,
                endpoint,
                profile,
            },
        );
        self.dirty.insert(id);
        match remaining {
            _ if walk && settings.active => self.schedule(id, now),
            Some(wait) => self.schedule(id, now + wait),
            None if settings.active && !departed => self.schedule(id, now),
            None => (),
        }
    }
    /// The ledger is local to the broker endpoint and base topic. Persist new
    /// ownership before publishing and keep retired names until MQTT PUBACK.
    pub async fn reconcile_topics(&self, out: &crate::mqtt::Publisher) -> Result<()> {
        let Some(dir) = &self.directory else {
            return Ok(());
        };
        let path = dir.join("availability-topics.json");
        let mut ledger: BTreeMap<String, BTreeSet<String>> =
            match crate::store::open_regular(&path, File::options().read(true)) {
                Ok(f) => serde_json::from_reader(f)?,
                Err(e) if e.kind() == ErrorKind::NotFound => BTreeMap::new(),
                Err(e) => return Err(e.into()),
            };
        let scope = out.ownership_scope();
        let active: BTreeSet<_> = self.peers.values().map(|p| p.name.clone()).collect();
        let previous = ledger.get(scope).cloned().unwrap_or_default();
        if previous == active {
            return Ok(());
        }
        let union: BTreeSet<_> = previous.union(&active).cloned().collect();
        if union != previous {
            ledger.insert(scope.into(), union);
            write_snapshot(dir, "availability-topics.json", &ledger)?;
        }
        if previous.is_subset(&active) {
            return Ok(());
        }
        for name in previous.difference(&active) {
            out.retire_availability(name).await?;
        }
        ledger.insert(scope.into(), active);
        write_snapshot(dir, "availability-topics.json", &ledger)
    }
    fn clear_due(&mut self, id: u64) {
        if let Some(p) = self.peers.get_mut(&id)
            && let Some(deadline) = p.due.take()
        {
            if p.settings.active {
                self.active.remove(&(deadline, id));
            } else {
                self.passive.remove(&(deadline, id));
            }
        }
    }
    fn schedule(&mut self, id: u64, when: Instant) {
        self.clear_due(id);
        let p = self.peers.get_mut(&id).unwrap();
        p.due = Some(when);
        if p.settings.active {
            self.active.insert((when, id));
        } else {
            self.passive.insert((when, id));
        }
    }
    fn interval(&self, id: u64, now: Instant) -> Duration {
        let p = &self.peers[&id];
        let jitter = self.rng.hash_one((id, now)) % (u64::from(p.settings.jitter) + 1);
        let seconds =
            (p.settings.timeout.as_secs_f64() + 1. + (jitter as f64 / 1000.)) * p.multiplier;
        Duration::from_secs_f64(seconds.min(2_147_483.647))
    }
    fn publish(&mut self, id: u64, online: bool) {
        let p = self.peers.get_mut(&id).unwrap();
        if p.online != online {
            p.online = online;
            self.dirty.insert(id);
        }
    }
    fn finish_probe(&mut self, id: u64, now: Instant) {
        if self.probing.as_ref().is_some_and(|p| p.ieee == id) {
            self.probing = None;
            self.next_probe = now + PROBE_SPACING;
        }
    }
    pub fn seen(&mut self, id: u64, now: Instant, wall_ms: u64) {
        self.startup_only.remove(&id);
        self.finish_probe(id, now);
        let Some(p) = self.peers.get_mut(&id) else {
            return;
        };
        if p.departed {
            return;
        }
        p.retry = None;
        p.suspended = false;
        p.multiplier = 1.;
        let wait = if p.settings.active {
            self.interval(id, now)
        } else {
            p.settings.timeout
        };
        self.publish(id, true);
        self.saved.insert(
            id,
            Sighting {
                last_ms: Some(wall_ms),
                active: self.peers[&id].settings.active,
                departed: false,
            },
        );
        self.save_due.get_or_insert(now + CHECKPOINT);
        self.schedule(id, now + wait);
    }
    pub fn admit(&mut self, id: u64, now: Instant) {
        let Some(p) = self.peers.get_mut(&id) else {
            return;
        };
        if !p.departed {
            return;
        }
        p.departed = false;
        if let Some(sighting) = self.saved.get_mut(&id) {
            sighting.departed = false;
            self.save_due.get_or_insert(now + CHECKPOINT);
        }
        if p.settings.active {
            self.schedule(id, now);
        }
    }
    pub fn left(&mut self, id: u64, now: Instant) {
        self.startup_only.remove(&id);
        self.finish_probe(id, now);
        let Some(p) = self.peers.get_mut(&id) else {
            return;
        };
        p.departed = true;
        self.clear_due(id);
        self.publish(id, false);
        self.saved.insert(
            id,
            Sighting {
                last_ms: None,
                active: self.peers[&id].settings.active,
                departed: self.peers[&id].departed,
            },
        );
        self.save_due.get_or_insert(now + CHECKPOINT);
    }
    pub fn all_states_queued(&self) -> bool {
        self.dirty.is_empty()
    }

    pub fn has_peers(&self) -> bool {
        !self.peers.is_empty() || !self.startup_only.is_empty() || self.probing.is_some()
    }
    pub fn deadline(&self) -> Option<Instant> {
        let active = match &self.probing {
            Some(p) => Some(p.deadline),
            None if !self.startup_only.is_empty() => Some(self.next_probe),
            None => self.active.first().map(|(d, _)| (*d).max(self.next_probe)),
        };
        [
            active,
            self.passive.first().map(|(d, _)| *d),
            self.save_due,
            self.publish_retry,
        ]
        .into_iter()
        .flatten()
        .min()
    }
    pub fn due(&mut self, now: Instant, current: impl Fn(&Destination) -> bool) -> Result<Option<Probe>> {
        if self.writing.as_ref().is_some_and(|job| job.is_finished()) {
            self.join_writer()?;
        }
        if self.save_due.is_some_and(|d| d <= now) {
            if self.writing.is_some() {
                self.save_due = Some(now + Duration::from_secs(1));
            } else {
                if let Some(dir) = self.directory.clone() {
                    let saved = self.saved.clone();
                    self.writing = Some(
                        std::thread::Builder::new()
                            .name("device-availability".into())
                            .spawn(move || {
                                write_snapshot(&dir, "availability-seen.json", &saved)
                            })?,
                    );
                }
                self.save_due = None;
            }
        }
        while let Some(&(d, id)) = self.passive.first() {
            if d > now {
                break;
            }
            self.clear_due(id);
            self.publish(id, false);
        }
        if let Some(probe) = &self.probing {
            let retired = probe.submitted.as_ref().is_some_and(|to| !current(to));
            if !retired && now < probe.deadline {
                return Ok(None);
            }
            let id = probe.ieee;
            let probe = self.probing.take().unwrap();
            self.next_probe = now + PROBE_SPACING;
            if self.peers.contains_key(&id) {
                if let Some(to) = probe.submitted.filter(|_| !retired) {
                    self.failed(id, to, now);
                } else {
                    self.schedule(id, now + self.interval(id, now));
                }
            }
        }
        if now < self.next_probe { return Ok(None); }
        let probe = if let Some(&(_, id)) = self.active.first().filter(|(when, _)| *when <= now) {
            self.clear_due(id);
            let p = &self.peers[&id];
            Probe { ieee: id, endpoint: p.endpoint, profile: p.profile }
        } else if let Some((_, probe)) = self.startup_only.pop_first() {
            probe
        } else { return Ok(None); };
        self.probing = Some(ActiveProbe { ieee: probe.ieee, deadline: now + PROBE_LIMIT, submitted: None });
        Ok(Some(probe))
    }
    pub fn awaiting_address(&self, id: u64) -> bool {
        self.probing.as_ref().is_some_and(|p| p.ieee == id && p.submitted.is_none())
    }
    pub fn wait_for_address(&mut self, id: u64, until: Instant) {
        if let Some(p) = self.probing.as_mut().filter(|p| p.ieee == id && p.submitted.is_none()) {
            p.deadline = until;
        }
    }
    /// Arm the reply timeout only once the adapter accepts the probe.
    pub fn submitted(&mut self, to: Destination, now: Instant) {
        if let Some(p) = &mut self.probing && p.ieee == to.ieee() && p.submitted.is_none() {
            p.submitted = Some(to);
            p.deadline = now + PROBE_LIMIT;
        }
    }

    /// A missing address or refused submission gives us no information about the peer.
    pub fn not_submitted(&mut self, id: u64, now: Instant) {
        if !self.probing.as_ref().is_some_and(|p| p.ieee == id && p.submitted.is_none()) {
            return;
        }
        self.finish_probe(id, now);
        if self.peers.contains_key(&id) { self.schedule(id, now + self.interval(id, now)); }
    }

    fn failed(&mut self, id: u64, to: Destination, now: Instant) {
        let p = self.peers.get_mut(&id).unwrap();
        if p.online && !p.retry.as_ref().is_some_and(|first| first.same_assignment(&to)) {
            p.retry = Some(to);
            self.schedule(id, now + Duration::from_secs(3));
            return;
        }
        let was_online = p.online;
        p.retry = None;
        if p.settings.backoff {
            p.suspended = p.settings.pause > 0 && p.multiplier >= f64::from(p.settings.pause);
            p.multiplier = (p.multiplier * if was_online { 1.5 } else { 2. }).min(2_147_483.);
        }
        let pause = p.suspended;
        self.publish(id, false);
        self.saved.insert(
            id,
            Sighting {
                last_ms: None,
                active: self.peers[&id].settings.active,
                departed: self.peers[&id].departed,
            },
        );
        self.save_due.get_or_insert(now + CHECKPOINT);
        if !pause {
            let wait = self.interval(id, now);
            self.schedule(id, now + wait);
        }
    }
    pub fn publish_due(&mut self, out: &crate::mqtt::Publisher, now: Instant) -> Result<()> {
        if self.publish_retry.is_some_and(|d| d > now) {
            return Ok(());
        }
        self.publish_retry = None;
        while let Some(&id) = self.dirty.first() {
            let p = &self.peers[&id];
            match out.device_availability(&p.name, p.online) {
                Ok(()) => {
                    self.dirty.remove(&id);
                }
                Err(crate::mqtt::SubmitError::Full) => {
                    self.publish_retry = Some(now + Duration::from_millis(50));
                    break;
                }
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }
    fn join_writer(&mut self) -> Result<()> {
        if let Some(job) = self.writing.take() {
            job.join()
                .map_err(|_| anyhow::anyhow!("availability writer panicked"))??;
        }
        Ok(())
    }
}
fn write_snapshot(dir: &Path, name: &str, value: &impl Serialize) -> Result<()> {
    use std::io::Write;
    let mut file = tempfile::NamedTempFile::new_in(dir)?;
    file.write_all(&serde_json::to_vec(value)?)?;
    file.as_file().sync_all()?;
    file.persist(dir.join(name))?;
    File::open(dir)?.sync_all()?;
    Ok(())
}
impl Drop for Tracker {
    fn drop(&mut self) {
        if let Err(e) = self.join_writer() {
            eprintln!("Device availability checkpoint failed: {e}");
        }
        if self.save_due.is_some()
            && let Some(dir) = &self.directory
            && let Err(e) = write_snapshot(dir, "availability-seen.json", &self.saved)
        {
            eprintln!("Saving device availability at shutdown: {e}");
        }
    }
}
pub(crate) fn wall_ms() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_millis()
        .try_into()?)
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
