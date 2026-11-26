//! An administrator can label a physical member without changing its MQTT route.
//! Imported routes remain the defaults until an explicit route change is committed.
use super::Bindings;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fs::File, io::ErrorKind, path::Path};

const FILE: &str = "device-names.json";

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    display_name: String,
    route: Option<String>,
}

#[derive(Default, Clone, Serialize, Deserialize)]
pub(super) struct Names(BTreeMap<u64, Entry>);

impl Names {
    pub(super) fn display(&self, ieee: u64) -> Option<&str> {
        self.0.get(&ieee).map(|n| n.display_name.as_str())
    }

    pub(super) fn displays(&self) -> impl Iterator<Item = (u64, String)> + '_ {
        self.0.iter().map(|(&id, n)| (id, n.display_name.clone()))
    }

    pub(super) fn route(&self, ieee: u64, imported: &BTreeMap<u64, String>) -> String {
        self.0
            .get(&ieee)
            .and_then(|n| n.route.as_ref())
            .or_else(|| imported.get(&ieee))
            .cloned()
            .unwrap_or_else(|| format!("0x{ieee:016x}"))
    }

    // Call under the coordinator's exclusive Store lock. Prepared files cannot
    // become authoritative until the complete new table has been synced.
    pub(super) fn prepare(&self, dir: &Path) -> Result<tempfile::NamedTempFile> {
        let file = tempfile::NamedTempFile::new_in(dir)?;
        serde_json::to_writer(file.as_file(), self)?;
        file.as_file().sync_all()?;
        Ok(file)
    }
}

pub(super) fn commit(staged: tempfile::NamedTempFile, dir: &Path) -> Result<()> {
    staged.persist(dir.join(FILE))?;
    File::open(dir)?.sync_all()?;
    Ok(())
}

impl Bindings {
    pub fn load_naming(&mut self, dir: &Path) -> Result<()> {
        self.naming = match crate::store::open_regular(&dir.join(FILE), File::options().read(true))
        {
            Ok(file) => serde_json::from_reader(file).context("invalid device naming registry")?,
            Err(e) if e.kind() == ErrorKind::NotFound => Names::default(),
            Err(e) => return Err(e.into()),
        };
        Ok(())
    }

    pub(super) fn named_candidate(
        &self,
        ieee: u64,
        name: &str,
        route: bool,
        imported: &BTreeMap<u64, String>,
    ) -> Result<Self> {
        let old = &self.peers.get(&ieee).context("unknown device")?.0;
        ensure!(
            !name.is_empty()
                && name.trim() == name
                && name.len() <= 512
                && !name.chars().any(char::is_control),
            "invalid device name"
        );
        let mut next = self.clone();
        let entry = next.naming.0.entry(ieee).or_insert_with(|| Entry {
            display_name: old.clone(),
            route: None,
        });
        if !route {
            entry.display_name = name.into();
            return Ok(next);
        }
        if name == old {
            return Ok(next);
        }
        ensure!(
            crate::mqtt::device_name(name) && !name.contains('/') && !name.starts_with('$'),
            "use a single MQTT topic level for the device route"
        );
        if let Some(hex) = name.strip_prefix("0x")
            && hex.len() == 16
            && u64::from_str_radix(hex, 16).is_ok()
        {
            ensure!(
                name == format!("0x{ieee:016x}"),
                "IEEE address aliases are reserved"
            );
        }
        // Also reserve the namespace of migrated members whose interview is incomplete.
        for (id, (other, _)) in &self.peers {
            ensure!(
                *id == ieee || (other != name && !other.starts_with(&format!("{name}/"))),
                "MQTT device route already in use"
            );
        }
        for id in imported
            .keys()
            .chain(self.naming.0.keys())
            .filter(|id| **id != ieee)
        {
            let other = self.naming.route(*id, imported);
            ensure!(
                other != name && !other.starts_with(&format!("{name}/")),
                "MQTT device route already in use"
            );
        }
        entry.route = Some(name.into());
        next.peers.get_mut(&ieee).unwrap().0 = name.into();
        next.topics.retain(|_, id| id.0 != ieee);
        for (&id, b) in next.devices.range_mut((ieee, 0)..=(ieee, u8::MAX)) {
            let suffix = b
                .name
                .strip_prefix(old)
                .context("inconsistent endpoint route")?;
            b.name = format!("{name}{suffix}");
            b.device_name = name.into();
            ensure!(
                crate::mqtt::device_name(&b.name)
                    && next.topics.insert(b.name.clone(), id).is_none(),
                "endpoint route collision"
            );
        }
        Ok(next)
    }

    pub(super) fn name_info(&self, ieee: u64) -> serde_json::Value {
        let route = &self.peers[&ieee].0;
        serde_json::json!({"ieee":format!("0x{ieee:016x}"), "display_name":self.naming.display(ieee).unwrap_or(route), "route":route})
    }
}
