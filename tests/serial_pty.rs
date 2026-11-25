//! Linux returns EIO at the PTY master after the final slave closes and its
//! queued bytes have been read. This gives the test an observable end marker.
#![cfg(target_os = "linux")]

use anyhow::{Context, Result, ensure};
use std::{io, path::Path, time::Duration};
use tempfile::tempdir;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    time::timeout,
};
use tokio_serial::{SerialPort, SerialStream};
use zigbee_bridge_rs::{
    serial::{Serial, WriteOutcome},
    session::{Controller, Startup},
    store::Store,
};

fn start(path: &str, dir: &Path) -> Result<(Serial, Controller)> {
    let mut startup = Startup::new(Store::open(dir)?);
    let port = Serial::attach(&mut startup, tokio_serial::new(path, 115200))?;
    Ok((port, startup.start()))
}

async fn read_to_hangup(peer: &mut SerialStream, received: &mut Vec<u8>) -> io::Result<()> {
    match peer.read_to_end(received).await {
        Ok(_) => Ok(()),
        Err(e) if e.raw_os_error() == Some(libc::EIO) => Ok(()),
        Err(e) => Err(e),
    }
}

// A counter in every word makes duplicated or skipped prefixes detectable.
fn pattern(n: u32) -> Vec<u8> {
    (0..n).flat_map(u32::to_le_bytes).collect()
}

#[tokio::test]
async fn stop_at_midframe_then_reopen_without_replaying() -> Result<()> {
    timeout(Duration::from_secs(5), async {
        let dir = tempdir()?;
        let (mut peer, initial_slave) = SerialStream::pair()?;
        let path = initial_slave.name().context("PTY slave pathname")?;
        drop(initial_slave);
        let (mut serial, controller) = start(&path, dir.path())?;
        ensure!(
            SerialStream::open(&tokio_serial::new(&path, 115200)).is_err(),
            "tty not exclusive"
        );

        let frame = pattern(65536);
        let writing = serial.try_send(frame.clone())?;
        let queued = serial.try_send(b"queued behind the partial frame".to_vec())?;
        let halfway = frame.len() / 2;
        let mut received = vec![0; halfway];
        peer.read_exact(&mut received).await?;
        // Deliberately stop draining TX. Prove the worker can still deliver RX.
        peer.write_all(b"peer ready").await?;
        let mut response = Vec::new();
        while response.len() < 10 {
            response.extend(
                serial
                    .incoming
                    .recv()
                    .await
                    .context("serial reader closed")?,
            );
        }
        assert_eq!(response, b"peer ready");
        controller.shutdown().await?;
        let WriteOutcome::Partial { accepted } = writing.await? else {
            anyhow::bail!("expected a partial local write");
        };
        assert!((halfway..frame.len()).contains(&accepted));
        assert_eq!(queued.await?, WriteOutcome::Unsent);
        assert_eq!(
            serial.try_send(vec![1]).unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );

        read_to_hangup(&mut peer, &mut received).await?;
        assert_eq!(received.len(), accepted);
        assert!(
            received == frame[..accepted],
            "observed stream differs from frame prefix"
        );

        // Keep the old public handles alive during handover. Only the successor
        // can own the tty and emit the independently identifiable marker.
        let (next, successor) = start(&path, dir.path())?;
        let marker = b"new owner";
        assert_eq!(
            next.try_send(marker.to_vec())?.await?,
            WriteOutcome::Accepted
        );
        successor.shutdown().await?;
        let mut fresh = Vec::new();
        read_to_hangup(&mut peer, &mut fresh).await?;
        assert_eq!(fresh, marker);
        println!(
            "frame={} stop_observed={} accepted={} peer_total={} successor_bytes={}",
            frame.len(),
            halfway,
            accepted,
            received.len(),
            fresh.len()
        );
        anyhow::Ok(())
    })
    .await
    .context("PTY handover timed out")?
}

#[tokio::test]
async fn entire_frame_crosses_multiple_writes_once() -> Result<()> {
    timeout(Duration::from_secs(5), async {
        let dir = tempdir()?;
        let (mut peer, slave) = SerialStream::pair()?;
        let path = slave.name().context("PTY slave pathname")?;
        drop(slave);
        let (serial, controller) = start(&path, dir.path())?;
        let frame = pattern(65536);
        let receipt = serial.try_send(frame.clone())?;
        let mut data = vec![0; frame.len()];
        peer.read_exact(&mut data).await?;
        assert_eq!(receipt.await?, WriteOutcome::Accepted);
        controller.shutdown().await?;
        read_to_hangup(&mut peer, &mut data).await?;
        assert!(data == frame, "complete frame mismatch");
        anyhow::Ok(())
    })
    .await
    .context("uninterrupted frame timed out")?
}

#[tokio::test]
async fn peer_disconnect_resolves_partial_and_queued_writes() -> Result<()> {
    timeout(Duration::from_secs(5), async {
        let dir = tempdir()?;
        let (mut peer, slave) = SerialStream::pair()?;
        let path = slave.name().context("PTY slave pathname")?;
        drop(slave);
        let (serial, controller) = start(&path, dir.path())?;
        let frame = pattern(65536);
        let writing = serial.try_send(frame.clone())?;
        let queued = serial.try_send(vec![0xf6])?;
        let mut seen = [0; 1024];
        peer.read_exact(&mut seen).await?;
        assert_eq!(seen, frame[..seen.len()]);
        drop(peer);
        assert!(controller.finished().await.is_err());
        let WriteOutcome::Partial { accepted } = writing.await? else {
            anyhow::bail!("lost partial write count on hangup");
        };
        assert!((seen.len()..frame.len()).contains(&accepted));
        assert_eq!(queued.await?, WriteOutcome::Unsent);
        Store::open(dir.path())?;
        anyhow::Ok(())
    })
    .await
    .context("peer disconnect timed out")?
}

#[tokio::test]
async fn abandoned_startup_releases_port_and_resolves_admitted_frames() -> Result<()> {
    let dir = tempdir()?;
    let (_peer, slave) = SerialStream::pair()?;
    let path = slave.name().context("PTY slave pathname")?;
    drop(slave);
    let mut startup = Startup::new(Store::open(dir.path())?);
    let settings = tokio_serial::new(&path, 115200);
    let serial = Serial::attach(&mut startup, settings.clone())?;
    let queued = serial.try_send(vec![1])?;
    drop(startup);
    assert_eq!(queued.await?, WriteOutcome::Unsent);
    assert_eq!(
        serial.try_send(vec![2]).unwrap_err().kind(),
        io::ErrorKind::BrokenPipe
    );
    SerialStream::open(&settings)?;
    Ok(())
}
