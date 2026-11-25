use super::*;
use crate::{
    session::{Controller, Startup},
    store::Store,
};
use anyhow::{Context, Result};
use std::path::Path;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_serial::{SerialPort, SerialStream};

fn start(path: &str, dir: &Path) -> Result<(Serial, Controller)> {
    let mut startup = Startup::new(Store::open(dir)?);
    let serial = Serial::attach(&mut startup, tokio_serial::new(path, 115200))?;
    Ok((serial, startup.start()))
}

async fn receive(peer: &mut SerialStream) -> Result<Frame> {
    let mut decoder = Decoder::default();
    loop {
        if let Some(result) = decoder.push(peer.read_u8().await?) {
            return result.map_err(|()| anyhow::anyhow!("invalid frame from host"));
        }
    }
}

async fn send(peer: &mut SerialStream, control: u8, data: &[u8]) -> Result<()> {
    peer.write_all(&Frame::encode(control, data)).await?;
    Ok(())
}

async fn handshake(peer: &mut SerialStream, sequence: u8) -> Result<()> {
    assert_eq!(receive(peer).await?.control, 0xc0);
    send(peer, 0xc1, &[2, 2]).await?; // an unrelated power-on reset
    send(peer, 1, &[sequence, 0x80, 0, 14, 2, 0, 1]).await?; // buffered old reply
    send(peer, 0xc1, &[2, 0x0b]).await?;
    assert_eq!(receive(peer).await?.control, 0x80);
    assert_eq!(
        receive(peer).await?,
        Frame {
            control: 0,
            data: vec![sequence, 0, 0, 16]
        }
    );
    Ok(())
}

#[tokio::test]
async fn reset_after_ack_retires_request_and_releases_port() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(5), async {
        let dir = tempfile::tempdir()?;
        let (mut peer, slave) = SerialStream::pair()?;
        let path = slave.name().context("PTY slave name")?;
        drop(slave);
        let (mut port, controller) = start(&path, dir.path())?;
        let host = probe_version(&mut port, 16, 5, Duration::from_secs(3));
        let ncp = async {
            handshake(&mut peer, 5).await?;
            send(&mut peer, 0x81, &[]).await?;
            send(&mut peer, 0xc1, &[2, 3]).await?;
            send(&mut peer, 1, &[5, 0x80, 0, 14, 2, 0, 1]).await?;
            anyhow::Ok(())
        };
        let (result, simulated) = tokio::join!(host, ncp);
        simulated?;
        let error = result.unwrap_err();
        assert_eq!(
            error.delivery,
            Delivery::OutcomeUnknown {
                link_acknowledged: true
            }
        );
        assert!(matches!(error.cause, Failure::PeerReset(3)));
        controller.shutdown().await?;

        let (mut fresh, successor) = start(&path, dir.path())?;
        let next = probe_version(&mut fresh, 16, 6, Duration::from_secs(3));
        let ncp = async {
            // The successor starts with RST. Replaying the previous request,
            // or sending an ACK for its trailing reply, fails this check.
            handshake(&mut peer, 6).await?;
            send(&mut peer, 0xa0, &[]).await?;
            assert_eq!(
                receive(&mut peer).await?,
                Frame {
                    control: 8,
                    data: vec![6, 0, 0, 16]
                }
            );
            // A callback, its duplicate, and an older sequence cannot resolve VERSION.
            send(&mut peer, 1, &[6, 0x90, 0, 14, 2, 0, 1]).await?;
            assert_eq!(receive(&mut peer).await?.control, 0x81);
            send(&mut peer, 9, &[6, 0x90, 0, 14, 2, 0, 1]).await?;
            assert_eq!(receive(&mut peer).await?.control, 0x81);
            send(&mut peer, 0x11, &[5, 0x80, 0, 14, 2, 0, 1]).await?;
            assert_eq!(receive(&mut peer).await?.control, 0x82);
            send(&mut peer, 0x21, &[6, 0x80, 0, 16, 2, 0x34, 0x12]).await?;
            assert_eq!(receive(&mut peer).await?.control, 0x83);
            anyhow::Ok(())
        };
        let (version, simulated) = tokio::join!(next, ncp);
        simulated?;
        assert_eq!(
            version?,
            Version {
                protocol: 16,
                stack_type: 2,
                stack_version: 0x1234
            }
        );
        successor.shutdown().await?;
        anyhow::Ok(())
    })
    .await
    .context("simulated NCP stalled")?
}

