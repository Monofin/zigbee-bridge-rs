//! Zigbee R8 OTA upgrade server for administrator-selected publisher images.
//! A catalogue entry is eligible only after a client reports its own identity
//! and running version. A local schedule separately authorizes one IEEE address.
//!
//! Configure a trusted local JSON manifest with an `images` array. Each
//! entry has `id`, `file` (a basename alongside the manifest), `source`
//! (the publisher HTTPS URL) and `sha256` (the expected full-file digest).
//! The administrator obtains that digest from a trusted source. The server
//! checks integrity; the client retains its normal image/signature validation.
//! The in-memory catalogue accepts 128 MiB of unmodified file contents.
//!
//! When persistence is enabled, unexpired schedules are bound to the image
//! digest. Queries and transfers restart with fresh peer evidence. A client
//! success consumes the schedule and leaves a receipt for duplicate Upgrade
//! End requests. Only a later device version report establishes installation.
mod image;
use crate::store::open_regular;
use anyhow::{Context, Result, ensure};
use image::{Image, Key};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::File,
    io,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub(crate) const NO_IMAGE: u8 = 0x98;
pub(crate) const MALFORMED: u8 = 0x80;

#[derive(Clone, Copy)]
struct Query {
    endpoint: u8,
    key: Key,
    hardware: Option<u16>,
}

impl Query {
    fn matches(self, ieee: u64, image: &Image) -> bool {
        image.key.version > self.key.version && self.same_product(ieee, image)
    }
    fn same_product(self, ieee: u64, image: &Image) -> bool {
        image.key.manufacturer == self.key.manufacturer
            && image.key.image_type == self.key.image_type
            && image.destination.is_none_or(|dest| dest == ieee)
            && image
                .hardware
                .is_none_or(|(min, max)| self.hardware.is_some_and(|hw| (min..=max).contains(&hw)))
    }
}

#[derive(Debug, Serialize)]
pub struct Check {
    pub current: Option<u32>,
    pub available: Option<bool>,
    pub image: Option<String>,
    pub latest: Option<u32>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Schedule {
    id: String,
    digest: String,
    created: u64,
    expires: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    completed_endpoint: Option<u8>,
}
impl Schedule {
    fn active(&self, now: u64) -> bool {
        (self.created..self.expires).contains(&now)
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    version: u8,
    schedules: BTreeMap<u64, Schedule>,
}

#[derive(Default)]
pub struct Server {
    images: BTreeMap<String, Image>,
    queries: BTreeMap<u64, Query>,
    schedules: BTreeMap<u64, Schedule>,
    offered: BTreeMap<u64, u8>,
    directory: Option<PathBuf>,
    uncertain: bool,
}

fn now() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

impl Server {
    /// Trust comes from the local manifest, including its independently
    /// obtained publisher digest. Loading never downloads or modifies an image.
    pub fn load(manifest: &Path) -> Result<Self> {
        Ok(Self {
            images: image::load(manifest)?,
            ..Self::default()
        })
    }

    pub fn images(&self) -> impl Iterator<Item = &str> {
        self.images.keys().map(String::as_str)
    }

