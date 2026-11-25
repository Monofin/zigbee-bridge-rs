//! Resolve an Options read against the current peer before issuing its write.
use super::{Bindings, Identity, Read, ember::Pending};
use anyhow::Result;
use std::{collections::BTreeMap, time::Duration};
use tokio::time::Instant;
use zigbee_devices::standard_controls::level_config::{CLUSTER, Field, PROPERTY, Patch, State};
use zigbee_zcl::{
    Direction, Frame,
    foundation::{GeneralCommand, Payload},
};

#[derive(Clone, Copy)]
pub(super) enum Operation {
    Read(Field),
    Readback,
    ReadOptions(Patch),
    Write,
}

pub(super) fn busy(
    pending: &BTreeMap<u8, Pending>,
    peer: Identity,
    command: &crate::mqtt::Command,
) -> bool {
    command.operation == crate::mqtt::Operation::Set
        && command.properties.contains_key(PROPERTY)
        && pending.values().any(|p| {
            p.peer == peer
                && matches!(
                    p.read,
                    Some(Read::LevelConfig(
                        Operation::Write | Operation::ReadOptions(_)
                    ))
                )
        })
}

pub(super) async fn receive(
    link: &mut crate::ash::Ezsp<'_>,
    devices: &mut Bindings,
    out: &crate::mqtt::Publisher,
    pending: &mut BTreeMap<u8, Pending>,
    peer: Identity,
    short: u16,
    frame: Frame<'_>,
) -> Result<()> {
    if frame.header.direction != Direction::ServerToClient || frame.header.manufacturer.is_some() {
        return Ok(());
    }
    let sequence = frame.header.sequence;
    let op = pending.get(&sequence).and_then(|entry| match entry.read {
        Some(Read::LevelConfig(op))
            if (entry.peer, entry.short, entry.cluster) == (peer, short, CLUSTER) =>
        {
            Some(op)
        }
        _ => None,
    });
    if let Some(Operation::Write) = op
        && frame.header.general_command() == Some(GeneralCommand::WriteAttributesResponse)
    {
        if matches!(
            Payload::decode(frame),
            Ok(Payload::WriteAttributesResponse(_))
        ) {
            // An ordinary multi-attribute write can partially succeed. Read the
            // actual fields after either response, without publishing the request.
            let entry = pending.get_mut(&sequence).expect("matched request");
            entry.read = Some(Read::Refresh(PROPERTY));
            entry.until = Instant::now();
        }
        return Ok(());
    }
    let expected = match frame.header.general_command() {
        Some(GeneralCommand::ReportAttributes) => None,
        Some(GeneralCommand::ReadAttributesResponse) => match op {
            Some(Operation::Read(field)) => Some(field),
            Some(Operation::ReadOptions(_)) => Some(Field::Options),
            Some(Operation::Readback) => None,
            _ => return Ok(()),
        },
        _ => return Ok(()),
    };
    let readback=matches!(op,Some(Operation::Readback))
        && frame.header.general_command()==Some(GeneralCommand::ReadAttributesResponse);
    let observed = match if readback {State::observe_readback(frame)} else {State::observe(frame, expected)} {
        Ok(Some(state)) => state,
        Ok(None) => return Ok(()),
        Err(error) => {
            eprintln!("Invalid level configuration frame: {error}");
            return Ok(());
        }
    };
    let binding = devices.devices.get_mut(&peer).expect("matched member");
    if let Some(value) = binding.level_state.merge(observed) {
        super::ember::submitted(out.state(&binding.name, PROPERTY, value))?;
    }
    if expected.is_none() && !readback {
        return Ok(());
    }
    let Some((destination, route_epoch)) = pending.remove(&sequence).map(|p|(p.destination,p.route_epoch)) else {return Ok(());};
    if let Some(Operation::ReadOptions(patch)) = op {
        let Some(bitmap) = observed.options() else {
            eprintln!("Level configuration write cancelled: Options read failed");
            return Ok(());
        };
        // Use a new sequence so a delayed send confirmation for the read
        // cannot complete the write. Never write through a different address.
        let Some(tag) = super::ember::available_tag(link,pending,&destination,binding.descriptor.profile,CLUSTER,(1,peer.1),sequence.wrapping_add(1))
        else {
            eprintln!("Level configuration write cancelled: all transaction slots busy");
            return Ok(());
        };
        let req = patch.write(peer.1, tag, Some(bitmap))?;
        if super::zdo::still_assigned(link, &destination).await?
            && super::zdo::submit_unicast(
                link,
                &destination,
                binding.descriptor.profile,
                CLUSTER,
                (1, peer.1),
                tag,
                &req.frame,
            )
            .await?
                == 0
        {
            pending.insert(
                tag,
                Pending {
                    route_epoch,
                    destination,
                    schedule:None,
                    peer,
                    short,
                    cluster: CLUSTER,
                    read: Some(Read::LevelConfig(Operation::Write)),
                    until: Instant::now() + Duration::from_secs(8),
                },
            );
        }
    }
    Ok(())
}