#[test]
fn version_correlation_requires_the_intact_synchronous_response() {
    for control in [0, 0x88, 0x90, 0xa0] {
        assert_eq!(
            version_reply(3, &[3, control, 0, 16, 2, 0, 1]).unwrap(),
            None
        );
    }
    assert!(matches!(
        version_reply(3, &[3, 0x82, 0, 16, 2, 0, 1]),
        Err(Failure::InvalidVersionResponse)
    ));
    assert!(matches!(
        version_reply(3, &[3, 0x80, 0, 16]),
        Err(Failure::InvalidVersionResponse)
    ));
}

#[test]
fn rejection_is_latched_until_in_sequence_data_and_ack_numbers_wrap_only_on_receive() {
    let mut window = ReceiveWindow::default();
    assert!(matches!(window.receive(Err(())).unwrap(), Action::Nak));
    assert!(matches!(window.receive(Err(())).unwrap(), Action::Ignore));
    let frame = |control| {
        Ok(Frame {
            control,
            data: vec![0, 0, 0],
        })
    };
    assert!(matches!(
        window.receive(frame(0x20)).unwrap(),
        Action::Ignore
    ));
    for n in 0..8 {
        assert!(matches!(
            window.receive(frame(n << 4 | 1)).unwrap(),
            Action::Deliver(_)
        ));
    }
    assert_eq!(window.next, 0);
    assert!(window.acknowledged);
    assert!(matches!(window.receive(frame(0x79)).unwrap(), Action::Ack));
    assert!(matches!(window.receive(frame(0x80)).unwrap(), Action::Nak)); // stale ackNum
    assert!(matches!(
        window.receive(frame(0xa1)).unwrap(),
        Action::Ignore
    )); // already acked
}

#[tokio::test]
async fn v13_handles_interleaved_callbacks_wrap_and_session_retirement() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(5), async {
        let dir = tempfile::tempdir()?;
        let (mut peer, slave) = SerialStream::pair()?;
        let path = slave.name().context("PTY slave name")?;
        drop(slave);
        let (mut port, controller) = start(&path, dir.path())?;
        let (prefix_sent, prefix_ready) = tokio::sync::oneshot::channel();
        let (expired, finish_frame) = tokio::sync::oneshot::channel();
        let ncp = async {
            assert_eq!(receive(&mut peer).await?.control, 0xc0);
            send(&mut peer, 0xc1, &[2, 0x0b]).await?;
            assert_eq!(receive(&mut peer).await?.control, 0x80);
            assert_eq!(receive(&mut peer).await?.data, [0, 0, 0, 13]);
            send(&mut peer, 1, &[0, 0x80, 0, 13, 2, 0x50, 0x74]).await?;
            assert_eq!(receive(&mut peer).await?.control, 0x81);
            let mut rx = 1;
            for seq in 1..=10u8 {
                let req = receive(&mut peer).await?;
                assert_eq!(req.control, ((seq & 7) << 4) | rx);
                assert_eq!(req.data, [seq, 0, 1, 0x18, 0]);
                let ack = (seq + 1) & 7;
                if seq == 2 {
                    send(&mut peer, (rx << 4) | ack, &[0, 0x90, 1, 0x19, 0, 0x90]).await?;
                    rx = (rx + 1) & 7;
                    assert_eq!(receive(&mut peer).await?.control, 0x80 | rx);
                }
                send(&mut peer, (rx << 4) | ack, &[seq, 0x80, 1, 0x18, 0, 2]).await?;
                rx = (rx + 1) & 7;
                assert_eq!(receive(&mut peer).await?.control, 0x80 | rx);
            }
            let frame = Frame::encode((rx << 4) | 3, &[0, 0x90, 1, 0x19, 0, 0x9d]);
            peer.write_all(&frame[..4]).await?;
            prefix_sent.send(()).unwrap();
            finish_frame.await?;
            peer.write_all(&frame[4..]).await?;
            assert_eq!(receive(&mut peer).await?.control, 0x80 | ((rx + 1) & 7));
            let _ = receive(&mut peer).await?;
            send(&mut peer, 0x84, &[]).await?;
            send(&mut peer, 0xc1, &[2, 3]).await?;
            anyhow::Ok(())
        };
        let host = async {
            let wait = Duration::from_secs(2);
            let mut link = Ezsp::connect(&mut port, wait).await?;
            for _ in 0..10 {
                assert_eq!(link.command(0x18, &[], wait).await?, [2]);
            }
            let queued = link.next_callback().await.unwrap();
            assert_eq!((queued.id, queued.payload), (0x19, vec![0x90]));
            prefix_ready.await?;
            let deadline = Instant::now() + Duration::from_millis(10);
            assert!(link.callback_until(deadline).await.unwrap().is_none());
            expired.send(()).unwrap();
            let completed = link.next_callback().await.unwrap();
            assert_eq!((completed.id, completed.payload), (0x19, vec![0x9d]));
            let err = link.command(0x18, &[], wait).await.unwrap_err();
            assert!(matches!(err.cause, Failure::PeerReset(3)));
            assert_eq!(
                err.delivery,
                Delivery::OutcomeUnknown {
                    link_acknowledged: true
                }
            );
            assert!(matches!(
                link.command(0x18, &[], wait).await.unwrap_err().cause,
                Failure::InvalidSession
            ));
            anyhow::Ok(())
        };
        let (n, h) = tokio::join!(ncp, host);
        n?;
        h?;
        controller.shutdown().await?;
        anyhow::Ok(())
    })
    .await
    .context("persistent session stalled")?
}

