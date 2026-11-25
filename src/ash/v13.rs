//! Extended EZSP 13/19 headers, following the initial legacy VERSION exchange.
use super::*;
use std::collections::VecDeque;

#[derive(Clone)]
pub struct Callback {
    pub id: u16,
    pub payload: Vec<u8>,
}

pub(crate) enum Next<T> {
    Callback(Callback),
    Wake(T),
}

pub struct Ezsp<'a> {
    pub(super) link: Link<'a>,
    pub(super) authority: Option<crate::bridge::addresses::Authority>,
    pub(super) recent_requests:super::destination::RecentRequests,
    pub(super) standalone: super::destination::Standalone,
    pub(super) incarnation: std::sync::Arc<()>,
    sequence: u8,
    usable: bool,
    pub version: Version,
    pub(super) protocol: u8,
}

impl<'a> Ezsp<'a> {
    /// Protocol offered in the first legacy VERSION exchange.
    pub const PROTOCOL: u8 = 13;
    pub async fn connect(port: &'a mut Serial, limit: Duration) -> Result<Self, ProbeError> {
        let end = tokio::time::Instant::now() + limit;
        let mut link = Link::new(port);
        let version = tokio::time::timeout_at(end, link.run(Self::PROTOCOL, 0))
            .await
            .unwrap_or(Err(Failure::Timeout))
            .and_then(|v| {
                if supports_protocol(v.protocol) && v.stack_type == 2 {
                    Ok(v)
                } else {
                    Err(Failure::UnsupportedVersion)
                }
            })
            .map_err(|cause| ProbeError {
                cause,
                delivery: link.delivery,
            })?;
        let mut session = Self {
            link,
            authority: None,
            standalone: Default::default(),
            recent_requests: Default::default(),
            incarnation: Default::default(),
            sequence: 1,
            usable: true,
            protocol: version.protocol,
            version,
        };
        if version.protocol != Self::PROTOCOL {
            let remaining = end.saturating_duration_since(tokio::time::Instant::now());
            let reply = session.command(0, &[version.protocol], remaining).await?;
            let [lo, hi] = version.stack_version.to_le_bytes();
            if reply != [version.protocol, version.stack_type, lo, hi] {
                return Err(ProbeError {
                    cause: Failure::InvalidVersionResponse,
                    delivery: session.link.delivery,
                });
            }
        }
        Ok(session)
    }

    pub(crate) async fn runtime_command(
        &mut self,
        id: u16,
        args: &[u8],
        limit: Duration,
    ) -> anyhow::Result<Vec<u8>> {
        anyhow::ensure!(id != 0x34 || self.authority.is_none(), "managed unicast requires a destination");
        self.runtime_checked(id, args, limit).await
    }

    pub(super) async fn runtime_checked(
        &mut self, id: u16, args: &[u8], limit: Duration,
    ) -> anyhow::Result<Vec<u8>> {
        if self.protocol == 19 {
            let encoded = ezsp19::encode(id, args)?;
            let body = self.command_inner(id, &encoded, limit).await?;
            ezsp19::response(id, body)
        } else {
            Ok(self.command_inner(id, args, limit).await?)
        }
    }

    /// Send native EZSP parameters and return the native response payload.
    /// Cancellation, timeout and link errors retire the session. A successor
    /// must reacquire the port and reset the NCP before issuing commands.
    pub async fn command(
        &mut self,
        id: u16,
        args: &[u8],
        limit: Duration,
    ) -> Result<Vec<u8>, ProbeError> {
        if id == 0x34 && self.authority.is_some() {
            return Err(ProbeError {cause: Failure::InvalidSession, delivery: Delivery::Unsent});
        }
        self.command_inner(id,args,limit).await
    }

    async fn command_inner(&mut self, id:u16, args:&[u8], limit:Duration) -> Result<Vec<u8>, ProbeError> {
        if !self.usable || args.len() > super::frame::MAX_DATA - 5 {
            return Err(ProbeError {
                cause: Failure::InvalidSession,
                delivery: Delivery::Unsent,
            });
        }
        self.usable = false;
        let seq = self.sequence;
        self.sequence = seq.wrapping_add(1);
        let [lo, hi] = id.to_le_bytes();
        let mut request = vec![seq, 0, 1, lo, hi];
        request.extend_from_slice(args);
        let result = timeout(
            limit,
            self.link.exchange(&request, |link, data| {
                let (sequence, callback, frame, payload) = header(data)?;
                if callback {
                    store_callback(&mut link.callbacks, frame, payload)?;
                    if let Some(cb)=link.callbacks.back() {self.standalone.observe(cb,self.protocol);}
                    return Ok(None);
                }
                if sequence != seq {
                    return Ok(None);
                }
                if frame != id {
                    return Err(Failure::InvalidResponse);
                }
                Ok(Some(payload.to_vec()))
            }),
        )
        .await
        .unwrap_or(Err(Failure::Timeout));
        self.usable = result.is_ok();
        result.map_err(|cause| ProbeError {
            cause,
            delivery: self.link.delivery,
        })
    }

    /// Callbacks received while waiting for an earlier command's reply.
    pub(crate) fn has_buffered_callbacks(&self) -> bool {
        !self.link.callbacks.is_empty()
    }

