//! Standard HVAC operations are scoped to a single interviewed endpoint.
use super::{
    Bindings, Identity, Read,
    ember::{Pending, submitted},
};
use crate::mqtt::{Publisher, discovery::Feature};
use anyhow::Result;
use std::collections::BTreeMap;
use tokio::time::Instant;
use zigbee_devices::hvac::{Capabilities, PROPERTIES, Property, Setpoints};
use zigbee_zcl::{
    Direction, Frame,
    attributes::{Status, WriteStatus},
    foundation::Payload,
};

#[derive(Clone, Copy)]
pub(super) struct Operation {
    pub property: Property,
    pub write: bool,
}

pub(super) fn features(cap: &Capabilities) -> Vec<Feature> {
    let modes = cap.modes(Property::SystemMode);
    let climate = [Property::HeatingSetpoint, Property::CoolingSetpoint]
        .into_iter()
        .find_map(|p| {
            cap.setpoint_limits(p)
                .filter(|_| !modes.is_empty() && !cap.dual_mode())
                .map(|(lo, hi)| Feature::Thermostat {
                    modes,
                    setpoint: p.name(),
                    min: f64::from(lo) / 100.,
                    max: f64::from(hi) / 100.,
                })
        });
    climate
        .into_iter()
        .chain(
            PROPERTIES
                .into_iter()
                .filter(|p| climate.is_none() || *p == Property::FanMode)
                .filter(|&p| cap.readable(p))
                .map(|p| {
                    let name = p.name();
                    let modes = cap.modes(p);
                    if !modes.is_empty() {
                        Feature::Select {
                            property: name,
                            options: modes,
                        }
                    } else if let Some((lo, hi)) = cap.setpoint_limits(p) {
                        Feature::Number {
                            property: name,
                            min: f64::from(lo) / 100.,
                            max: f64::from(hi) / 100.,
                            step: 0.01,
                            unit: Some("°C"),
                        }
                    } else if matches!(p, Property::SystemMode | Property::FanMode) {
                        Feature::TextSensor(name)
                    } else {
                        Feature::Measurement {
                            property: name,
                            unit: Some("°C"),
                        }
                    }
                }),
        )
        .collect()
}

#[derive(Clone, Copy)]
pub(super) enum SetpointOperation {
    Preflight(Setpoints),
    Write(Setpoints),
    Readback,
}

pub(super) fn busy(
    pending: &BTreeMap<u8, Pending>,
    peer: Identity,
    cmd: &crate::mqtt::Command,
) -> bool {
    cmd.operation == crate::mqtt::Operation::Set
        && [Property::HeatingSetpoint, Property::CoolingSetpoint]
            .iter()
            .any(|p| cmd.properties.contains_key(p.name()))
        && pending
            .values()
            .any(|p| p.peer == peer && matches!(p.read, Some(Read::Setpoints(_))))
}

