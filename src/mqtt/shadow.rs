//! Checkpoint reported values by endpoint. Discovery deletions commit immediately.
use anyhow::Result;
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition, backends::FileBackend};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{File, OpenOptions},
    path::Path,
    sync::{Arc, Mutex},
};

pub(super) type Identity = (u64, u8);
const STATES: TableDefinition<Identity, (u64, bool, &[u8])> = TableDefinition::new("reported_v1");
const TOMBSTONES: TableDefinition<(&str, &str), ()> = TableDefinition::new("retained_deletes_v1");
const DEPARTED: TableDefinition<u64, ()> = TableDefinition::new("departed_v1");
const CLOCK: TableDefinition<u8, u64> = TableDefinition::new("revision_v1");

pub(super) struct Shadow {
    db: Database,
    staged: Mutex<BTreeMap<Identity, Arc<[u8]>>>,
    // Serialize forget with checkpoint, including retirement of the staged copy.
    writer: Mutex<()>,
}
impl Shadow {
    pub fn open(dir: &Path) -> Result<Self> {
        let marker = dir.join("mqtt-state.required");
        let required = marker.try_exists()?;
        let mut opts = OpenOptions::new();
        opts.read(true).write(true).create(!required);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let file = crate::store::open_regular(&dir.join("mqtt-state.redb"), &mut opts)?;
        anyhow::ensure!(
            !required || file.metadata()?.len() > 0,
            "empty MQTT state database in restored state"
        );
        let db = Database::builder().create_with_backend(FileBackend::new(file))?;
        if required {
            let tx = db.begin_read()?;
            tx.open_table(STATES)?;
            tx.open_table(CLOCK)?;
            tx.open_table(TOMBSTONES)?;
            // Older managed stores have no departure ledger. A missing table is
            // the migration signal; propagate corruption and type mismatches.
            match tx.open_table(DEPARTED) {
                Ok(_) => {}
                Err(redb::TableError::TableDoesNotExist(_)) => {
                    let write = db.begin_write()?;
                    Self::migrate_departures(&write, dir)?;
                    write.commit()?;
                }
                Err(error) => return Err(error.into()),
            }
        } else {
            let tx = db.begin_write()?;
            tx.open_table(STATES)?;
            tx.open_table(CLOCK)?;
            tx.open_table(TOMBSTONES)?;
            Self::migrate_departures(&tx, dir)?;
            tx.commit()?;
            crate::store::open_regular(&marker, File::options().create_new(true).write(true))?
                .sync_all()?;
            File::open(dir)?.sync_all()?;
        }
        Ok(Self {
            db,
            staged: Mutex::default(),
            writer: Mutex::new(()),
        })
    }

    /// A pre-migration installation may have recorded confirmed leaves in its
    /// availability checkpoint. Import those exactly once, including on restore.
    fn migrate_departures(tx: &redb::WriteTransaction, dir: &Path) -> Result<()> {
        #[derive(serde::Deserialize)]
        struct Sighting { departed: bool }

        let mut table = tx.open_table(DEPARTED)?;
        let file = match crate::store::open_regular(
            &dir.join("availability-seen.json"), File::options().read(true),
        ) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        let previous: BTreeMap<u64, Sighting> = serde_json::from_reader(file)?;
        for (ieee, sighting) in previous {
            if sighting.departed {
                table.insert(ieee, ())?;
                Self::delete_endpoints(tx, ieee)?;
            }
        }
        Ok(())
    }

    pub fn departed(&self) -> Result<BTreeSet<u64>> {
        let tx = self.db.begin_read()?;
        let table = tx.open_table(DEPARTED)?;
        Ok(table.iter()?.map(|r| r.map(|(key, _)| key.value())).collect::<Result<_, _>>()?)
    }

    /// Serialize the membership transition with checkpointing. A restored leave
    /// and the deletion of all of its endpoint shadows must be visible together.
    pub fn membership(&self, ieee: u64, present: bool) -> Result<()> {
        let _writer = self.writer.lock().unwrap();
        let tx = self.db.begin_write()?;
        if present {
            tx.open_table(DEPARTED)?.remove(ieee)?;
        } else {
            tx.open_table(DEPARTED)?.insert(ieee, ())?;
            Self::delete_endpoints(&tx, ieee)?;
        }
        tx.commit()?;
        if !present {
            self.staged.lock().unwrap().retain(|id, _| id.0 != ieee);
        }
        Ok(())
    }

    fn delete_endpoints(tx: &redb::WriteTransaction, ieee: u64) -> Result<()> {
        let mut states = tx.open_table(STATES)?;
        let endpoints: Vec<Identity> = states.range((ieee, 0)..=(ieee, u8::MAX))?
            .map(|r| r.map(|(k, _)| k.value())).collect::<Result<_, _>>()?;
        for id in endpoints {
            states.remove(id)?;
        }
        Ok(())
    }

