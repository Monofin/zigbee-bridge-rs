//! Retained Home Assistant configs owned by this broker, namespace and physical member.
use super::Bindings;
use crate::mqtt::Publisher;
use anyhow::Result;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{File, OpenOptions},
    io::ErrorKind,
    path::{Path, PathBuf},
};

type Members = BTreeMap<u64, BTreeSet<String>>;

pub(super) struct Ownership {
    directory: PathBuf,
    scopes: BTreeMap<String, Members>,
}

impl Ownership {
    pub(super) fn open(directory: &Path) -> Result<Self> {
        let path = directory.join("discovery-topics.json");
        let scopes = match crate::store::open_regular(&path, OpenOptions::new().read(true)) {
            Ok(file) => serde_json::from_reader(file)?,
            Err(e) if e.kind() == ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => return Err(e.into()),
        };
        Ok(Self {
            directory: directory.to_owned(),
            scopes,
        })
    }

    fn scope(out: &Publisher) -> Result<String> {
        Ok(serde_json::to_string(&(
            out.ownership_scope(),
            out.discovery_prefix(),
        ))?)
    }

    fn save(&self) -> Result<()> {
        let temp = tempfile::NamedTempFile::new_in(&self.directory)?;
        serde_json::to_writer(temp.as_file(), &self.scopes)?;
        temp.as_file().sync_all()?;
        temp.persist(self.directory.join("discovery-topics.json"))?;
        File::open(&self.directory)?.sync_all()?;
        Ok(())
    }

    // Claim before publication. Keeping old topics until PUBACK makes an interrupted
    // update recoverable on either side of the atomic member-file replacement.
    pub(super) fn claim(
        &mut self,
        ieee: u64,
        active: &BTreeSet<String>,
        out: &Publisher,
    ) -> Result<()> {
        let owned = self
            .scopes
            .entry(Self::scope(out)?)
            .or_default()
            .entry(ieee)
            .or_default();
        if !active.is_subset(owned) {
            owned.extend(active.iter().cloned());
            self.save()?;
        }
        Ok(())
    }

    pub(super) async fn after_interview(
        &self,
        ieee: u64,
        devices: &Bindings,
        out: &Publisher,
    ) -> Result<()> {
        if !devices.complete_descriptors.contains(&ieee) {
            return Ok(());
        }
        let active = devices.member_topics(ieee, out)?;
        if let Some(owned) = self
            .scopes
            .get(&Self::scope(out)?)
            .and_then(|m| m.get(&ieee))
        {
            for topic in owned.difference(&active) {
                out.remove_member_topic(ieee, topic, false).await?;
            }
        }
        Ok(())
    }

    pub(super) async fn startup(&mut self, devices: &Bindings, out: &Publisher) -> Result<()> {
        let scope = Self::scope(out)?;
        let mut active = Members::new();
        for &ieee in devices.peers.keys() {
            active.insert(ieee, devices.member_topics(ieee, out)?);
        }
        let owned = self.scopes.entry(scope.clone()).or_default();
        let mut changed = false;
        for (&ieee, topics) in &active {
            let previous = owned.entry(ieee).or_default();
            if !topics.is_subset(previous) {
                previous.extend(topics.iter().cloned());
                changed = true;
            }
        }
        if changed {
            self.save()?;
        }
        // A sleeping member can time out before reporting all its endpoints.
        // Keep its old topics until the device supplies a complete descriptor set.
        active.retain(|ieee, _| devices.complete_descriptors.contains(ieee));
        let mut retired = false;
        for (&ieee, current) in &active {
            for topic in self.scopes[&scope][&ieee].difference(current) {
                out.remove_member_topic(ieee, topic, true).await?;
                retired = true;
            }
        }
        if retired {
            self.scopes
                .get_mut(&scope)
                .expect("active ownership scope")
                .extend(active);
            self.save()?;
        }
        Ok(())
    }
}
