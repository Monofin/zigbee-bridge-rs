use crate::registry::{Device, Registry};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions, TryLockError},
    io::{self, BufReader, BufWriter, Write},
    path::{Path, PathBuf},
};

// Reserved inside a bridge-owned state directory. Only the lock holder uses it.
const PENDING: &str = "devices.json.pending";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    version: u8,
    devices: Vec<Device>,
}

// Open first and check the returned handle. A pathname check before open
// would race replacement and a blocking FIFO open could wait for a peer.
pub(crate) fn open_regular(path: &Path, options: &mut OpenOptions) -> io::Result<File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_NOCTTY);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected a regular file",
        ));
    }
    Ok(file)
}

// Sync the parent of each new directory before creating its children.
fn create_state_directory(path: &Path) -> io::Result<()> {
    if path.try_exists()? {
        return Ok(());
    }
    let parent = path.parent().ok_or(io::ErrorKind::NotFound)?;
    create_state_directory(parent)?;
    match fs::create_dir(path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists && path.is_dir() => {}
        Err(e) => return Err(e),
    }
    File::open(parent)?.sync_all()
}

fn decode_registry(file:File) -> Result<Registry> {
    let s: Snapshot = serde_json::from_reader(BufReader::new(file))?;
    ensure!(s.version==1,"unsupported device store version");
    Registry::from_snapshot(s.devices)
}

/// Only the process that acquired this lock may release it. A forked child
/// shares the underlying open file description until it closes the file or execs.
struct StoreLock {
    file: File,
    owner: u32,
}

impl StoreLock {
    fn acquired(file: File) -> Self {
        Self { file, owner: std::process::id() }
    }

    fn owned_here(&self) -> bool {
        self.owner == std::process::id()
    }
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        if self.owned_here() {
            // Closing alone leaves flock held if an unrelated child still has
            // its inherited descriptor. A forked copy must only close its fd.
            while let Err(error) = self.file.unlock() {
                if error.kind() != io::ErrorKind::Interrupted {
                    break;
                }
            }
        }
    }
}

/// All writes finish synchronously under the directory lock. There is no
/// background writer that can outlive the process holding the store.
pub struct Store {
    root: PathBuf,
    lock: StoreLock,
    registry: Registry,
    writable: bool,
}
impl Store {
    /// Hold a shared lock across an offline read of names and member records.
    /// Inspection cannot create a state directory or perform crash recovery.
    pub fn inspect<T>(root:&Path, read:impl FnOnce(&Registry)->Result<T>)->Result<T> {
        let lock=open_regular(&root.join("bridge.lock"),File::options().read(true))?;
        lock.try_lock_shared().context("staged state is in use")?;
        let _guard = StoreLock::acquired(lock);
        let registry = match open_regular(&root.join("devices.json"), File::options().read(true)) {
            Ok(file) => decode_registry(file)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => Registry::default(),
            Err(e) => return Err(e.into()),
        };
        read(&registry)
    }

    pub fn open(root: &Path) -> Result<Self> {
        let root = std::path::absolute(root)?;
        create_state_directory(&root)?;
        let root = fs::canonicalize(root)?;
        let lock = open_regular(
            &root.join("bridge.lock"),
            File::options()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true),
        )
        .context("opening bridge.lock")?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => anyhow::bail!("another bridge owns this directory"),
            Err(TryLockError::Error(err)) => return Err(err).context("locking bridge.lock"),
        }
        let lock = StoreLock::acquired(lock);
        let registry = match open_regular(&root.join("devices.json"), File::options().read(true)) {
            Ok(file) => decode_registry(file)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Registry::default(),
            Err(e) => return Err(e).context("opening devices.json"),
        };
        // A crash before rename can leave an incomplete staging file. Only
        // the committed snapshot above is authoritative. Reclaim staging
        // under the lock, after validating that snapshot.
        match fs::remove_file(root.join(PENDING)) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).context("reclaiming pending snapshot"),
        }
        File::open(&root)?
            .sync_all()
            .context("confirming recovered directory state")?;
        Ok(Self {
            root,
            lock,
            registry,
            writable: true,
        })
    }
    pub fn begin_interview(&self) -> Result<crate::interview_log::InterviewLog> {
        ensure!(self.lock.owned_here(), "store was acquired by another process");
        crate::interview_log::InterviewLog::create(&self.root)
    }
    pub fn registry(&self) -> &Registry {
        &self.registry
    }
    pub fn update<T>(&mut self, edit: impl FnOnce(&mut Registry) -> Result<T>) -> Result<T> {
        ensure!(self.lock.owned_here(), "store was acquired by another process");
        ensure!(
            self.writable,
            "snapshot durability uncertain; reopen the store"
        );
        let mut next = self.registry.staged();
        let value = edit(&mut next)?;
        let mut temp = tempfile::Builder::new()
            .prefix(PENDING)
            .rand_bytes(0)
            .tempfile_in(&self.root)?;
        let mut writer = BufWriter::new(&mut temp);
        serde_json::to_writer(
            &mut writer,
            &Snapshot {
                version: 1,
                devices: next.devices().cloned().collect(),
            },
        )?;
        writer.flush().context("flushing snapshot contents")?;
        drop(writer);
        temp.as_file().sync_all()?;
        temp.persist(self.root.join("devices.json"))?;
        self.registry = next;
        self.writable = false;
        File::open(&self.root)
            .and_then(|dir| dir.sync_all())
            .context("snapshot replaced but directory sync failed; durability is uncertain")?;
        self.writable = true;
        Ok(value)
    }
}

#[cfg(test)]
mod lock_tests {
    use super::*;

    #[test]
    fn dropping_an_inherited_copy_cannot_unlock_its_owner() -> Result<()> {
        let temp = tempfile::NamedTempFile::new()?;
        let file = temp.reopen()?;
        file.try_lock()?;
        let owner = StoreLock::acquired(file);
        // dup and fork refer to the same open file description. Make only the
        // copied guard foreign to this process without running Rust after fork.
        let copy = StoreLock {
            file: owner.file.try_clone()?,
            owner: owner.owner.wrapping_add(1),
        };
        drop(copy);
        let contender = temp.reopen()?;
        assert!(matches!(contender.try_lock(), Err(TryLockError::WouldBlock)));
        drop(owner);
        contender.try_lock()?;
        Ok(())
    }
}