    pub fn tombstones(&self, scope: &str) -> Result<Vec<String>> {
        let tx = self.db.begin_read()?;
        let table = tx.open_table(TOMBSTONES)?;
        let mut topics = Vec::new();
        for row in table.iter()? {
            let (key, _) = row?;
            let (owner, topic) = key.value();
            if owner == scope {
                topics.push(topic.to_owned());
            }
        }
        Ok(topics)
    }

    pub fn update_tombstones(&self, scope: &str, changes: &[(&str, bool)]) -> Result<()> {
        if changes.is_empty() {
            return Ok(());
        }
        let tx = self.db.begin_write()?;
        let mut table = tx.open_table(TOMBSTONES)?;
        let mut changed = false;
        for &(topic, remove) in changes {
            let key = (scope, topic);
            if remove {
                if table.get(key)?.is_none() {
                    table.insert(key, ())?;
                    changed = true;
                }
            } else {
                changed |= table.remove(key)?.is_some();
            }
        }
        drop(table);
        if changed {
            tx.commit()?;
        }
        Ok(())
    }

    pub fn get(&self, id: Identity) -> Result<Option<Vec<u8>>> {
        if let Some(bytes) = self.staged.lock().unwrap().get(&id) {
            return Ok(Some(bytes.to_vec()));
        }
        let tx = self.db.begin_read()?;
        Ok(tx
            .open_table(STATES)?
            .get(id)?
            .map(|v| v.value().2.to_vec()))
    }

    pub fn stage(&self, id: Identity, bytes: &[u8]) {
        self.staged.lock().unwrap().insert(id, Arc::from(bytes));
    }

    pub fn has_changes(&self) -> bool {
        !self.staged.lock().unwrap().is_empty()
    }

    pub fn checkpoint(&self) -> Result<usize> {
        let _writer = self.writer.lock().unwrap();
        let batch = self.staged.lock().unwrap().clone();
        if batch.is_empty() {
            return Ok(0);
        }
        let tx = self.db.begin_write()?;
        let mut clock = tx.open_table(CLOCK)?;
        let mut table = tx.open_table(STATES)?;
        let mut revision = clock.get(0)?.map_or(0, |v| v.value());
        let mut changed = 0;
        for (&id, bytes) in &batch {
            if table
                .get(id)?
                .is_some_and(|v| v.value().2 == bytes.as_ref())
            {
                continue;
            }
            revision = revision
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("MQTT revision exhausted"))?;
            // Keep the v1 row readable by earlier versions. Reconnect replays all rows.
            table.insert(id, (revision, true, bytes.as_ref()))?;
            changed += 1;
        }
        if changed > 0 {
            clock.insert(0, revision)?;
        }
        drop(table);
        drop(clock);
        if changed > 0 {
            tx.commit()?;
        } else {
            tx.abort()?;
        }
        let mut staged = self.staged.lock().unwrap();
        for (id, saved) in batch {
            if staged
                .get(&id)
                .is_some_and(|current| Arc::ptr_eq(current, &saved))
            {
                staged.remove(&id);
            }
        }
        Ok(changed)
    }

    #[cfg(test)]
    pub fn remove(&self, id: Identity) -> Result<()> {
        let _writer = self.writer.lock().unwrap();
        let tx = self.db.begin_write()?;
        let existed = tx.open_table(STATES)?.remove(id)?.is_some();
        if existed {
            tx.commit()?;
        }
        self.staged.lock().unwrap().remove(&id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_report_received_during_checkpoint_survives_for_the_next_batch() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let store = Shadow::open(dir.path())?;
        let id = (1, 1);
        store.stage(id, b"first");
        std::thread::scope(|threads| -> Result<()> {
            // Hold redb's writer until the checkpoint has taken its snapshot.
            let writer = store.db.begin_write()?;
            let task = threads.spawn(|| store.checkpoint());
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while Arc::strong_count(&store.staged.lock().unwrap()[&id]) == 1 {
                anyhow::ensure!(
                    std::time::Instant::now() < deadline,
                    "checkpoint did not start"
                );
                std::thread::yield_now();
            }
            store.stage(id, b"second");
            writer.abort()?;
            assert_eq!(task.join().unwrap()?, 1);
            Ok(())
        })?;
        assert_eq!(store.get(id)?.as_deref(), Some(b"second".as_slice()));
        assert_eq!(store.checkpoint()?, 1);
        assert!(!store.has_changes());
        drop(store);
        assert_eq!(
            Shadow::open(dir.path())?.get(id)?.as_deref(),
            Some(b"second".as_slice())
        );
        Ok(())
    }
}
