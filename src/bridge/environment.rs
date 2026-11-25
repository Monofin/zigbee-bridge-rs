//! Named environmental reads, reports and confirmed illuminance-target updates.
use super::{
    Binding, Identity, Read,
    ember::{Pending, submitted},
};
use crate::mqtt::{
    Publisher,
    discovery::{Feature, Sensor},
};
use anyhow::Result;
use std::collections::BTreeMap;
use tokio::time::Instant;
use zigbee_devices::measurement::environment::{Reading, TARGET};
use zigbee_zcl::{
    Direction, Frame,
    attributes::{Status, WriteStatus},
    foundation::{GeneralCommand, Payload},
};

pub(super) fn feature(reading: Reading) -> Feature {
    match reading.property {
        "flow" => Feature::Numeric {
            property: reading.property,
            unit: reading.unit,
            device_class: Some("volume_flow_rate"),
            state_class: Some("measurement"),
        },
        "occupancy" => Feature::Binary("occupancy", "occupancy"),
        "illuminance" => Feature::Sensor(Sensor::Illuminance),
        "soil_moisture" => Feature::Sensor(Sensor::SoilMoisture),
        "illuminance_level" => Feature::TextSensor(reading.property),
        "illuminance_target" => Feature::Number {
            property: reading.property,
            min: 1.0,
            max: 10_f64.powf(6.5534).floor(),
            step: 1.0,
            unit: Some("lx"),
        },
        _ => Feature::Measurement {
            property: reading.property,
            unit: reading.unit,
        },
    }
}

pub(super) fn receive(
    b: &Binding,
    out: &Publisher,
    pending: &mut BTreeMap<u8, Pending>,
    peer: Identity,
    short: u16,
    cluster: u16,
    frame: Frame<'_>,
) -> Result<bool> {
    if frame.header.manufacturer.is_some() || frame.header.direction != Direction::ServerToClient {
        return Ok(false);
    }
    let seq = frame.header.sequence;
    let requested = pending.get(&seq).and_then(|p| match p.read {
        Some(Read::Environment(r, write))
            if (p.peer, p.short, p.cluster) == (peer, short, cluster) =>
        {
            Some((r, write))
        }
        _ => None,
    });
    // An endpoint can advertise several sensor clusters and readbacks must stay
    // bound to the attribute and current membership of the requesting operation.
    let matching = |id| {
        b.environment
            .iter()
            .copied()
            .find(|r| (r.cluster, r.attribute) == (cluster, id))
    };
    let publish = |reading: Reading, raw| -> Result<()> {
        if let Some(value) = reading.value(raw) {
            submitted(out.state(&b.name, reading.property, value))?;
        }
        Ok(())
    };
    match frame.header.general_command() {
        Some(GeneralCommand::ReportAttributes) => {
            let Ok(Payload::ReportAttributes(rows)) = Payload::decode(frame) else {
                return Ok(false);
            };
            let mut recognized = false;
            for row in rows {
                if let Some(r) = matching(row.id) {
                    if row.value.data_type().id() != r.wire_type() {
                        continue;
                    }
                    recognized = true;
                    publish(r, row.value)?;
                }
            }
            Ok(recognized)
        }
        Some(GeneralCommand::ReadAttributesResponse) => {
            let Some((r, false)) = requested else {
                return Ok(false);
            };
            let Ok(Payload::ReadAttributesResponse(rows)) = Payload::decode(frame) else {
                return Ok(false);
            };
            let mut found = rows.into_iter().filter(|row| row.id == r.attribute);
            let Some(row) = found.next() else {
                return Ok(false);
            };
            if found.next().is_some() {
                return Ok(false);
            }
            if let Status::Success(v) = row.result {
                if v.data_type().id() != r.wire_type() {
                    return Ok(false);
                }
                publish(r, v)?;
            }
            pending.remove(&seq);
            Ok(true)
        }
        Some(GeneralCommand::WriteAttributesResponse) if requested == Some((TARGET, true)) => {
            let Ok(Payload::WriteAttributesResponse(status)) = Payload::decode(frame) else {
                return Ok(false);
            };
            match status {
                WriteStatus::Success => {
                    let p = pending.get_mut(&seq).expect("matched target write");
                    p.read = Some(Read::Refresh(TARGET.property));
                    p.until = Instant::now();
                }
                WriteStatus::Failures(rows) if rows.iter().any(|r| r.key == TARGET.attribute) => {
                    pending.remove(&seq);
                }
                _ => return Ok(false),
            }
            Ok(true)
        }
        _ => Ok(false),
    }
}
