//! Single-request-at-a-time ASH v2 link to an Ember NCP.
//!
//! This bench operation sends a software reset. Use a freshly acquired
//! exclusive serial session. After the probe, shut down its supervisor before
//! opening a successor. A dropped future can leave an admitted write pending.
mod frame;
mod ezsp19;
mod destination;
pub(crate) use destination::{Destination, locally_refused};
mod v13;
use crate::serial::{Serial, WriteOutcome};
use frame::{Decoder, Frame};
use std::{error::Error, fmt, io, time::Duration};
use tokio::time::{Instant, timeout, timeout_at};
pub use v13::{Callback, Ezsp};
/// Whether the runtime has a wire codec for this EZSP protocol.
pub const fn supports_protocol(protocol: u8) -> bool {
    matches!(protocol, 13 | 19)
}

/// Original name for the coordinator session, retained for existing callers.
#[deprecated(note = "use Ezsp to negotiate supported protocol versions")]
pub type Ezsp13<'a> = Ezsp<'a>;
pub(crate) use v13::Next;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Version {
    pub protocol: u8,
    pub stack_type: u8,
    pub stack_version: u16,
}

/// What the host can establish about the current request on failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    Unsent,
    OutcomeUnknown { link_acknowledged: bool },
    ReplyReceived(Version),
    ResponseReceived,
}

#[derive(Debug)]
pub enum Failure {
    Timeout,
    Serial(io::Error),
    UnsupportedAsh(u8),
    PeerReset(u8),
    PeerError(u8),
    RetryLimit,
    InvalidVersionResponse,
    InvalidResponse,
    CallbackOverflow,
    InvalidSession,
    UnsupportedVersion,
    StackStatus(u32),
}

#[derive(Debug)]
pub struct ProbeError {
    pub delivery: Delivery,
    pub cause: Failure,
}

impl fmt::Display for ProbeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Ember request {:?}, delivery {:?}",
            self.cause, self.delivery
        )
    }
}
impl Error for ProbeError {}

/// Probe the NCP's advertised version. A different returned protocol number
/// is reported verbatim; using a full EZSP session would require negotiating
/// a supported version. The time limit covers local writes and all peer waits.
pub async fn probe_version(
    port: &mut Serial,
    preferred: u8,
    sequence: u8,
    limit: Duration,
) -> Result<Version, ProbeError> {
    let mut probe = Link::new(port);
    if limit.is_zero() {
        return Err(ProbeError {
            delivery: Delivery::Unsent,
            cause: Failure::Timeout,
        });
    }
    let result = timeout(limit, probe.run(preferred, sequence))
        .await
        .unwrap_or(Err(Failure::Timeout));
    result.map_err(|cause| ProbeError {
        delivery: probe.delivery,
        cause,
    })
}

const CALLBACK_CAPACITY: usize = 64;
// ASH has three-bit sequence numbers. Leave space for the full in-flight window.
const PAUSE_AT: usize = CALLBACK_CAPACITY - 8;
const RESUME_AT: usize = CALLBACK_CAPACITY / 2;
const PAUSE_REFRESH: Duration = Duration::from_millis(480);

struct Link<'a> {
    rx: ReceiveWindow,
    callbacks: std::collections::VecDeque<Callback>,
    refresh_pause: Option<Instant>,
    port: &'a mut Serial,
    bytes: std::vec::IntoIter<u8>,
    decoder: Decoder,
    delivery: Delivery,
}

impl<'a> Link<'a> {
    fn new(port: &'a mut Serial) -> Self {
        Self {
            port,
            bytes: Vec::new().into_iter(),
            decoder: Decoder::default(),
            delivery: Delivery::Unsent,
            rx: ReceiveWindow::default(),
            callbacks: Default::default(),
            refresh_pause: None,
        }
    }

