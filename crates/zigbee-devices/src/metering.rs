//! Electrical Measurement and Metering, Zigbee Cluster Library 07-5123 R8,
//! §§4.9.2.2.6.10, 4.9.2.2.7.5–6, 10.4.2.2.1.1 and 10.4.2.2.4.1–3.
//! One decoder belongs to one verified device membership and endpoint. Drop
//! it on membership change; callers supply only observations from that scope.

use crate::{Endpoint, Fingerprint};
use anyhow::{Result, ensure};
use core::num::NonZeroU32;
use serde::Deserialize;
use zigbee_zcl::value::Value;

pub const ELECTRICAL_MEASUREMENT: u16 = 0x0b04;
pub const METERING: u16 = 0x0702;
pub const ELECTRICAL_PROBE: &[u16] = &[
    0x0600, 0x0601, 0x0505, 0x0602, 0x0603, 0x0508, 0x0604, 0x0605, 0x050b,
];
pub const METERING_PROBE: &[u16] = &[0x0300, 0x0301, 0x0302, 0, 0x0400];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unit {
    Watts,
    Volts,
    Amperes,
    KilowattHours,
}

/// Exact scaled measurement, including quantities larger than u64 after multiplication.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Measurement {
    pub unit: Unit,
    pub numerator: i128,
    pub denominator: NonZeroU32,
}

#[derive(Default)]
struct Scale {
    multiplier: Option<u32>,
    divisor: Option<NonZeroU32>,
}