#[tokio::test]
async fn v19_requires_a_matching_second_version_before_issuing_runtime_commands() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (mut peer, slave) = SerialStream::pair()?;
    let path = slave.name().context("PTY slave name")?;
    drop(slave);
    let (mut port, owner) = start(&path, dir.path())?;
    let limit = Duration::from_secs(4);
    let ncp = async {
        assert_eq!(receive(&mut peer).await?.control, 0xc0);
        send(&mut peer, 0xc1, &[2, 0x0b]).await?;
        assert_eq!(receive(&mut peer).await?.control, 0x80);
        assert_eq!(receive(&mut peer).await?.data, [0, 0, 0, 13]);
        send(&mut peer, 1, &[0, 0x80, 0, 19, 2, 0x20, 0x91]).await?;
        assert_eq!(receive(&mut peer).await?.control, 0x81);
        assert_eq!(receive(&mut peer).await?.data, [1, 0, 1, 0, 0, 19]);
        // The second VERSION reply disagrees with the first.
        send(&mut peer, 0x12, &[1, 0x80, 1, 0, 0, 18, 2, 0x20, 0x91]).await?;
        assert_eq!(receive(&mut peer).await?.control, 0x82);
        anyhow::Ok(())
    };
    let host = async {
        let result = Ezsp::connect(&mut port, limit).await;
        assert!(matches!(
            result,
            Err(ProbeError {
                cause: Failure::InvalidVersionResponse,
                ..
            })
        ));
        anyhow::Ok(())
    };
    let (n, h) = tokio::time::timeout(limit, async { tokio::join!(ncp, host) }).await?;
    n?;
    h?;
    owner.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn backpressure_pauses_callbacks_while_a_command_reply_can_still_arrive() -> Result<()> {
    timeout(Duration::from_secs(6), async {
        let (mut peer, slave) = SerialStream::pair()?;
        let path = slave.name().context("PTY name")?;
        drop(slave);
        let dir = tempfile::tempdir()?;
        let (mut serial, owner) = start(&path, dir.path())?;
        let host = async {
            let mut ezsp = Ezsp::connect(&mut serial, Duration::from_secs(3)).await?;
            let result = ezsp.command(0x26, &[], Duration::from_secs(3)).await?;
            assert_eq!(result, [0; 8]);
            for id in 0..PAUSE_AT as u8 {
                let cb = ezsp
                    .next_raw_callback()
                    .await
                    .map_err(|e| anyhow::anyhow!("{e:?}"))?;
                assert_eq!((cb.id, cb.payload.as_slice()), (0x45, &[id][..]));
            }
            anyhow::Ok(())
        };
        let ncp = async {
            assert_eq!(receive(&mut peer).await?.control, 0xc0);
            send(&mut peer, 0xc1, &[2, 0x0b]).await?;
            assert_eq!(receive(&mut peer).await?.control, 0x80);
            let version = receive(&mut peer).await?;
            send(&mut peer, 1, &[version.data[0], 0x80, 0, 13, 2, 0, 0]).await?;
            assert_eq!(receive(&mut peer).await?.control, 0x81);
            let request = receive(&mut peer).await?;
            let mut tx = 1;
            for id in 0..PAUSE_AT as u8 {
                send(&mut peer, (tx << 4) | 2, &[0, 0x90, 1, 0x45, 0, id]).await?;
                tx = (tx + 1) & 7;
                let flag = if usize::from(id) + 1 == PAUSE_AT {
                    8
                } else {
                    0
                };
                assert_eq!(receive(&mut peer).await?.control, 0x80 | flag | tx);
            }
            // The link stays alive while the NCP has normal callbacks paused.
            assert_eq!(receive(&mut peer).await?.control, 0x88 | tx);
            let mut body = vec![request.data[0], 0x80, 1, 0x26, 0];
            body.extend([0; 8]);
            send(&mut peer, (tx << 4) | 2, &body).await?;
            tx = (tx + 1) & 7;
            assert_eq!(receive(&mut peer).await?.control, 0x88 | tx);
            assert_eq!(receive(&mut peer).await?.control, 0x80 | tx);
            anyhow::Ok(())
        };
        tokio::try_join!(host, ncp)?;
        owner.shutdown().await?;
        anyhow::Ok(())
    })
    .await
    .context("flow-control exchange stalled")?
}

