//! Evidence exported by this CIE to its authorised IAS ACE readers.
use super::{Enrollment, server, table::UNASSIGNED};
use crate::{
    ash::Destination,
    bridge::{Bindings, Identity, ace::Zone},
    interview::Incoming,
};
use anyhow::Result;
use std::{collections::BTreeMap, time::Duration};
use tokio::time::Instant;
use zigbee_devices::ias::CLUSTER;
use zigbee_zcl::{
    Direction, Frame,
    cluster::{Command, IasZone},
};

impl Enrollment {
    pub(in crate::bridge) fn revoke_reader(&mut self, ieee: u64) -> Result<()> {
        self.table.leave(ieee)
    }

    pub(in crate::bridge) fn departed(&self, ieee: u64) -> bool {
        self.table.left.contains(&ieee)
    }

    pub(in crate::bridge) fn directory(&self, devices: &Bindings, current: impl Fn(&Destination) -> bool) -> BTreeMap<u8, Zone> {
        self.table
            .rows
            .iter()
            .filter_map(|(&peer, row)| {
                if !row.confirmed
                    || row.id == UNASSIGNED
                    || self.departed(peer.0)
                    || server(devices, peer) != Some(row.profile)
                {
                    return None;
                }
                let kind = devices
                    .devices
                    .get(&peer)
                    .and_then(|b| b.zone)
                    .and_then(|z| z.zone_type)
                    .filter(|&n| n != u16::MAX);
                let status = self
                    .status_samples
                    .get(&peer)
                    .filter(|(at, _, destination)| at.elapsed() <= Duration::from_secs(300) && current(destination))
                    .map(|(_, s, _)| *s);
                Some((
                    row.id,
                    Zone {
                        ieee: peer.0,
                        kind,
                        status,
                    },
                ))
            })
            .collect()
    }

    pub(in crate::bridge) fn observe_ace_status(
        &mut self,
        peer: Identity,
        msg: &Incoming<'_>,
        frame: Frame<'_>,
        destination: &Destination,
    ) {
        if (msg.sender, peer.0) != (destination.short(), destination.ieee())
            || !matches!(msg.kind, 0 | 1)
            || msg.aps.cluster != CLUSTER
            || msg.aps.dst != 1
            || !msg.aps_encrypted
            || frame.header.manufacturer.is_some()
            || frame.header.direction != Direction::ServerToClient
        {
            return;
        }
        let Some(row) = self.table.rows.get(&peer) else {
            return;
        };
        if !row.confirmed || row.profile != msg.aps.profile || self.departed(peer.0) {
            return;
        }
        let now = Instant::now();
        let sample = match Command::decode(CLUSTER, frame) {
            Ok(Command::IasZone(IasZone::ZoneStatusChangeNotification {
                zone_id,
                zone_status,
                delay,
                ..
            })) if zone_id == row.id => {
                let Some(at) = now.checked_sub(Duration::from_millis(250 * u64::from(delay)))
                else {
                    return;
                };
                Some((at, zone_status))
            }
            _ if frame.header.general_command()
                == Some(zigbee_zcl::foundation::GeneralCommand::ReportAttributes) =>
            {
                zigbee_devices::ias::observe(frame, None)
                    .status
                    .map(|s| (now, s))
            }
            _ => None,
        };
        if let Some((at, status)) = sample
            && self
                .status_samples
                .get(&peer)
                .is_none_or(|(prior, _, _)| at >= *prior)
        {
            self.status_samples.insert(peer, (at, status, destination.clone()));
        }
    }
}
