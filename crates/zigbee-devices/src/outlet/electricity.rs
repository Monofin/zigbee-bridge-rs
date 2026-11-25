//! Product-selected meter access and scaling. A decoder is owned by one active
//! device membership. A missing ratio or relay state leaves affected values unknown.
use crate::{
    Endpoint,
    lighting::Request,
    metering::{ELECTRICAL_MEASUREMENT, METERING, Measurement, Meter, MeterProfiles, Unit},
};
use anyhow::{Result, ensure};
use serde::Deserialize;
use std::num::NonZeroU32;
use zigbee_zcl::{Direction, Frame, FrameType, Header, foundation::Payload, value::Value};

const SONOFF: u16 = 0xfc11;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MeterKind {
    Standard,
    Innr,
    Sonoff,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reading {
    Power,
    Voltage,
    Current,
    Energy,
    Demand,
}
impl Reading {
    pub const ALL: [Self; 4] = [Self::Power, Self::Voltage, Self::Current, Self::Energy];
    pub fn property(self) -> &'static str {
        match self {
            Self::Power | Self::Demand => "power",
            Self::Voltage => "voltage",
            Self::Current => "current",
            Self::Energy => "energy",
        }
    }
}

impl MeterKind {
    fn address(self, r: Reading) -> (u16, u16) {
        match (self, r) {
            (_, Reading::Energy) => (METERING, 0),
            (_, Reading::Demand) => (METERING, 0x0400),
            (Self::Sonoff, Reading::Current) => (SONOFF, 0x7004),
            (Self::Sonoff, Reading::Voltage) => (SONOFF, 0x7005),
            (Self::Sonoff, Reading::Power) => (SONOFF, 0x7006),
            (_, Reading::Voltage) => (ELECTRICAL_MEASUREMENT, 0x0505),
            (_, Reading::Current) => (ELECTRICAL_MEASUREMENT, 0x0508),
            (_, Reading::Power) => (ELECTRICAL_MEASUREMENT, 0x050b),
        }
    }
    pub fn handles(self, cluster: u16) -> bool {
        [Reading::Energy, Reading::Power]
            .iter()
            .any(|r| self.address(*r).0 == cluster)
    }
    pub fn read(self, endpoint: u8, r: Reading, sequence: u8) -> Result<Request> {
        ensure!(
            self != Self::Sonoff || r != Reading::Energy,
            "device declares report-only energy"
        );
        let (cluster, attr) = self.address(r);
        let mut attrs = match (self, r) {
            (Self::Standard, Reading::Power) => vec![0x0604, 0x0605],
            (Self::Standard, Reading::Current) => vec![0x0602, 0x0603],
            (Self::Standard, Reading::Voltage) => vec![0x0600, 0x0601],
            (_, Reading::Energy | Reading::Demand) => vec![0x0300, 0x0301, 0x0302],
            _ => Vec::new(),
        };
        attrs.push(attr);
        let payload = Payload::ReadAttributes(attrs).encode()?;
        let mut frame = Vec::new();
        Frame {
            header: Header {
                frame_type: FrameType::General,
                manufacturer: None,
                direction: Direction::ClientToServer,
                disable_default_response: true,
                sequence,
                command_id: 0,
            },
            payload: &payload,
        }
        .encode_into(&mut frame);
        Ok(Request {
            endpoint,
            cluster,
            frame,
        })
    }
    pub fn matches(self, r: Reading, cluster: u16, attr: u16) -> bool {
        self.address(r) == (cluster, attr)
    }
    pub fn requested(self, r: Reading, cluster: u16, attr: u16) -> bool {
        if self.matches(r, cluster, attr) {
            return true;
        }
        matches!(
            (self, r, cluster, attr),
            (
                _,
                Reading::Energy | Reading::Demand,
                METERING,
                0x0300..=0x0302
            ) | (
                Self::Standard,
                Reading::Current,
                ELECTRICAL_MEASUREMENT,
                0x0602..=0x0603
            ) | (
                Self::Standard,
                Reading::Voltage,
                ELECTRICAL_MEASUREMENT,
                0x0600..=0x0601
            ) | (
                Self::Standard,
                Reading::Power,
                ELECTRICAL_MEASUREMENT,
                0x0604..=0x0605
            )
        )
    }
    pub fn scale_attribute(self, cluster: u16, attr: u16) -> bool {
        (cluster == METERING && (0x0300..=0x0302).contains(&attr))
            || (self == Self::Standard
                && cluster == ELECTRICAL_MEASUREMENT
                && (0x0600..=0x0605).contains(&attr))
    }
}
pub struct Electricity {
    pub kind: MeterKind,
    meter: Meter,
    relay: Option<bool>,
}
impl Electricity {
    /// Restore only quantities shown to have a usable scale, unit and sample.
    /// Prefer ActivePower when both clusters provide instantaneous power.
    pub fn standard<'a>(
        ep: &Endpoint<'_>,
        values: impl Iterator<Item = (u16, u16, Value<'a>)> + Clone,
    ) -> Option<(Self, Vec<Reading>)> {
        let kind = MeterKind::Standard;
        let mut meter = Meter::standard(ep)?;
        for (cluster, attr, value) in values
            .clone()
            .filter(|(c, a, _)| kind.scale_attribute(*c, *a))
        {
            meter.observe(cluster, attr, None, value);
        }
        let mut supported = Vec::new();
        for reading in Reading::ALL.into_iter().chain([Reading::Demand]) {
            if reading == Reading::Demand && supported.contains(&Reading::Power) {
                continue;
            }
            if values.clone().any(|(c, a, v)| {
                kind.matches(reading, c, a) && meter.observe(c, a, None, v).is_some()
            }) {
                supported.push(reading);
            }
        }
        (!supported.is_empty()).then_some((
            Self {
                kind,
                meter,
                relay: None,
            },
            supported,
        ))
    }

