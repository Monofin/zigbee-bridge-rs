//! R8 §§4.2–4.3, 4.6–4.8, 4.10–4.13. Only standard server attributes are mapped.
use crate::{Endpoint, input::Reading as InputReading, lighting::Request};
use anyhow::{Result, ensure};
use serde_json::Value as Json;
use zigbee_zcl::{
    Direction, Frame, FrameType, Header, attributes::AttributeValue, foundation::Payload,
    value::Value,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Encoding {
    Lux,
    Level,
    LuxTarget,
    Occupancy,
    Tenths,
    PercentHundredths,
    PhHundredths,
    Hundredths,
    FractionPpm,
    FractionPercent,
}

/// Also describes the conversion applied to both reads and reports.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Reading {
    pub cluster: u16,
    pub attribute: u16,
    pub property: &'static str,
    pub unit: Option<&'static str>,
    encoding: Encoding,
}

const fn reading(
    cluster: u16,
    property: &'static str,
    unit: Option<&'static str>,
    encoding: Encoding,
) -> Reading {
    Reading {
        cluster,
        attribute: 0,
        property,
        unit,
        encoding,
    }
}

pub const TARGET: Reading = Reading {
    attribute: 0x0010,
    ..reading(
        0x0401,
        "illuminance_target",
        Some("lx"),
        Encoding::LuxTarget,
    )
};

const MEASUREMENTS: &[Reading] = &[
    reading(0x0400, "illuminance", Some("lx"), Encoding::Lux),
    reading(0x0401, "illuminance_level", None, Encoding::Level),
    TARGET,
    reading(0x0404, "flow", Some("m³/h"), Encoding::Tenths),
    reading(0x0406, "occupancy", None, Encoding::Occupancy),
    reading(
        0x0407,
        "leaf_wetness",
        Some("%"),
        Encoding::PercentHundredths,
    ),
    reading(
        0x0408,
        "soil_moisture",
        Some("%"),
        Encoding::PercentHundredths,
    ),
    reading(0x0409, "ph", None, Encoding::PhHundredths),
    reading(
        0x040a,
        "electrical_conductivity",
        Some("mS/m"),
        Encoding::Tenths,
    ),
    reading(0x040b, "wind_speed", Some("m/s"), Encoding::Hundredths),
];

// R8 §4.13.1.3. All measured values are fractions in [0,1]. Mass and volume
// fractions use the basis defined by that substance's cluster. PM2.5 here is
// explicitly volume-based: no mass concentration can be inferred from it.
const SUBSTANCES: [&str; 32] = [
    "carbon_monoxide",
    "carbon_dioxide",
    "ethylene",
    "ethylene_oxide",
    "hydrogen",
    "hydrogen_sulfide",
    "nitric_oxide",
    "nitrogen_dioxide",
    "oxygen",
    "ozone",
    "sulfur_dioxide",
    "dissolved_oxygen",
    "bromate",
    "chloramines",
    "chlorine",
    "fecal_coliform",
    "fluoride",
    "haloacetic_acids",
    "total_trihalomethanes",
    "total_coliform_bacteria",
    "turbidity",
    "copper",
    "lead",
    "manganese",
    "sulfate",
    "bromodichloromethane",
    "bromoform",
    "chlorodibromomethane",
    "chloroform",
    "sodium",
    "pm25_volume_percent",
    "formaldehyde",
];

pub fn for_endpoint<'a>(ep: &Endpoint<'a>) -> impl Iterator<Item = Reading> + use<'a> {
    let ep = Endpoint { ..*ep };
    let concentrations = SUBSTANCES.iter().enumerate().map(|(i, name)| {
        let cluster = 0x040c + i as u16;
        if matches!(cluster, 0x040c | 0x040d) {
            reading(cluster, name, Some("ppm"), Encoding::FractionPpm)
        } else {
            reading(cluster, name, Some("%"), Encoding::FractionPercent)
        }
    });
    MEASUREMENTS
        .iter()
        .copied()
        .chain(concentrations)
        .filter(move |r| ep.has_standard_server(r.cluster))
}

impl Reading {
    pub fn wire_type(self) -> u8 {
        match self.encoding {
            Encoding::Occupancy => 0x18,
            Encoding::Level => 0x30,
            Encoding::FractionPercent | Encoding::FractionPpm => 0x39,
            _ => 0x21,
        }
    }

    pub fn value(self, value: Value<'_>) -> Option<Json> {
        if value.data_type().id() != self.wire_type() {
            return None;
        }
        let n = || u16::from_le_bytes(value.as_bytes().try_into().expect("typed uint16"));
        match self.encoding {
            Encoding::Occupancy => InputReading::Occupancy.observe(value).map(|(_, v)| v),
            Encoding::Lux => InputReading::Illuminance.observe(value).map(|(_, v)| v),
            Encoding::Level => ["on_target", "below_target", "above_target"]
                .get(usize::from(value.as_bytes()[0]))
                .map(|s| (*s).into()),
            Encoding::FractionPpm | Encoding::FractionPercent => {
                let v = f32::from_le_bytes(value.as_bytes().try_into().ok()?);
                let scale = if self.encoding == Encoding::FractionPpm {
                    1e6
                } else {
                    100.0
                };
                (0.0..=1.0)
                    .contains(&v)
                    .then(|| (f64::from(v) * scale).into())
            }
            Encoding::LuxTarget if n() != u16::MAX => {
                Some(10_f64.powf(f64::from(n()) / 10_000.0).into())
            }
            Encoding::Tenths if n() != u16::MAX => Some((f64::from(n()) / 10.0).into()),
            Encoding::Hundredths if n() != u16::MAX => Some((f64::from(n()) / 100.0).into()),
            Encoding::PercentHundredths if n() <= 10_000 => Some((f64::from(n()) / 100.0).into()),
            Encoding::PhHundredths if n() <= 1400 => Some((f64::from(n()) / 100.0).into()),
            _ => None,
        }
    }

