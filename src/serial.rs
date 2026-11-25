//! Supervised Unix serial transport. The protocol layer supplies complete
//! encoded frames and consumes an ordered stream of received bytes.
use crate::session::{Session, Startup};
use anyhow::{Context, Result, bail};
use std::{future::poll_fn, io, pin::Pin, task::Poll};
use tokio::{
    io::{AsyncReadExt, AsyncWrite, ReadHalf, WriteHalf, split},
    sync::{mpsc, oneshot},
};
use tokio_serial::{SerialPortBuilder, SerialStream};

/// Counts refer to acceptance by the local tty driver. Peer delivery requires
/// a protocol acknowledgement, even for a completely accepted frame.
#[derive(Debug, PartialEq, Eq)]
pub enum WriteOutcome {
    Unsent,
    Partial { accepted: usize },
    Accepted,
}

struct Frame {
    bytes: Vec<u8>,
    accepted: usize,
    receipt: Option<oneshot::Sender<WriteOutcome>>,
}

impl Drop for Frame {
    fn drop(&mut self) {
        let outcome = match self.accepted {
            0 => WriteOutcome::Unsent,
            n if n == self.bytes.len() => WriteOutcome::Accepted,
            accepted => WriteOutcome::Partial { accepted },
        };
        if let Some(receipt) = self.receipt.take() {
            let _ = receipt.send(outcome);
        }
    }
}

pub struct Serial {
    session: Session,
    queue: mpsc::Sender<Frame>,
    pub incoming: mpsc::Receiver<Vec<u8>>,
}

impl Serial {
    /// Acquire the exclusive tty during startup, inside an I/O-enabled runtime.
    /// Dropping an unstarted Startup also releases the port and resolves queued receipts.
    pub fn attach(startup: &mut Startup, settings: SerialPortBuilder) -> Result<Self> {
        let port = SerialStream::open(&settings.exclusive(true))?;
        let (queue, frames) = mpsc::channel(16);
        let (received, incoming) = mpsc::channel(16);
        startup.worker(move |session| async move {
            let (reader, writer) = split(port);
            tokio::select! {
                biased;
                _ = session.cancelled() => Ok(()),
                result = transmit(writer, &session, frames) => result.context("serial transmit"),
                result = receive(reader, &session, received) => result.context("serial receive"),
            }
        });
        Ok(Self {
            session: startup.session(),
            queue,
            incoming,
        })
    }

    /// Queue one nonempty frame. A full queue reports WouldBlock. Dropping a
    /// receipt abandons observation only; it cannot retract an admitted frame.
    pub fn try_send(&self, bytes: Vec<u8>) -> io::Result<oneshot::Receiver<WriteOutcome>> {
        if bytes.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "empty serial frame",
            ));
        }
        let (receipt, result) = oneshot::channel();
        self.session
            .while_running(|| {
                self.queue
                    .try_send(Frame {
                        bytes,
                        accepted: 0,
                        receipt: Some(receipt),
                    })
                    .map_err(|error| match error {
                        mpsc::error::TrySendError::Full(_) => io::ErrorKind::WouldBlock,
                        mpsc::error::TrySendError::Closed(_) => io::ErrorKind::BrokenPipe,
                    })
            })
            .ok_or(io::ErrorKind::BrokenPipe)??;
        Ok(result)
    }
}

async fn transmit(
    mut writer: WriteHalf<SerialStream>,
    session: &Session,
    mut frames: mpsc::Receiver<Frame>,
) -> Result<()> {
    while let Some(mut frame) = frames.recv().await {
        while frame.accepted < frame.bytes.len() {
            // Poll one nonblocking write under the same gate that revokes the
            // session. On Pending the gate is released before the task yields.
            let remaining = &frame.bytes[frame.accepted..];
            let result = poll_fn(|cx| {
                match session.while_running(|| Pin::new(&mut writer).poll_write(cx, remaining)) {
                    Some(poll) => poll.map(Some),
                    None => Poll::Ready(None),
                }
            })
            .await;
            let Some(result) = result else {
                return Ok(());
            };
            let count = result?;
            if count == 0 {
                return Err(io::Error::from(io::ErrorKind::WriteZero).into());
            }
            frame.accepted += count;
        }
    }
    bail!("frame producer closed")
}

async fn receive(
    mut reader: ReadHalf<SerialStream>,
    session: &Session,
    chunks: mpsc::Sender<Vec<u8>>,
) -> Result<()> {
    let mut buffer = [0; 2048];
    loop {
        let permit = chunks.reserve().await.context("byte consumer closed")?;
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            bail!("serial peer hung up");
        }
        if session
            .while_running(|| permit.send(buffer[..count].to_vec()))
            .is_none()
        {
            return Ok(());
        }
    }
}