pub(super) async fn receive(
    link: &mut crate::ash::Ezsp<'_>,
    devices: &mut Bindings,
    out: &Publisher,
    pending: &mut BTreeMap<u8, Pending>,
    peer: Identity,
    source: (u16, u16),
    frame: Frame<'_>,
) -> Result<()> {
    let (short, cluster) = source;
    if frame.header.direction != Direction::ServerToClient || frame.header.manufacturer.is_some() {
        return Ok(());
    }
    let seq = frame.header.sequence;
    let op = pending.get(&seq).and_then(|p| match p.read {
        Some(Read::Hvac(op)) if (p.peer, p.short, p.cluster) == (peer, short, cluster) => Some(op),
        _ => None,
    });
    let pair_op = pending.get(&seq).and_then(|p| match p.read {
        Some(Read::Setpoints(op)) if (p.peer, p.short, p.cluster) == (peer, short, cluster) => {
            Some(op)
        }
        _ => None,
    });
    let b = devices.devices.get_mut(&peer).expect("matched endpoint");
    let values = match Payload::decode(frame) {
        Ok(Payload::ReportAttributes(values)) => values
            .into_iter()
            .map(|r| (r.id, r.value))
            .collect::<Vec<_>>(),
        Ok(Payload::ReadAttributesResponse(records))
            if matches!(pair_op, Some(SetpointOperation::Readback)) =>
        {
            if records.len() != 2
                || ![0x11, 0x12]
                    .iter()
                    .all(|id| records.iter().any(|r| r.id == *id))
            {
                return Ok(());
            }
            pending.remove(&seq);
            records
                .into_iter()
                .filter_map(|r| match r.result {
                    Status::Success(v) => Some((r.id, v)),
                    _ => None,
                })
                .collect()
        }
        Ok(payload)
            if matches!(
                (&payload, pair_op),
                (
                    Payload::ReadAttributesResponse(_),
                    Some(SetpointOperation::Preflight(_))
                ) | (
                    Payload::WriteAttributesResponse(_),
                    Some(SetpointOperation::Write(_))
                )
            ) =>
        {
            let Some((destination, route_epoch)) = pending.remove(&seq).map(|p|(p.destination,p.route_epoch)) else {return Ok(());};
            let Some(tag) = super::ember::available_tag(link,pending,&destination,b.descriptor.profile,cluster,(1,peer.1),seq.wrapping_add(1)) else {
                return Ok(());
            };
            let (req, next) = match (payload, pair_op) {
                (
                    Payload::ReadAttributesResponse(rows),
                    Some(SetpointOperation::Preflight(proposed)),
                ) => match proposed.validate(peer.1, tag, &rows) {
                    Ok(req) => (req, SetpointOperation::Write(proposed)),
                    Err(e) => {
                        eprintln!("Thermostat setpoint request declined: {e}");
                        return Ok(());
                    }
                },
                (Payload::WriteAttributesResponse(WriteStatus::Success), _) => (
                    Setpoints::readback(peer.1, tag)?,
                    SetpointOperation::Readback,
                ),
                _ => return Ok(()),
            };
            if super::zdo::still_assigned(link, &destination).await?
                && super::zdo::submit_unicast(
                    link,
                    &destination,
                    b.descriptor.profile,
                    cluster,
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
                        cluster,
                        read: Some(Read::Setpoints(next)),
                        until: Instant::now() + std::time::Duration::from_secs(8),
                    },
                );
            }
            return Ok(());
        }
        Ok(Payload::ReadAttributesResponse(records)) => {
            let Some(Operation {
                property,
                write: false,
            }) = op
            else {
                return Ok(());
            };
            let [record] = records.as_slice() else {
                return Ok(());
            };
            if record.id != property.address().1 {
                return Ok(());
            }
            pending.remove(&seq);
            match record.result {
                Status::Success(value) => vec![(record.id, value)],
                _ => return Ok(()),
            }
        }
        Ok(Payload::WriteAttributesResponse(status)) => {
            let Some(Operation {
                property,
                write: true,
            }) = op
            else {
                return Ok(());
            };
            match status {
                WriteStatus::Success => {
                    let request = pending.get_mut(&seq).expect("matched HVAC write");
                    request.read = Some(Read::Refresh(property.name()));
                    request.until = Instant::now();
                }
                WriteStatus::Failures(f) if f.len() == 1 && f[0].key == property.address().1 => {
                    pending.remove(&seq);
                }
                _ => {}
            }
            return Ok(());
        }
        _ => return Ok(()),
    };
    // Apply a complete report's capability updates before interpreting values.
    // A changed mode sequence or tightened limit affects the very next write.
    let before = b.hvac.clone();
    for &(id, value) in &values {
        b.hvac.record(cluster, id, value);
    }
    if b.hvac != before {
        let ep = b.discovery(peer);
        let old = features(&before);
        let new = features(&b.hvac);
        // Domain changes (such as a measurement gaining confirmed bounds)
        // retract only the old discovery records before installing the new ones.
        for f in old {
            if !new.iter().any(|n| key(*n) == key(f)) {
                submitted(out.remove_discovery(out.discovery_prefix(), &ep, f))?;
            }
        }
        for f in new {
            submitted(out.discover(out.discovery_prefix(), &ep, f))?;
        }
    }
    for (id, v) in values {
        if let Some(p) = PROPERTIES
            .into_iter()
            .find(|p| p.address() == (cluster, id))
            && b.hvac.readable(p)
            && let Some(value) = p.confirmed(v)
        {
            submitted(out.state(&b.name, p.name(), value))?;
        }
    }
    Ok(())
}

fn key(feature: Feature) -> (std::mem::Discriminant<Feature>, &'static str) {
    let name = match feature {
        Feature::Numeric { property, .. } | Feature::Measurement { property, .. }
        | Feature::Number { property, .. }
        | Feature::Select { property, .. }
        | Feature::TextSensor(property) => property,
        Feature::Thermostat { .. } => "climate",
        _ => unreachable!("HVAC features only"),
    };
    (std::mem::discriminant(&feature), name)
}