#[tokio::test]
async fn conflict_during_lookup_fences_only_that_destination_before_serial_handoff() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(5), async {
        let dir = tempfile::tempdir()?;
        let (mut peer, slave) = SerialStream::pair()?;
        let path = slave.name().context("PTY slave name")?;
        drop(slave);
        let (mut port, controller) = start(&path, dir.path())?;

        let ncp = async {
            assert_eq!(receive(&mut peer).await?.control, 0xc0);
            send(&mut peer, 0xc1, &[2, 0x0b]).await?;
            assert_eq!(receive(&mut peer).await?.control, 0x80);
            assert_eq!(receive(&mut peer).await?.data, [0, 0, 0, 13]);
            send(&mut peer, 1, &[0, 0x80, 0, 13, 2, 0x50, 0x74]).await?;
            assert_eq!(receive(&mut peer).await?.control, 0x81);

            let lookup = receive(&mut peer).await?;
            assert_eq!(&lookup.data[..5], &[1, 0, 1, 0x60, 0]);
            send(&mut peer, 0x12, &[0, 0x90, 1, 0x7c, 0, 1, 2]).await?;
            assert_eq!(receive(&mut peer).await?.control, 0x82);
            send(&mut peer, 0x22, &[1, 0x80, 1, 0x60, 0, 1, 2]).await?;
            assert_eq!(receive(&mut peer).await?.control, 0x83);

            let send_request = receive(&mut peer).await?;
            // The very next serial command must address the unaffected device.
            assert_eq!(&send_request.data[..8], &[2, 0, 1, 0x34, 0, 0, 2, 2]);
            send(&mut peer, 0x33, &[2, 0x80, 1, 0x34, 0, 0, 7]).await?;
            assert_eq!(receive(&mut peer).await?.control, 0x84);
            anyhow::Ok(())
        };

        let host = async {
            let wait = Duration::from_secs(2);
            let mut link = Ezsp::connect(&mut port, wait).await?;
            link.manage_addresses(crate::bridge::addresses::Authority::default());
            assert!(link.adapter_mapping(11, 0x0201));
            assert!(link.adapter_mapping(22, 0x0202));
            let retired = link.destination(11, 0x0201)?;
            let other = link.destination(22, 0x0202)?;
            let raw = link.command(0x34, &[], wait).await.unwrap_err();
            assert_eq!(raw.delivery, Delivery::Unsent);
            assert_eq!(link.command(0x60, &11u64.to_le_bytes(), wait).await?, [1, 2]);
            let mut args = [0, 1, 2, 4, 1, 6, 0, 1, 1, 0, 0, 0, 0, 0, 9, 3, 1, 9, 1];
            assert!(link.submit_to(&retired, &args).await.unwrap_err().is::<super::destination::DestinationRetired>());
            args[1] = 2;
            assert_eq!(link.submit_to(&other, &args).await?, [0, 7]);
            let conflict = link.next_callback().await.map_err(|e| anyhow::anyhow!("callback: {e:?}"))?;
            assert_eq!((conflict.id, conflict.payload), (0x7c, vec![1, 2]));
            anyhow::Ok(())
        };
        tokio::try_join!(host, ncp)?;
        controller.shutdown().await?;
        anyhow::Ok(())
    })
    .await
    .context("conflict or unrelated serial command stalled")?
}
