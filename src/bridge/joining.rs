//! Cooperatively run the ordinary interview while the adapter handles other traffic.
//! Dropping the job drops its future and both queues; no task outlives the radio.
use crate::{
    ash::{Callback, Destination, Ezsp},
    interview::{self, InterviewIo},
    interview_log::{Entry, IdentitySource, InterviewReport, Observation},
};
use anyhow::{Context, Result, ensure};
use std::{
    future::Future,
    pin::Pin,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::{
    sync::{mpsc, oneshot},
    time::Instant,
};

pub(super) struct Send {
    pub args: Vec<u8>,
    pub result: oneshot::Sender<Result<Vec<u8>>>,
}
struct Port {
    maintenance: bool,
    send: mpsc::Sender<Send>,
    incoming: mpsc::Receiver<Callback>,
}
impl InterviewIo for Port {
    fn next_sequence(&self, current: u8) -> u8 {
        if self.maintenance && current == u8::MAX {
            interview::reporting::FIRST_SEQUENCE
        } else {
            current.wrapping_add(1)
        }
    }
    async fn send(&mut self, args: &[u8]) -> Result<Vec<u8>> {
        let (result, wait) = oneshot::channel();
        self.send
            .send(Send {
                args: args.to_vec(),
                result,
            })
            .await
            .map_err(|_| anyhow::anyhow!("interview stopped"))?;
        wait.await?
    }
    async fn receive(&mut self, deadline: Instant) -> Result<Option<Callback>> {
        match tokio::time::timeout_at(deadline, self.incoming.recv()).await {
            Ok(cb) => Ok(Some(cb.context("interview stopped")?)),
            Err(_) => Ok(None),
        }
    }
}

pub(super) struct Event {
    owner: mpsc::Sender<Callback>,
    pub step: Step,
}
impl Event {
    pub fn belongs_to(&self, interview: &Interview) -> bool {
        self.owner.same_channel(&interview.incoming)
    }
}
pub(super) enum Step {
    Send(Send),
    Finished(Result<InterviewReport>),
}

pub(super) struct Interview {
    pub destination: Destination,
    pub mains_powered: bool,
    pub maintenance: bool,
    task: Pin<Box<dyn Future<Output = Result<InterviewReport>>>>,
    requests: mpsc::Receiver<Send>,
    incoming: mpsc::Sender<Callback>,
}
impl Interview {
    pub fn new(destination: Destination, capability: u8) -> Self {
        Self::start(destination, capability, None)
    }
    pub fn setup(
        destination: Destination,
        coordinator: u64,
        plans: Vec<interview::reporting::Endpoint>,
    ) -> Self {
        Self::start(destination, 0, Some((coordinator, plans)))
    }
    fn start(
        destination: Destination,
        capability: u8,
        setup: Option<(u64, Vec<interview::reporting::Endpoint>)>,
    ) -> Self {
        let ieee = destination.ieee();
        let short = destination.short();
        let maintenance = setup.is_some();
        let (send, requests) = mpsc::channel(1);
        let (incoming, rx) = mpsc::channel(32);
        let task = Box::pin(async move {
            let mut entries = Vec::new();
            let mut port = Port {
                send,
                incoming: rx,
                maintenance,
            };
            let mut record = |observation| {
                let unix_ms = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis().try_into()?;
                if !maintenance && matches!(observation, Observation::Finished) {
                    entries.push(Entry { unix_ms,
                        observation: Observation::ReceiverOnWhenIdle { enabled: capability & 0x08 != 0 } });
                }
                entries.push(Entry {
                    unix_ms,
                    observation,
                });
                Ok(())
            };
            if let Some((coordinator, plans)) = setup {
                interview::reporting::reconcile(
                    &mut port,
                    short,
                    ieee.to_le_bytes(),
                    coordinator,
                    &plans,
                    &mut record,
                )
                .await?;
            } else {
                interview::interview(
                    &mut port,
                    short,
                    ieee.to_le_bytes(),
                    IdentitySource::Announcement,
                    &mut record,
                )
                .await?;
            }
            Ok(InterviewReport {
                finished: true,
                entries,
            })
        });
        Self {
            destination,
            mains_powered: capability & 0x04 != 0,
            maintenance,
            task,
            requests,
            incoming,
        }
    }
    pub async fn event(&mut self) -> Event {
        let step = tokio::select! {
            done=&mut self.task=>Step::Finished(done),
            Some(req)=self.requests.recv()=>Step::Send(req),
        };
        Event { owner: self.incoming.clone(), step }
    }
    pub fn observe(&self, link: &Ezsp<'_>, cb: &Callback) -> Result<()> {
        ensure!(link.current(&self.destination), "interview address changed");
        if cb.id == 0x45 {
            let m = interview::incoming(&cb.payload)?;
            if m.sender == self.destination.short()
                && m.kind <= 1
                && (m.aps.profile == 0
                    || zigbee_zcl::Frame::decode(m.body).is_ok_and(|frame| {
                        matches!(
                            frame.header.general_command(),
                            Some(
                                zigbee_zcl::foundation::GeneralCommand::ReadAttributesResponse
                                    | zigbee_zcl::foundation::GeneralCommand::ConfigureReportingResponse
                                    | zigbee_zcl::foundation::GeneralCommand::ReadReportingConfigurationResponse
                                    | zigbee_zcl::foundation::GeneralCommand::DiscoverAttributesExtendedResponse
                                    | zigbee_zcl::foundation::GeneralCommand::DiscoverCommandsReceivedResponse
                                    | zigbee_zcl::foundation::GeneralCommand::DefaultResponse
                            )
                        )
                    }))
            {
                ensure!(
                    self.incoming.try_send(cb.clone()).is_ok(),
                    "interview callback queue full"
                );
            }
        }
        Ok(())
    }
}