impl Scale {
    fn apply(&self, unit: Unit, raw: i128) -> Option<Measurement> {
        Some(Measurement {
            unit,
            numerator: raw * i128::from(self.multiplier?),
            denominator: self.divisor?,
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MeterProfile {
    source: String,
    fingerprint: Fingerprint,
    summation_manufacturer_code: u16,
}

pub struct MeterProfiles(Vec<MeterProfile>);

impl MeterProfiles {
    pub fn builtin() -> Result<Self> {
        let profiles: Vec<MeterProfile> =
            serde_json::from_str(include_str!("../profiles/metering.json"))?;
        for profile in &profiles {
            profile.fingerprint.validate()?;
            ensure!(
                !profile.source.is_empty(),
                "missing meter profile provenance"
            );
        }
        Ok(Self(profiles))
    }

    fn resolve(
        &self,
        endpoint: &Endpoint<'_>,
        manufacturer: &str,
        model: &str,
    ) -> Option<&MeterProfile> {
        let mut matches = self
            .0
            .iter()
            .filter(|profile| profile.fingerprint.matches(manufacturer, model, endpoint));
        let first = matches.next()?;
        matches.next().is_none().then_some(first)
    }
}

pub struct Meter {
    power_server: bool,
    energy_server: bool,
    summation_manufacturer_code: Option<u16>,
    power_scale: Scale,
    voltage_scale: Scale,
    current_scale: Scale,
    energy_scale: Scale,
    binary_kwh: bool,
}

impl Meter {
    /// Manufacturer, model and server roles come from the same interviewed device.
    pub fn new(
        profiles: &MeterProfiles,
        endpoint: &Endpoint<'_>,
        manufacturer: &str,
        model: &str,
    ) -> Option<Self> {
        let profile = profiles.resolve(endpoint, manufacturer, model);
        let server = |cluster| {
            endpoint.has_ha_server(cluster)
                || (profile.is_some() && endpoint.servers.contains(&cluster))
        };
        Self::from_servers(
            server(ELECTRICAL_MEASUREMENT),
            server(METERING),
            profile.map(|p| p.summation_manufacturer_code),
        )
    }

    /// Generic conversion also needs typed scale and reading evidence from this endpoint.
    pub fn standard(endpoint: &Endpoint<'_>) -> Option<Self> {
        if !(1..=240).contains(&endpoint.id) || !matches!(endpoint.profile, 0x0104 | 0xc05e) {
            return None;
        }
        Self::from_servers(
            endpoint.servers.contains(&ELECTRICAL_MEASUREMENT),
            endpoint.servers.contains(&METERING),
            None,
        )
    }

    fn from_servers(
        power_server: bool,
        energy_server: bool,
        summation_manufacturer_code: Option<u16>,
    ) -> Option<Self> {
        (power_server || energy_server).then_some(Self {
            power_server,
            energy_server,
            summation_manufacturer_code,
            power_scale: Scale::default(),
            voltage_scale: Scale::default(),
            current_scale: Scale::default(),
            energy_scale: Scale::default(),
            binary_kwh: false,
        })
    }

    /// The endpoint advertises the Electrical Measurement server. Individual
    /// measurements still require a typed ActivePower value and observed scale.
    pub(crate) fn reset(&mut self) {
        self.power_scale = Scale::default();
        self.voltage_scale = Scale::default();
        self.current_scale = Scale::default();
        self.energy_scale = Scale::default();
        self.binary_kwh = false;
    }

    /// A correlated read requests a fresh, complete ratio for this quantity.
    pub(crate) fn forget_formatting(&mut self, cluster: u16, sample: u16) {
        match (cluster, sample) {
            (ELECTRICAL_MEASUREMENT, 0x0505) => self.voltage_scale = Scale::default(),
            (ELECTRICAL_MEASUREMENT, 0x0508) => self.current_scale = Scale::default(),
            (ELECTRICAL_MEASUREMENT, 0x050b) => self.power_scale = Scale::default(),
            (METERING, 0 | 0x0400) => {
                self.energy_scale = Scale::default();
                self.binary_kwh = false;
            }
            _ => {}
        }
    }

    pub fn supports_watts(&self) -> bool {
        self.power_server
    }

    /// The endpoint advertises the Metering server. Conversion to kWh also
    /// requires an observed binary electricity unit and both scale attributes.
    pub fn supports_delivered_kwh(&self) -> bool {
        self.energy_server
    }

    /// Apply a single observed attribute. Formatting updates affect subsequent
    /// measurements. An invalid formatting update clears the old interpretation.
    pub fn observe(
        &mut self,
        cluster: u16,
        attribute: u16,
        manufacturer_code: Option<u16>,
        value: Value<'_>,
    ) -> Option<Measurement> {
        if manufacturer_code.is_some()
            && !(manufacturer_code == self.summation_manufacturer_code
                && (cluster, attribute) == (METERING, 0))
        {
            return None;
        }
        match (cluster, attribute) {
            (ELECTRICAL_MEASUREMENT, attribute) if self.power_server => match attribute {
                0x0600 => self.voltage_scale.multiplier = power_factor(value).map(NonZeroU32::get),
                0x0601 => self.voltage_scale.divisor = power_factor(value),
                0x0602 => self.current_scale.multiplier = power_factor(value).map(NonZeroU32::get),
                0x0603 => self.current_scale.divisor = power_factor(value),
                id @ (0x0505 | 0x0508) => {
                    let raw = u16::from_le_bytes(bytes(value, 0x21)?);
                    if raw != u16::MAX {
                        let (scale, unit) = if id == 0x0505 {
                            (&self.voltage_scale, Unit::Volts)
                        } else {
                            (&self.current_scale, Unit::Amperes)
                        };
                        return scale.apply(unit, raw.into());
                    }
                }
                0x0604 => {
                    self.power_scale.multiplier = power_factor(value).map(NonZeroU32::get);
                }
                0x0605 => self.power_scale.divisor = power_factor(value),
                0x050b => {
                    let raw = i16::from_le_bytes(bytes(value, 0x29)?);
                    if raw != i16::MIN {
                        return self.power_scale.apply(Unit::Watts, raw.into());
                    }
                }
                _ => {}
            },
            (METERING, attribute) if self.energy_server => match attribute {
                0x0300 => self.binary_kwh = bytes(value, 0x30) == Some([0x00]),
                0x0301 => self.energy_scale.multiplier = uint24(value),
                0x0302 => self.energy_scale.divisor = uint24(value).and_then(NonZeroU32::new),
                0x0400 if self.binary_kwh => {
                    // R8 Tables 10-72 and 10-75: binary kW, signed 24-bit, excluding the minimum.
                    let [a, b, c] = bytes(value, 0x2a)?;
                    let demand =
                        i32::from_le_bytes([a, b, c, if c & 0x80 == 0 { 0 } else { 0xff }]);
                    if demand != -8_388_608 {
                        return self
                            .energy_scale
                            .apply(Unit::Watts, i128::from(demand) * 1000);
                    }
                }
                0x0000 if self.binary_kwh => {
                    // R8 Table 10-57 explicitly assigns the full uint48 range.
                    let [a, b, c, d, e, f] = bytes(value, 0x25)?;
                    let raw = u64::from_le_bytes([a, b, c, d, e, f, 0, 0]);
                    return self.energy_scale.apply(Unit::KilowattHours, raw.into());
                }
                _ => {}
            },
            _ => {}
        }
        None
    }
}

fn bytes<const N: usize>(value: Value<'_>, data_type: u8) -> Option<[u8; N]> {
    if value.data_type().id() == data_type {
        value.as_bytes().try_into().ok()
    } else {
        None
    }
}

fn power_factor(value: Value<'_>) -> Option<NonZeroU32> {
    NonZeroU32::new(u16::from_le_bytes(bytes(value, 0x21)?).into())
}

fn uint24(value: Value<'_>) -> Option<u32> {
    let [a, b, c] = bytes(value, 0x22)?;
    Some(u32::from_le_bytes([a, b, c, 0]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value(kind: u8, data: &[u8]) -> Value<'_> {
        Value::from_bytes(kind.try_into().unwrap(), data).unwrap()
    }
    fn endpoint(profile: u16) -> Endpoint<'static> {
        Endpoint {
            id: 1,
            profile,
            servers: &[ELECTRICAL_MEASUREMENT, METERING],
            clients: &[],
        }
    }

    #[test]
    fn active_power_needs_both_live_factors_and_keeps_the_sign() {
        let mut meter = Meter::new(
            &MeterProfiles::builtin().unwrap(),
            &endpoint(0x0104),
            "",
            "",
        )
        .unwrap();
        let c = ELECTRICAL_MEASUREMENT;
        let raw = value(0x29, &[0xf9, 0xff]); // -7 W before scaling
        assert_eq!(meter.observe(c, 0x050b, None, raw), None);
        meter.observe(c, 0x0604, None, value(0x21, &[3, 0]));
        meter.observe(c, 0x0605, None, value(0x21, &[2, 0]));
        let negative = meter.observe(c, 0x050b, None, raw).unwrap();
        assert_eq!(
            (
                negative.unit,
                negative.numerator,
                negative.denominator.get()
            ),
            (Unit::Watts, -21, 2)
        );
        assert_eq!(
            meter.observe(c, 0x050b, None, value(0x29, &[0, 0x80])),
            None
        );
        meter.observe(c, 0x0605, None, value(0x21, &[0, 0]));
        assert_eq!(meter.observe(c, 0x050b, None, raw), None);
    }

    #[test]
    fn delivered_energy_requires_binary_electric_units_and_preserves_all_72_bits() {
        let mut meter = Meter::new(
            &MeterProfiles::builtin().unwrap(),
            &endpoint(0xc05e),
            "innr",
            "SP 120",
        )
        .unwrap();
        let c = METERING;
        let raw = value(0x25, &[255; 6]);
        meter.observe(c, 0x0301, None, value(0x22, &[255; 3]));
        meter.observe(c, 0x0302, None, value(0x22, &[3, 0, 0]));
        assert_eq!(meter.observe(c, 0, None, raw), None);
        meter.observe(c, 0x0300, None, value(0x30, &[0]));
        assert_eq!(meter.observe(c, 0, Some(0x1234), raw), None);
        let reading = meter.observe(c, 0, Some(0x1166), raw).unwrap();
        assert_eq!(
            (reading.unit, reading.numerator, reading.denominator.get()),
            (Unit::KilowattHours, 0xffffffffffff * 0xffffff_i128, 3)
        );
        for other in [1, 0x80, 0xff] {
            meter.observe(c, 0x0300, None, value(0x30, &[other]));
            assert_eq!(meter.observe(c, 0, None, raw), None);
        }
    }

    #[test]
    fn ambiguous_meter_profile_keeps_the_extension_disabled() {
        let mut profiles = MeterProfiles::builtin().unwrap();
        profiles.0.extend(MeterProfiles::builtin().unwrap().0);
        assert!(Meter::new(&profiles, &endpoint(0xc05e), "innr", "SP 120").is_none());
    }

    #[test]
    fn client_only_and_unqualified_zll_endpoints_create_no_meter() {
        let remote = Endpoint {
            servers: &[],
            clients: &[METERING],
            ..endpoint(0x0104)
        };
        assert!(Meter::new(&MeterProfiles::builtin().unwrap(), &remote, "", "").is_none());
        assert!(
            Meter::new(
                &MeterProfiles::builtin().unwrap(),
                &endpoint(0xc05e),
                "other",
                "SP 120"
            )
            .is_none()
        );
    }
}