    /// Receive all native callback fields, including full-width EZSP 19 message
    /// tags and packet metadata. Pair this method with native `command` calls.
    pub async fn next_raw_callback(&mut self) -> Result<Callback, Failure> {
        match self.raw_callback_or(std::future::pending::<()>()).await? {
            Next::Callback(cb) => Ok(cb),
            Next::Wake(()) => unreachable!("pending future cannot wake"),
        }
    }

    /// Receive a callback in the coordinator runtime's parameter model.
    /// Dropping a pending call retires the session just like a cancelled command.
    pub async fn next_callback(&mut self) -> Result<Callback, Failure> {
        self.receive(None).await?.ok_or(Failure::InvalidSession)
    }

    /// Only the idle byte receive can expire. The decoder retains partial frames,
    /// and a started acknowledgement completes before the session is reused.
    pub async fn callback_until(
        &mut self,
        deadline: tokio::time::Instant,
    ) -> Result<Option<Callback>, Failure> {
        self.receive(Some(deadline)).await
    }

    async fn receive(
        &mut self,
        deadline: Option<tokio::time::Instant>,
    ) -> Result<Option<Callback>, Failure> {
        let wake = async {
            match deadline {
                Some(end) => tokio::time::sleep_until(end).await,
                None => std::future::pending::<()>().await,
            }
        };
        Ok(match self.callback_or(wake).await? {
            Next::Callback(cb) => Some(cb),
            Next::Wake(()) => None,
        })
    }

    /// Race another input only while waiting for raw bytes. Once a frame has
    /// arrived, update the ASH receive window and finish its ACK before yielding.
    pub(crate) async fn callback_or<T>(
        &mut self,
        wake: impl std::future::Future<Output = T>,
    ) -> Result<Next<T>, Failure> {
        Ok(self.callback_or_with_sender(wake).await?.0)
    }

    pub(crate) async fn callback_or_with_sender<T>(
        &mut self,
        wake: impl std::future::Future<Output = T>,
    ) -> Result<(Next<T>, Option<u64>), Failure> {
        tokio::pin!(wake);
        loop {
            match self.raw_callback_or(wake.as_mut()).await? {
                Next::Callback(raw) if self.protocol == 19 => {
                    if let Some(cb) = ezsp19::callback(raw.id, &raw.payload)? {
                        let sender = ezsp19::source_eui(raw.id, &raw.payload);
                        return Ok((Next::Callback(cb), sender));
                    }
                }
                next => return Ok((next, None)),
            }
        }
    }

    async fn raw_callback_or<T>(
        &mut self,
        wake: impl std::future::Future<Output = T>,
    ) -> Result<Next<T>, Failure> {
        if !self.usable {
            return Err(Failure::InvalidSession);
        }
        tokio::pin!(wake);
        if self.link.refresh_pause.is_some() {
            // Drain already accepted callbacks before admitting new work while
            // the NCP is paused. Cancellation during a control write retires the link.
            self.usable = false;
            let next = self.link.callbacks.pop_front();
            if self.link.callbacks.len() <= RESUME_AT
                || self.link.refresh_pause.is_some_and(|at| Instant::now() >= at)
            {
                self.link.acknowledge(false).await?;
            }
            self.usable = true;
            if let Some(cb) = next { return Ok(Next::Callback(cb)); }
        }
        if !self.link.callbacks.is_empty() {
            // A busy radio can queue more callbacks on each command. Give
            // another ready source a turn even while callbacks are buffered.
            if self.link.callbacks.iter().any(|c| matches!(c.id,0x19|0x24|0x7c)) {
                return Ok(Next::Callback(self.link.callbacks.pop_front().expect("queued lifecycle callback")));
            }
            tokio::select! {
                input = &mut wake => return Ok(Next::Wake(input)),
                _ = std::future::ready(()) => {
                    return Ok(Next::Callback(self.link.callbacks.pop_front().expect("queued callback")));
                }
            }
        }
        self.usable = false;
        loop {
            let event = tokio::select! {
                input = &mut wake => {self.usable = true; return Ok(Next::Wake(input));},
                event = self.link.next() => event?,
            };
            // Between commands only the current (next outbound) ACK number is valid.
            let action = self.link.rx.receive(event)?;
            match action {
                Action::Deliver(data) => {
                    let (_, callback, id, body) = header(&data)?;
                    self.link.acknowledge(false).await?;
                    if callback {
                        self.usable = true;
                        let cb=Callback {id,payload:body.to_vec()};
                        self.standalone.observe(&cb,self.protocol);
                        return Ok(Next::Callback(cb));
                    }
                }
                Action::Ack => self.link.acknowledge(false).await?,
                Action::Nak => self.link.acknowledge(true).await?,
                Action::Ignore | Action::Retry => (),
            }
        }
    }
}

fn header(data: &[u8]) -> Result<(u8, bool, u16, &[u8]), Failure> {
    let [seq, ctrl, 1, lo, hi, payload @ ..] = data else {
        return Err(Failure::InvalidResponse);
    };
    if ctrl & 0xe3 != 0x80 {
        return Err(Failure::InvalidResponse);
    }
    Ok((
        *seq,
        ctrl & 0x18 != 0,
        u16::from_le_bytes([*lo, *hi]),
        payload,
    ))
}

fn store_callback(queue: &mut VecDeque<Callback>, id: u16, payload: &[u8]) -> Result<(), Failure> {
    if queue.len() >= CALLBACK_CAPACITY {
        return Err(Failure::CallbackOverflow);
    }
    queue.push_back(Callback {
        id,
        payload: payload.to_vec(),
    });
    Ok(())
}
