#![cfg(unix)]

use anyhow::{Result, ensure};
use std::{fs, io, os::unix::fs::symlink, process::Command, time::Duration};
use tempfile::tempdir;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::unix::pipe,
    sync::oneshot,
    time::timeout,
};
use zigbee_bridge_rs::{session::Startup, store::Store};

#[test]
fn store_rejects_fifos_without_waiting_for_a_peer() -> Result<()> {
    let dir = tempdir()?;
    for name in ["devices.json", "bridge.lock"] {
        let path = dir.path().join(name);
        // The first attempted open can create the ordinary sidecar lock.
        if path.try_exists()? {
            fs::remove_file(&path)?;
        }
        ensure!(Command::new("mkfifo").arg(&path).status()?.success());
        let (sender, receiver) = std::sync::mpsc::channel();
        let root = dir.path().to_owned();
        let worker = std::thread::spawn(move || sender.send(Store::open(&root).err()).unwrap());
        let error = receiver
            .recv_timeout(Duration::from_secs(5))?
            .expect("accepted FIFO");
        worker.join().unwrap();
        assert!(format!("{error:#}").contains("expected a regular file"));
        fs::remove_file(path)?;
    }
    Store::open(dir.path())?;
    Ok(())
}

#[test]
fn dangling_snapshot_symlink_cannot_be_mistaken_for_an_empty_store() -> Result<()> {
    let dir = tempdir()?;
    symlink("missing-target", dir.path().join("devices.json"))?;
    assert!(Store::open(dir.path()).is_err());
    Ok(())
}

#[tokio::test]
async fn shutdown_releases_a_backpressured_pipe_with_accepted_bytes_intact() -> Result<()> {
    let dir = tempdir()?;
    let mut startup = Startup::new(Store::open(dir.path())?);
    let (mut writer, mut reader) = pipe::pipe()?;
    let (full, waiting) = oneshot::channel();
    startup.worker(move |session| async move {
        writer.writable().await?;
        let chunk = [0x55; 4096];
        let mut accepted = 0;
        loop {
            match writer.try_write(&chunk) {
                Ok(n) => { ensure!(n > 0); accepted += n; }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e.into()),
            }
        }
        ensure!(accepted > 0, "pipe was never filled");
        full.send(accepted).unwrap();
        tokio::select! {
            _ = session.cancelled() => Ok(()),
            result = writer.write(&chunk) => anyhow::bail!("unexpected write before reader drained: {result:?}"),
        }
    });
    let controller = startup.start();
    let accepted = timeout(Duration::from_secs(5), waiting).await??;
    timeout(Duration::from_secs(5), controller.shutdown()).await??;
    // All writers have been reclaimed. The reader can drain and observe EOF
    // without having to run concurrently with shutdown to make space.
    let mut received = Vec::new();
    timeout(Duration::from_secs(5), reader.read_to_end(&mut received)).await??;
    assert_eq!(received, vec![0x55; accepted]);
    Store::open(dir.path())?;
    Ok(())
}

#[test]
fn successor_reclaims_partial_snapshot_after_acquiring_the_lock() -> Result<()> {
    let parent = tempdir()?;
    let dir = parent.path().join("new/bridge");
    let mut owner = Store::open(&dir)?;
    owner.update(|_| Ok(()))?;
    let committed = fs::read(dir.join("devices.json"))?;
    let staging = dir.join("devices.json.pending");
    fs::write(&staging, b"partial")?;
    assert!(Store::open(&dir).is_err());
    assert!(staging.exists());
    drop(owner);
    let mut successor = Store::open(&dir)?;
    assert!(!staging.try_exists()?);
    assert_eq!(fs::read(dir.join("devices.json"))?, committed);
    successor.update(|_| Ok(()))?;
    Ok(())
}