    /// Call once at startup under exclusive ownership of this state directory.
    /// In-flight transfers require a new compatible query after process restart.
    pub fn persist_in(&mut self, directory: &Path) -> Result<()> {
        ensure!(
            self.directory.is_none() && self.schedules.is_empty() && self.queries.is_empty(),
            "OTA persistence requires a fresh server"
        );
        let path = directory.join("ota-schedules.json");
        let mut saved = match open_regular(&path, File::options().read(true)) {
            Ok(file) => {
                let snapshot: Snapshot = serde_json::from_reader(io::BufReader::new(file))?;
                ensure!(snapshot.version == 1, "unsupported OTA state version");
                snapshot.schedules
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => return Err(e.into()),
        };
        let time = now()?;
        saved.retain(|_, s| {
            s.active(time) && self.images.get(&s.id).is_some_and(|i| i.digest == s.digest)
        });
        self.directory = Some(directory.to_owned());
        // Flush removed authorizations so reintroducing an old catalogue cannot
        // resurrect a schedule rejected on an intervening restart.
        self.commit(saved)
    }

    fn commit(&mut self, next: BTreeMap<u64, Schedule>) -> Result<()> {
        ensure!(
            !self.uncertain,
            "OTA authorization durability uncertain; reopen state"
        );
        if let Some(dir) = &self.directory {
            let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
            let snapshot = Snapshot {
                version: 1,
                schedules: next,
            };
            serde_json::to_writer(tmp.as_file_mut(), &snapshot)?;
            tmp.as_file().sync_all()?;
            tmp.persist(dir.join("ota-schedules.json"))?;
            if let Err(error) = File::open(dir).and_then(|f| f.sync_all()) {
                self.schedules.clear();
                self.offered.clear();
                self.uncertain = true;
                return Err(error).context("syncing OTA authorization directory");
            }
            self.schedules = snapshot.schedules;
        } else {
            self.schedules = next;
        }
        Ok(())
    }

    fn best(&self, ieee: u64, query: Query) -> Result<Option<&str>> {
        let mut best = None;
        let mut highest = None;
        let mut ambiguous = false;
        for (id, image) in &self.images {
            if !query.matches(ieee, image) {
                continue;
            }
            if Some(image.key.version) == highest {
                ambiguous = true;
            }
            if Some(image.key.version) > highest {
                best = Some(id.as_str());
                highest = Some(image.key.version);
                ambiguous = false;
            }
        }
        ensure!(
            !ambiguous,
            "multiple matching images at highest version; select an image ID"
        );
        Ok(best)
    }

    pub(crate) fn known_product(&self, ieee: u64) -> bool {
        self.queries.get(&ieee).is_some_and(|query| {
            self.images
                .values()
                .any(|image| query.same_product(ieee, image))
        })
    }
    pub(crate) fn is_transferring(&self, ieee: u64) -> bool {
        self.offered.contains_key(&ieee)
    }
    pub(crate) fn observed_endpoint(&self, ieee: u64) -> Option<u8> {
        self.queries.get(&ieee).map(|q| q.endpoint)
    }
    pub(crate) fn authorization(&self, ieee: u64) -> Result<Option<(u32, bool)>> {
        let time = now()?;
        Ok(self.authorized(ieee, time).map(|i| {
            (
                i.key.version,
                self.schedules[&ieee].completed_endpoint.is_some(),
            )
        }))
    }

    pub fn check(&self, ieee: u64) -> Result<Check> {
        let Some(&query) = self.queries.get(&ieee) else {
            return Ok(Check {
                current: None,
                available: None,
                image: None,
                latest: None,
            });
        };
        let best = self.best(ieee, query)?;
        Ok(Check {
            current: Some(query.key.version),
            available: Some(best.is_some()),
            image: best.map(str::to_owned),
            latest: best.map(|id| self.images[id].key.version),
        })
    }

    pub fn schedule(&mut self, ieee: u64, image: Option<&str>, ttl: Duration) -> Result<()> {
        ensure!(ieee != 0 && ieee != u64::MAX, "invalid OTA destination");
        let query = self.queries.get(&ieee).copied();
        let id = match image {
            Some(id) => id,
            None => self
                .best(ieee, query.context("awaiting device OTA query")?)?
                .context("no applicable upgrade image")?,
        };
        let image = self.images.get(id).context("unknown OTA image")?;
        ensure!(
            image.destination.is_none_or(|dest| dest == ieee),
            "image is bound to a different device"
        );
        if let Some(q) = query {
            ensure!(
                q.matches(ieee, image),
                "image is incompatible with observed device"
            );
        }
        let created = now()?;
        ensure!(ttl.as_secs() > 0, "schedule must last at least one second");
        let expires = created
            .checked_add(ttl.as_secs())
            .context("OTA schedule expiry out of range")?;
        let mut next = self.schedules.clone();
        next.retain(|_, s| s.active(created));
        next.insert(
            ieee,
            Schedule {
                id: id.to_owned(),
                digest: image.digest.clone(),
                created,
                expires,
                completed_endpoint: None,
            },
        );
        self.commit(next)?;
        self.offered.remove(&ieee);
        Ok(())
    }

    pub fn cancel(&mut self, ieee: u64) -> Result<bool> {
        let mut next = self.schedules.clone();
        let removed = next.remove(&ieee).is_some();
        if removed {
            self.commit(next)?;
        }
        self.offered.remove(&ieee);
        Ok(removed)
    }
    pub(crate) fn retire_transfer(&mut self, ieee: u64) {
        self.offered.remove(&ieee);
        self.queries.remove(&ieee);
    }

    pub fn forget(&mut self, ieee: u64) -> Result<()> {
        self.retire_transfer(ieee);
        self.cancel(ieee)?;
        Ok(())
    }

    fn authorized(&self, ieee: u64, time: u64) -> Option<&Image> {
        let schedule = self.schedules.get(&ieee)?;
        schedule
            .active(time)
            .then(|| self.images.get(&schedule.id))
            .flatten()
    }

    fn query(&mut self, ieee: u64, query: Query, time: u64) -> Option<(Key, u32)> {
        self.queries.insert(ieee, query);
        self.offered.remove(&ieee);
        if self.schedules.get(&ieee)?.completed_endpoint.is_some() {
            return None;
        }
        let image = self
            .authorized(ieee, time)
            .filter(|image| query.matches(ieee, image))?;
        let result = (image.key, image.bytes.len() as u32);
        self.offered.insert(ieee, query.endpoint);
        Some(result)
    }

    fn offered_image(&self, ieee: u64, endpoint: u8, key: Key, time: u64) -> Option<&Image> {
        if self.offered.get(&ieee) != Some(&endpoint) {
            return None;
        }
        self.authorized(ieee, time).filter(|image| image.key == key)
    }
}

mod service;
pub(crate) use service::Response;
