//! Each committed row authorizes one managed IAS server. A reserved ID stays
//! occupied until its membership ends, including across delivery failures.
use super::super::Identity;
use anyhow::{Result, ensure};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition, backends::FileBackend};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    path::Path,
};

const OWNER: TableDefinition<u8, u64> = TableDefinition::new("cie_v1");
const LEFT: TableDefinition<u64, ()> = TableDefinition::new("departed_v1");
const ZONES: TableDefinition<(u64, u8), (u16, u8, bool)> = TableDefinition::new("zones_v1");
pub(super) const UNASSIGNED: u8 = 255;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Row {
    pub profile: u16,
    pub id: u8,
    pub confirmed: bool,
}

pub(super) struct Table {
    db: Database,
    pub left: BTreeSet<u64>,
    pub rows: BTreeMap<Identity, Row>,
}
impl Table {
    pub fn open(dir: &Path) -> Result<Self> {
        let mut opts = File::options();
        opts.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let file = crate::store::open_regular(&dir.join("ias-zones.redb"), &mut opts)?;
        let db = Database::builder().create_with_backend(FileBackend::new(file))?;
        File::open(dir)?.sync_all()?;
        Ok(Self {
            db,
            rows: BTreeMap::new(),
            left: BTreeSet::new(),
        })
    }
    pub fn attach(&mut self, cie: u64) -> Result<()> {
        ensure!(
            !matches!(cie, 0 | u64::MAX),
            "invalid IAS coordinator identity"
        );
        let tx = self.db.begin_write()?;
        {
            let mut t = tx.open_table(OWNER)?;
            let previous = t.get(0)?.map(|v| v.value());
            ensure!(
                previous.is_none_or(|n| n == cie),
                "IAS state belongs to another coordinator"
            );
            t.insert(0, cie)?;
        }
        tx.open_table(LEFT)?;
        tx.open_table(ZONES)?;
        tx.commit()?;
        let read = self.db.begin_read()?;
        let mut rows = BTreeMap::new();
        let mut used = [false; 255];
        for e in read.open_table(ZONES)?.iter()? {
            let (key, val) = e?;
            let (profile, id, confirmed) = val.value();
            let (ieee, ep) = key.value();
            ensure!(
                !matches!(ieee, 0 | u64::MAX)
                    && (1..=240).contains(&ep)
                    && matches!(profile, 0x0104 | 0xc05e),
                "invalid IAS membership"
            );
            if id != UNASSIGNED {
                ensure!(
                    !std::mem::replace(&mut used[usize::from(id)], true),
                    "duplicate saved IAS ID"
                );
            } else {
                ensure!(!confirmed, "confirmed unassigned IAS zone");
            }
            rows.insert(
                (ieee, ep),
                Row {
                    profile,
                    id,
                    confirmed,
                },
            );
        }
        let mut left = BTreeSet::new();
        for entry in read.open_table(LEFT)?.iter()? {
            left.insert(entry?.0.value());
        }
        self.left = left;
        self.rows = rows;
        Ok(())
    }
    pub fn save(&mut self, key: Identity, row: Row) -> Result<()> {
        if self.rows.get(&key) == Some(&row) {
            return Ok(());
        }
        let tx = self.db.begin_write()?;
        tx.open_table(ZONES)?
            .insert(key, (row.profile, row.id, row.confirmed))?;
        tx.commit()?;
        self.rows.insert(key, row);
        Ok(())
    }
    pub fn leave(&mut self, ieee: u64) -> Result<()> {
        let keys: Vec<_> = self
            .rows
            .range((ieee, 0)..=(ieee, u8::MAX))
            .map(|(&k, _)| k)
            .collect();
        let tx = self.db.begin_write()?;
        tx.open_table(LEFT)?.insert(ieee, ())?;
        {
            let mut t = tx.open_table(ZONES)?;
            for &key in &keys {
                t.remove(key)?;
            }
        }
        tx.commit()?;
        self.left.insert(ieee);
        for key in keys {
            self.rows.remove(&key);
        }
        Ok(())
    }
    pub fn readmit(&mut self, ieee: u64) -> Result<()> {
        if self.left.contains(&ieee) {
            let tx = self.db.begin_write()?;
            tx.open_table(LEFT)?.remove(ieee)?;
            tx.commit()?;
            self.left.remove(&ieee);
        }
        Ok(())
    }
    pub fn reserve(&mut self, key: Identity, old: Option<u8>) -> Result<Option<u8>> {
        let row = self.rows[&key];
        if row.id != UNASSIGNED {
            return Ok(old.is_none_or(|v| v == row.id).then_some(row.id));
        }
        let free = |id: u8| id != UNASSIGNED && self.rows.values().all(|r| r.id != id);
        let id = match old {
            Some(id) => free(id).then_some(id),
            None => (0..UNASSIGNED).find(|&id| free(id)),
        };
        if let Some(id) = id {
            self.save(key, Row { id, ..row })?;
        }
        Ok(id)
    }
}