    async fn send(&mut self, control: u8, data: &[u8]) -> Result<(), Failure> {
        let receipt = self
            .port
            .try_send(Frame::encode(control, data))
            .map_err(Failure::Serial)?;
        let first_command = control & 0x88 == 0 && self.delivery == Delivery::Unsent;
        if first_command {
            self.delivery = Delivery::OutcomeUnknown {
                link_acknowledged: false,
            };
        }
        match receipt.await {
            Ok(WriteOutcome::Accepted) => Ok(()),
            Ok(WriteOutcome::Unsent) if first_command => {
                self.delivery = Delivery::Unsent;
                Err(Failure::Serial(io::ErrorKind::BrokenPipe.into()))
            }
            _ => Err(Failure::Serial(io::ErrorKind::BrokenPipe.into())),
        }
    }

    // UG101, DATA Frame Flow Control: nRdy holds callbacks only. Command
    // replies and ASH retransmissions still flow in both directions.
    async fn acknowledge(&mut self, nak: bool) -> Result<(), Failure> {
        let occupied = self.callbacks.len();
        let pause = occupied >= PAUSE_AT || (self.refresh_pause.is_some() && occupied > RESUME_AT);
        let control = if nak { 0xa0 } else { 0x80 };
        self.send(control | (u8::from(pause) << 3) | self.rx.next, &[]).await?;
        self.refresh_pause = pause.then(|| Instant::now() + PAUSE_REFRESH);
        Ok(())
    }

    async fn next(&mut self) -> Result<Result<Frame, ()>, Failure> {
        loop {
            for byte in self.bytes.by_ref() {
                if let Some(event) = self.decoder.push(byte) {
                    return Ok(event);
                }
            }
            self.bytes = self
                .port
                .incoming
                .recv()
                .await
                .ok_or_else(|| Failure::Serial(io::ErrorKind::BrokenPipe.into()))?
                .into_iter();
        }
    }

    async fn reset(&mut self) -> Result<(), Failure> {
        self.send(0xc0, &[]).await?;
        loop {
            match self.next().await? {
                Ok(Frame {
                    control: 0xc1,
                    data,
                }) if data[1] == 0x0b => {
                    if data[0] != 2 {
                        return Err(Failure::UnsupportedAsh(data[0]));
                    }
                    break;
                }
                Ok(Frame {
                    control: 0xc2,
                    data,
                }) => return Err(Failure::PeerError(data[1])),
                _ => (), // Boot chatter and unsolicited power-on resets can precede our RSTACK.
            }
        }
        self.send(0x80, &[]).await
    }

    async fn run(&mut self, preferred: u8, sequence: u8) -> Result<Version, Failure> {
        self.reset().await?;
        let request = [sequence, 0, 0, preferred];
        self.exchange(&request, |link, data| {
            let result = version_reply(sequence, data)?;
            if let Some(version) = result {
                link.delivery = Delivery::ReplyReceived(version);
            }
            Ok(result)
        })
        .await
    }

