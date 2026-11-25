//! ZCL R8 §3.16. A check-in is an opportunity to serve one sleepy peer.
//! All fast-poll requests expire at the device even if this session is lost.
use super::{Bindings, Identity, Management, ember::Pending, responses::Outcome, zdo};
use crate::{ash::Ezsp, interview::Incoming};
use anyhow::Result;
use std::{collections::BTreeMap, time::Duration};
use tokio::time::Instant;
use zigbee_zcl::{
    Direction, Frame, FrameType, Header,
    cluster::{Command, PollControl},
};

pub(super) const CLUSTER: u16 = 0x0020;
// Match the bridge's eight-second application transaction window, in quarter seconds.
const FAST_QUARTERS: u16 = 32;

pub(super) struct Work<'a> {
    pub pending: &'a BTreeMap<u8, Pending>,
    pub deferred: Option<u64>,
    pub management: Option<&'a Management>,
}
impl Work<'_> {
    fn for_peer(&self, ieee: u64, now: Instant) -> bool {
        self.deferred == Some(ieee)
            || self
                .pending
                .values()
                .any(|p| p.peer.0 == ieee && (p.until > now || p.refresh_deadline().is_some()))
            || self.management.is_some_and(|m| m.poll_work(ieee))
    }
}

#[derive(Clone)]
struct Window {
    destination:crate::ash::Destination,
    short: u16,
    profile: u16,
    until: Instant,
}
#[derive(Default)]
pub(super) struct Client {
    windows: BTreeMap<Identity, Window>,
    sequence: u8,
}

fn command(sequence: u8, cmd: PollControl) -> Result<Vec<u8>> {
    let cmd = Command::PollControl(cmd);
    let mut bytes = Vec::new();
    Frame {
        header: Header {
            frame_type: FrameType::ClusterSpecific,
            manufacturer: None,
            direction: Direction::ClientToServer,
            disable_default_response: true,
            sequence,
            command_id: cmd.key().command,
        },
        payload: &cmd.encode()?,
    }
    .encode_into(&mut bytes);
    Ok(bytes)
}

impl Client {
    pub(super) fn forget(&mut self, ieee: u64, short: u16) {
        self.windows
            .retain(|id, w| id.0 != ieee && w.short != short);
    }

    pub(super) fn deadline(&self) -> Option<Instant> {
        self.windows.values().map(|w| w.until).min()
    }

    pub(super) async fn check_in(
        &mut self,
        link: &mut Ezsp<'_>,
        peer: Identity,
        msg: &Incoming<'_>,
        frame: Frame<'_>,
        work: Work<'_>,
    ) -> Result<()> {
        let destination=link.destination(peer.0,msg.sender)?;
        if !matches!(
            Command::decode(CLUSTER, frame),
            Ok(Command::PollControl(PollControl::CheckIn {}))
        ) {
            return super::responses::finish(link, &destination, msg, frame, Outcome::Malformed).await;
        }
        let now = Instant::now();
        let fast = work.for_peer(peer.0, now);
        let quarters = if fast { FAST_QUARTERS } else { 0 };
        let wire = command(
            frame.header.sequence,
            PollControl::CheckInResponse {
                start_fast_polling: fast,
                fast_poll_timeout: quarters,
            },
        )?;
        let status = zdo::submit_unicast(
            link,
            &destination,
            msg.aps.profile,
            CLUSTER,
            (msg.aps.dst, peer.1),
            0xff,
            &wire,
        )
        .await?;
        if status != 0 {
            eprintln!("Poll Control response declined: Ember {status:#04x}");
        } else if fast {
            self.windows.insert(
                peer,
                Window {
                    destination,
                    short: msg.sender,
                    profile: msg.aps.profile,
                    until: now + Duration::from_millis(u64::from(quarters) * 250),
                },
            );
        }
        Ok(())
    }

    pub(super) async fn finish_idle(
        &mut self,
        link: &mut Ezsp<'_>,
        work: Work<'_>,
        devices: &Bindings,
        now: Instant,
    ) -> Result<()> {
        // An expired request needs no stop. Recheck ownership before addressing
        // a delayed stop, since the short address can change during this window.
        self.windows
            .retain(|id, w| w.until > now && devices.peers.contains_key(&id.0));
        let idle: Vec<_> = self
            .windows
            .keys()
            .copied()
            .filter(|id| !work.for_peer(id.0, now))
            .collect();
        for id in idle {
            let w = self.windows.remove(&id).expect("listed window");
            if !zdo::still_assigned(link, &w.destination).await? || w.until <= Instant::now() {
                continue;
            }
            self.sequence = self.sequence.wrapping_add(1);
            let stop = command(self.sequence, PollControl::FastPollStop {})?;
            let status = match zdo::submit_unicast(
                link, &w.destination, w.profile, CLUSTER, (1, id.1), 0xff, &stop,
            ).await {
                Ok(status) => status,
                Err(e) if crate::ash::locally_refused(&e) => continue,
                Err(e) => return Err(e),
            };
            if status != 0 {
                eprintln!("Fast Poll Stop declined: Ember {status:#04x}");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn address_reuse_and_rejoin_retire_any_old_poll_window() {
        let mut client = Client::default();
        let tick = Instant::now();
        for (id, node) in [((1, 1), 5), ((2, 1), 6), ((2, 4), 6), ((3, 1), 7)] {
            client.windows.insert(
                id,
                Window {
                    destination:crate::ash::Destination::fixture(id.0,node),
                    short: node,
                    profile: 0x104,
                    until: tick,
                },
            );
        }
        client.forget(1, 6);
        assert_eq!(client.windows.keys().collect::<Vec<_>>(), [&(3, 1)]);
    }
}