    pub fn new(kind: MeterKind, ep: &Endpoint<'_>, maker: &str, model: &str) -> Result<Self> {
        let meter = Meter::new(&MeterProfiles::builtin()?, ep, maker, model)
            .ok_or_else(|| anyhow::anyhow!("missing meter cluster in reviewed outlet"))?;
        Ok(Self {
            kind,
            meter,
            relay: None,
        })
    }
    pub fn forget_formatting(&mut self, reading: Reading) {
        let (cluster, sample) = self.kind.address(reading);
        self.meter.forget_formatting(cluster, sample);
    }

    pub fn reset(&mut self) {
        self.relay = None;
        self.meter.reset();
    }
    pub fn relay(&mut self, on: bool) {
        self.relay = Some(on);
    }
    pub fn observe(
        &mut self,
        cluster: u16,
        attr: u16,
        mf: Option<u16>,
        value: Value<'_>,
    ) -> Option<Measurement> {
        fn scaled(unit: Unit, raw: i128, div: u32) -> Option<Measurement> {
            Some(Measurement {
                unit,
                numerator: raw,
                denominator: NonZeroU32::new(div)?,
            })
        }
        if self.kind == MeterKind::Sonoff
            && cluster == SONOFF
            && mf.is_none()
            && value.data_type().id() == 0x23
        {
            let raw = u32::from_le_bytes(value.as_bytes().try_into().ok()?);
            return match attr {
                0x7004 | 0x7006 => {
                    let unit = if attr == 0x7004 {
                        Unit::Amperes
                    } else {
                        Unit::Watts
                    };
                    scaled(unit, if self.relay? { raw.into() } else { 0 }, 1000)
                }
                0x7005 => scaled(Unit::Volts, raw.into(), 1000),
                _ => None,
            };
        }
        if self.kind == MeterKind::Innr {
            // This firmware advertises bogus scale factors. The reviewed Z2M
            // definition supplies fixed divisors, also used by the ZHA quirk.
            if cluster == ELECTRICAL_MEASUREMENT && mf.is_none() {
                return match (attr, value.data_type().id(), value.as_bytes()) {
                    (0x505 | 0x508, 0x21, [lo, hi]) => {
                        let n = u16::from_le_bytes([*lo, *hi]);
                        if n == u16::MAX {
                            return None;
                        }
                        if attr == 0x505 {
                            scaled(Unit::Volts, n.into(), 1)
                        } else {
                            scaled(Unit::Amperes, n.into(), 1000)
                        }
                    }
                    (0x50b, 0x29, [lo, hi]) => {
                        let n = i16::from_le_bytes([*lo, *hi]);
                        if n == i16::MIN {
                            return None;
                        }
                        scaled(Unit::Watts, n.into(), 1)
                    }
                    _ => None,
                };
            }
            if cluster == METERING
                && attr == 0
                && (mf.is_none() || mf == Some(0x1166))
                && value.data_type().id() == 0x25
            {
                let [a, b, c, d, e, f] = *value.as_bytes() else {
                    return None;
                };
                return scaled(
                    Unit::KilowattHours,
                    u64::from_le_bytes([a, b, c, d, e, f, 0, 0]).into(),
                    100,
                );
            }
            return None;
        }
        self.meter.observe(cluster, attr, mf, value)
    }
}
