use anyhow::{Result, bail};
use tempfile::tempdir;
use zigbee_bridge_rs::{
    registry::{Device, Registry},
    store::Store,
};

fn device(ieee: u64, short: u16) -> Device {
    Device {
        ieee,
        name: format!("fixture-{ieee}"),
        short: Some(short),
        temperature_force_update: None,
        vibration_timeout: None,
        endpoint: 1,
    }
}

#[test]
fn delayed_reply_after_same_address_rejoin_cannot_resolve() -> Result<()> {
    let mut r = Registry::default();
    let old = r.admit(device(1, 42))?;
    r.announce(1, 42)?;
    assert_eq!(r.resolve(&old).unwrap().1, 42);
    r.remove(1);
    let current = r.admit(device(1, 42))?;
    assert!(r.left(&old, 42).is_none());
    assert!(r.resolve(&old).is_none());
    assert_eq!(r.resolve(&current).unwrap().1, 42);
    Ok(())
}

#[test]
fn displaced_device_and_delayed_leave_cannot_erase_the_new_owner() -> Result<()> {
    let mut r = Registry::default();
    let first = r.admit(device(1, 42))?;
    let second = r.admit(device(2, 42))?;
    assert!(r.resolve(&first).is_none());
    let moved = r.announce(1, 43)?;
    assert!(r.left(&first, 42).is_none());
    assert_eq!(r.resolve(&moved).unwrap().1, 43);
    r.remove(1);
    assert_eq!(r.resolve(&second).unwrap().1, 42);
    Ok(())
}

#[test]
fn restart_restores_only_committed_devices_and_issues_fresh_handles() -> Result<()> {
    let dir = tempdir()?;
    let mut s = Store::open(dir.path())?;
    let old = s.update(|r| r.admit(device(1, 42)))?;
    let failed: Result<()> = s.update(|r| {
        r.remove(1);
        bail!("discard edit")
    });
    assert!(failed.is_err());
    assert!(s.registry().resolve(&old).is_some());
    drop(s);
    let s = Store::open(dir.path())?;
    let current = s.registry().membership(1).unwrap();
    assert!(s.registry().resolve(&old).is_none());
    assert_eq!(s.registry().resolve(&current).unwrap().1, 42);
    Ok(())
}

#[test]
fn invalid_store_does_not_start_or_hold_the_lock() -> Result<()> {
    let dir = tempdir()?;
    let path = dir.path().join("devices.json");
    std::fs::write(&path, r#"{"version":2,"devices":[]}"#)?;
    assert!(Store::open(dir.path()).is_err());
    assert_eq!(
        std::fs::read_to_string(&path)?,
        r#"{"version":2,"devices":[]}"#
    );
    std::fs::write(path, r#"{"version":1,"devices":[]}"#)?;
    Store::open(dir.path())?;
    Ok(())
}

#[test]
fn process_exit_without_destructors_releases_only_the_local_lock() -> Result<()> {
    use std::{
        env,
        path::Path,
        process::{self, Command},
    };
    const CHILD_DIR: &str = "ZIGBEE_BRIDGE_HANDOVER_TEST_DIR";
    const CHILD_ACTION: &str = "ZIGBEE_BRIDGE_HANDOVER_TEST_ACTION";
    if let Some(dir) = env::var_os(CHILD_DIR) {
        match env::var(CHILD_ACTION)?.as_str() {
            "contend" => {
                assert!(Store::open(Path::new(&dir)).is_err());
                return Ok(());
            }
            "commit-and-exit" => {
                let mut store = Store::open(Path::new(&dir))?;
                store.update(|r| r.admit(device(2, 43)))?;
                // Terminate with the lock still held; no Rust destructor runs.
                process::exit(75);
            }
            _ => bail!("unexpected child mode"),
        }
    }
    let dir = tempdir()?;
    let child = |mode: &str| -> Result<_> {
        Ok(Command::new(env::current_exe()?)
            .args([
                "--exact",
                "process_exit_without_destructors_releases_only_the_local_lock",
            ])
            .env(CHILD_DIR, dir.path())
            .env(CHILD_ACTION, mode)
            .output()?)
    };
    let mut first = Store::open(dir.path())?;
    first.update(|r| r.admit(device(1, 42)))?;
    assert!(child("contend")?.status.success());
    drop(first);
    assert_eq!(child("commit-and-exit")?.status.code(), Some(75));
    let successor = Store::open(dir.path())?;
    assert_eq!(successor.registry().devices().count(), 2);
    Ok(())
}

/// Keep a child between fork and exec while its parent hands the store over.
/// A forked child inherits the open file description even with FD_CLOEXEC.
#[cfg(unix)]
#[test]
fn owner_can_hand_over_while_child_still_holds_an_inherited_descriptor() -> Result<()> {
    use std::os::fd::AsRawFd;
    let directory = tempdir()?;
    let owner = Store::open(directory.path())?;
    let (hold, release) = std::io::pipe()?;
    let read_fd = hold.as_raw_fd();
    let write_fd = release.as_raw_fd();
    // SAFETY: the child only calls async-signal-safe functions and _exit.
    // The parent closes the last writer after it attempts the handover.
    let child = unsafe { libc::fork() };
    if child == -1 {
        return Err(std::io::Error::last_os_error().into());
    }
    if child == 0 {
        unsafe {
            libc::close(write_fd);
            let mut byte = 0_u8;
            libc::read(read_fd, (&raw mut byte).cast(), 1);
            libc::_exit(0);
        }
    }
    drop(hold);
    drop(owner);
    let successor = Store::open(directory.path());
    drop(release);
    let mut status = 0;
    loop {
        // SAFETY: wait for exactly our child and write to a live status slot.
        if unsafe { libc::waitpid(child, &mut status, 0) } == child { break; }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted { return Err(error.into()); }
    }
    assert!(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0);
    successor?;
    Ok(())
}