    pub fn request(self, endpoint: u8, seq: u8, write: Option<&Json>) -> Result<Request> {
        let raw;
        let payload = if let Some(v) = write {
            ensure!(self == TARGET, "environmental measurement is read-only");
            let lux = v
                .as_f64()
                .filter(|n| (1.0..=10_f64.powf(6.5534)).contains(n))
                .ok_or_else(|| anyhow::anyhow!("target illuminance outside supported lux range"))?;
            raw = ((lux.log10() * 10_000.0).round() as u16).to_le_bytes();
            Payload::WriteAttributes(vec![AttributeValue {
                id: self.attribute,
                value: Value::from_bytes(self.wire_type().try_into()?, &raw)?,
            }])
        } else {
            Payload::ReadAttributes(vec![self.attribute])
        };
        let body = payload.encode()?;
        let mut frame = Vec::new();
        Frame {
            header: Header {
                frame_type: FrameType::General,
                manufacturer: None,
                direction: Direction::ClientToServer,
                disable_default_response: true,
                sequence: seq,
                command_id: payload.command() as u8,
            },
            payload: &body,
        }
        .encode_into(&mut frame);
        Ok(Request {
            endpoint,
            cluster: self.cluster,
            frame,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reading_for(cluster: u16) -> Reading {
        for_endpoint(&Endpoint {
            id: 3,
            profile: 260,
            servers: &[cluster],
            clients: &[],
        })
        .next()
        .unwrap()
    }

    #[test]
    fn units_and_invalid_wire_values() {
        for (cluster, raw, expected) in [
            (0x0400, 10001, 10.0),
            (0x0404, 73, 7.3),
            (0x0407, 7900, 79.0),
            (0x0408, 4321, 43.21),
            (0x0409, 714, 7.14),
            (0x040a, 13, 1.3),
            (0x040b, 222, 2.22),
        ] {
            let r = reading_for(cluster);
            let decode = |n: u16| {
                r.value(Value::from_bytes(0x21.try_into().unwrap(), &n.to_le_bytes()).unwrap())
            };
            assert_eq!(decode(raw), Some(expected.into()));
            assert_eq!(decode(0xffff), None);
        }
        for (cluster, raw) in [(0x0407, 10001u16), (0x0408, 10001), (0x0409, 1401)] {
            assert_eq!(
                reading_for(cluster).value(
                    Value::from_bytes(0x21.try_into().unwrap(), &raw.to_le_bytes()).unwrap()
                ),
                None
            );
        }
        for cluster in 0x040c..=0x042b {
            let r = reading_for(cluster);
            let decode = |n: f32| {
                r.value(Value::from_bytes(0x39.try_into().unwrap(), &n.to_le_bytes()).unwrap())
            };
            assert_eq!(
                decode(0.125),
                Some(if cluster <= 0x040d { 125000.0 } else { 12.5 }.into())
            );
            for invalid in [f32::NAN, f32::INFINITY, -0.1, 1.1] {
                assert_eq!(decode(invalid), None);
            }
        }
        let occ = reading_for(0x0406);
        assert_eq!(
            occ.value(Value::from_bytes(0x18.try_into().unwrap(), &[1]).unwrap()),
            Some(true.into())
        );
        assert_eq!(
            occ.value(Value::from_bytes(0x20.try_into().unwrap(), &[1]).unwrap()),
            None
        );
        let level = reading_for(0x0401);
        assert_eq!(
            level.value(Value::from_bytes(0x30.try_into().unwrap(), &[2]).unwrap()),
            Some("above_target".into())
        );
        assert_eq!(
            level.value(Value::from_bytes(0x30.try_into().unwrap(), &[3]).unwrap()),
            None
        );
    }

    #[test]
    fn catalogued_types_roles_and_target_encoding() -> Result<()> {
        let clusters: Vec<_> = (0x0400..=0x042b).collect();
        for profile in [0x0104, 0xc05e] {
            let ep = Endpoint {
                id: 1,
                profile,
                servers: &clusters,
                clients: &[],
            };
            let readings: Vec<_> = for_endpoint(&ep).collect();
            assert_eq!(readings.len(), 42);
            for r in readings {
                assert_eq!(
                    zigbee_zcl::standard_attributes::wire_type(r.cluster, r.attribute),
                    Some(r.wire_type())
                );
                assert_eq!(
                    r.request(4, 1, None)?.frame,
                    [0x10, 1, 0, r.attribute as u8, 0]
                );
                if r != TARGET {
                    assert!(r.request(4, 1, Some(&0.into())).is_err());
                }
            }
        }
        for (profile, id) in [(0x0104, 0), (0x0104, 242), (0x0109, 1)] {
            assert_eq!(
                for_endpoint(&Endpoint {
                    id,
                    profile,
                    servers: &clusters,
                    clients: &[]
                })
                .count(),
                0
            );
        }
        assert_eq!(
            for_endpoint(&Endpoint {
                id: 1,
                profile: 260,
                servers: &[],
                clients: &clusters
            })
            .count(),
            0
        );
        assert_eq!(
            TARGET.request(1, 2, Some(&10.into()))?.frame,
            [0x10, 2, 2, 0x10, 0, 0x21, 0x10, 0x27]
        );
        for n in [0.0, -1.0, 4e6] {
            assert!(TARGET.request(1, 2, Some(&n.into())).is_err());
        }
        Ok(())
    }
}