    async fn exchange<T>(
        &mut self,
        request: &[u8],
        mut parse: impl FnMut(&mut Self, &[u8]) -> Result<Option<T>, Failure>,
    ) -> Result<T, Failure> {
        self.delivery = Delivery::Unsent;
        if self.rx.acknowledged {
            self.rx.sent = (self.rx.sent + 1) & 7;
        }
        self.rx.acknowledged = false;
        let control = self.rx.sent << 4;
        self.send(control | self.rx.next, request).await?;
        // Each direction has its own frame counter. The ACK
        // deadline remains absolute while other traffic is being received.
        let mut retries = 0;
        let mut ack_wait = Duration::from_millis(1600);
        let mut ack_due = Instant::now() + ack_wait;

        loop {
            let now = Instant::now();
            if self.refresh_pause.is_some_and(|at| now >= at) {
                self.acknowledge(false).await?;
            }
            let ack_deadline = (!self.rx.acknowledged).then_some(ack_due);
            let action = if ack_deadline.is_some_and(|at| now >= at) {
                Action::Retry
            } else {
                let next_deadline = ack_deadline.into_iter().chain(self.refresh_pause).min();
                let event = match next_deadline {
                    Some(at) => match timeout_at(at, self.next()).await {
                        Ok(event) => event?,
                        Err(_) => continue,
                    },
                    None => self.next().await?,
                };
                self.rx.receive(event)?
            };
            if self.rx.acknowledged {
                self.delivery = Delivery::OutcomeUnknown {
                    link_acknowledged: true,
                };
            }
            match action {
                Action::Deliver(data) => {
                    let reply = parse(self, &data)?;
                    if reply.is_some() && matches!(self.delivery, Delivery::OutcomeUnknown { .. }) {
                        self.delivery = Delivery::ResponseReceived;
                    }
                    self.acknowledge(false).await?;
                    if let Some(value) = reply {
                        if !self.rx.acknowledged {
                            return Err(Failure::InvalidResponse);
                        }
                        return Ok(value);
                    }
                }
                Action::Ack => self.acknowledge(false).await?,
                Action::Nak => self.acknowledge(true).await?,
                Action::Ignore => (),
                Action::Retry => {
                    if retries == 3 {
                        return Err(Failure::RetryLimit);
                    }
                    retries += 1;
                    self.send(control | 8 | self.rx.next, request).await?;
                    ack_wait = (ack_wait * 2).min(Duration::from_millis(3200));
                    ack_due = Instant::now() + ack_wait;
                }
            }
        }
    }
}

enum Action {
    Ack,
    Nak,
    Ignore,
    Retry,
    Deliver(Vec<u8>),
}

// Each direction starts at zero after reset and wraps modulo eight.
#[derive(Default)]
struct ReceiveWindow {
    sent: u8,
    next: u8,
    acknowledged: bool,
    rejecting: bool,
}

impl ReceiveWindow {
    fn receive(&mut self, frame: Result<Frame, ()>) -> Result<Action, Failure> {
        let Ok(Frame { control, data }) = frame else {
            return Ok(self.reject());
        };
        match control {
            0xc1 => return Err(Failure::PeerReset(data[1])),
            0xc2 => return Err(Failure::PeerError(data[1])),
            0xc0 => return Ok(self.reject()),
            _ => (),
        }
        let ack = control & 7;
        let successor = (self.sent + 1) & 7;
        if ack != successor && (ack != self.sent || self.acknowledged) {
            return Ok(self.reject());
        }
        self.acknowledged |= ack == successor;
        let action = match control {
            0..=0x7f if control >> 4 == self.next => {
                self.next = (self.next + 1) & 7;
                self.rejecting = false;
                Action::Deliver(data)
            }
            0..=0x7f if control & 8 != 0 => Action::Ack,
            0..=0x7f => self.reject(),
            0xa0..=0xaf if !self.acknowledged => Action::Retry,
            _ => Action::Ignore,
        };
        Ok(action)
    }

    fn reject(&mut self) -> Action {
        if std::mem::replace(&mut self.rejecting, true) {
            Action::Ignore
        } else {
            Action::Nak
        }
    }
}

fn version_reply(sequence: u8, data: &[u8]) -> Result<Option<Version>, Failure> {
    let [seq, control, id, args @ ..] = data else {
        return Ok(None);
    };
    // Network zero, response direction, neither kind of callback. The pending
    // callback and overflow flags can accompany an otherwise complete reply.
    if *seq != sequence || control & 0xf8 != 0x80 || *id != 0 {
        return Ok(None);
    }
    let [protocol, stack_type, lo, hi] = args else {
        return Err(Failure::InvalidVersionResponse);
    };
    if control & 2 != 0 {
        return Err(Failure::InvalidVersionResponse);
    }
    Ok(Some(Version {
        protocol: *protocol,
        stack_type: *stack_type,
        stack_version: u16::from_le_bytes([*lo, *hi]),
    }))
}

#[cfg(all(test, target_os = "linux"))]
mod tests;
