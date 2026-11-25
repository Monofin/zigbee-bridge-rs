//! Record an accepted transaction before its first side effect. A pending row
//! surviving a restart cannot establish whether the device acted on it.
use anyhow::{Context, Result, ensure};
use redb::{
    Database, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition, backends::FileBackend,
};
use std::{
    collections::BTreeSet,
    fs::File,
    path::Path,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

const REQUESTS: TableDefinition<&str, (&[u8], &[u8])> = TableDefinition::new("requests_v1");
const EXPIRY: TableDefinition<(u64, &str), ()> = TableDefinition::new("expiry_v1");
const JOURNAL_FILE: &str = "admin-transactions.redb";
const JOURNAL_REQUIRED: &str = "admin-transactions.required";
const RETENTION: u64 = 7 * 24 * 60 * 60;
const CAPACITY: u64 = 8192;
pub(super) const INDETERMINATE: &str =
    "Operation outcome indeterminate; inspect current state before issuing a new transaction";

#[derive(Clone)]
pub(crate) struct Journal(Arc<Inner>);
struct Inner {
    db: Database,
    active: Mutex<BTreeSet<String>>,
}
pub(super) struct Ticket {
    journal: Journal,
    key: String,
}
pub(super) enum Claim {
    New(Ticket),
    Completed(Vec<u8>),
    Active,
    Indeterminate,
    Conflict,
    Full,
}

impl Journal {
    pub(crate) fn open(dir: &Path) -> Result<Self> {
        // A new installation or pre-journal store has no marker. Once this store has
        // accepted tagged operations, an incomplete restore must not start a new ledger.
        let marker = dir.join(JOURNAL_REQUIRED);
        let required = marker.try_exists()?;
        let mut opts = File::options();
        opts.read(true)
            .write(true)
            .create(!required)
            .truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let file = crate::store::open_regular(&dir.join(JOURNAL_FILE), &mut opts)
            .context("opening admin transaction journal; restore the complete bridge state")?;
        ensure!(
            !required || file.metadata()?.len() > 0,
            "empty admin transaction journal in restored state"
        );
        let db = Database::builder().create_with_backend(FileBackend::new(file))?;
        if required {
            let read = db.begin_read()?;
            read.open_table(REQUESTS)?;
            read.open_table(EXPIRY)?;
        } else {
            let write = db.begin_write()?;
            write.open_table(REQUESTS)?;
            write.open_table(EXPIRY)?;
            write.commit()?;
            let mut opts = File::options();
            opts.write(true).create_new(true);
            crate::store::open_regular(&marker, &mut opts)?.sync_all()?;
            File::open(dir)?.sync_all()?;
        }
        Ok(Self(Arc::new(Inner {
            db,
            active: Mutex::new(BTreeSet::new()),
        })))
    }
    pub(super) fn claim(&self, key: String, fingerprint: &[u8]) -> Result<Claim> {
        self.claim_at(key, fingerprint, now()?)
    }
    fn claim_at(&self, key: String, fingerprint: &[u8], time: u64) -> Result<Claim> {
        let mut active = self.0.active.lock().unwrap();
        let w = self.0.db.begin_write()?;
        let mut pruned = false;
        let existing = {
            let mut rows = w.open_table(REQUESTS)?;
            let mut expiry = w.open_table(EXPIRY)?;
            loop {
                let expired = expiry.first()?.and_then(|(k, _)| {
                    let (until, key) = k.value();
                    (until <= time).then(|| (until, key.to_owned()))
                });
                let Some((until, old_key)) = expired else {
                    break;
                };
                rows.remove(old_key.as_str())?;
                expiry.remove((until, old_key.as_str()))?;
                pruned = true;
            }
            let existing = rows.get(key.as_str())?.map(|row| {
                let (digest, result) = row.value();
                if digest != fingerprint {
                    Claim::Conflict
                } else if !result.is_empty() {
                    Claim::Completed(result.to_vec())
                } else if active.contains(&key) {
                    Claim::Active
                } else {
                    Claim::Indeterminate
                }
            });
            if existing.is_some() {
                existing
            } else if rows.len()? >= CAPACITY {
                Some(Claim::Full)
            } else {
                rows.insert(key.as_str(), (fingerprint, &[][..]))?;
                None
            }
        };
        if pruned || existing.is_none() {
            w.commit()?;
        }
        if let Some(claim) = existing {
            return Ok(claim);
        }
        active.insert(key.clone());
        Ok(Claim::New(Ticket {
            journal: self.clone(),
            key,
        }))
    }
}
fn now() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}
impl Ticket {
    pub(super) fn finish(self, bytes: &[u8]) -> Result<()> {
        self.store(bytes, Some(now()?.saturating_add(RETENTION)))
    }
    /// Keep a tagged ambiguous effect reserved. Expiring this result would let
    /// a redelivery repeat it when the peer may already have acted.
    pub(super) fn indeterminate(self, bytes: &[u8]) -> Result<()> {
        self.store(bytes, None)
    }
    fn store(self, bytes: &[u8], expires: Option<u64>) -> Result<()> {
        ensure!(!bytes.is_empty(), "empty administration response");
        let w = self.journal.0.db.begin_write()?;
        {
            let mut rows = w.open_table(REQUESTS)?;
            let fingerprint = rows
                .get(self.key.as_str())?
                .ok_or_else(|| anyhow::anyhow!("missing transaction reservation"))?
                .value()
                .0
                .to_owned();
            rows.insert(self.key.as_str(), (fingerprint.as_slice(), bytes))?;
            if let Some(until) = expires {
                w.open_table(EXPIRY)?.insert((until, self.key.as_str()), ())?;
            }
        }
        w.commit()?;
        Ok(())
    }
}
impl Drop for Ticket {
    fn drop(&mut self) {
        self.journal.0.active.lock().unwrap().remove(&self.key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn incomplete_restore_cannot_accept_a_previously_seen_transaction() -> Result<()> {
        let dir = tempfile::tempdir()?;
        drop(Journal::open(dir.path())?);
        std::fs::remove_file(dir.path().join(JOURNAL_FILE))?;
        assert!(Journal::open(dir.path()).is_err());
        assert!(!dir.path().join(JOURNAL_FILE).exists());
        File::create(dir.path().join(JOURNAL_FILE))?;
        assert!(Journal::open(dir.path()).is_err());
        Ok(())
    }
    #[test]
    fn explicit_uncertain_result_survives_restart_and_normal_result_expiry() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let key = "multi-cluster-bind".to_owned();
        let journal = Journal::open(dir.path())?;
        let Claim::New(ticket) = journal.claim_at(key.clone(), b"request", 1)? else { panic!() };
        let partial = b"First cluster confirmed; second cluster outcome unknown";
        ticket.indeterminate(partial)?;
        drop(journal);

        let journal = Journal::open(dir.path())?;
        assert!(matches!(journal.claim_at(key.clone(), b"request", 1 + RETENTION)?,
            Claim::Completed(v) if v == partial));
        assert!(matches!(journal.claim_at(key, b"other request", 2 + RETENTION)?, Claim::Conflict));
        Ok(())
    }
    #[test]
    fn cached_response_and_uncertain_effect_survive_restore() -> Result<()> {
        let src = tempfile::tempdir()?;
        let restored = tempfile::tempdir()?;
        let key = "permit".to_owned();
        let other = "bind".to_owned();
        {
            let db = Journal::open(src.path())?;
            let Claim::New(ticket) = db.claim_at(key.clone(), b"a", 10)? else {
                panic!()
            };
            assert!(matches!(db.claim_at(key.clone(), b"a", 10)?, Claim::Active));
            assert!(matches!(
                db.claim_at(key.clone(), b"b", 10)?,
                Claim::Conflict
            ));
            ticket.store(b"confirmed", Some(11 + RETENTION))?;
            let Claim::New(interrupted) = db.claim_at(other.clone(), b"c", 11)? else {
                panic!()
            };
            drop(interrupted);
        }
        std::fs::copy(
            src.path().join("admin-transactions.redb"),
            restored.path().join("admin-transactions.redb"),
        )?;
        std::fs::copy(
            src.path().join(JOURNAL_REQUIRED),
            restored.path().join(JOURNAL_REQUIRED),
        )?;
        let db = Journal::open(restored.path())?;
        assert!(matches!(db.claim_at(key.clone(),b"a",12)?,Claim::Completed(v) if v==b"confirmed"));
        assert!(matches!(
            db.claim_at(other.clone(), b"c", 12 + RETENTION)?,
            Claim::Indeterminate
        ));
        assert!(matches!(
            db.claim_at(key, b"a", 12 + RETENTION)?,
            Claim::New(_)
        ));
        Ok(())
    }
}
