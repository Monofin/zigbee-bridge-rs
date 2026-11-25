//! Home Automation Thermostat and Fan Control server attributes, in wire units.
use crate::{Endpoint, lighting::Request, standard_controls::request};
use anyhow::{Context, Result, bail, ensure};
use serde_json::Value as Json;
use zigbee_zcl::{FrameType, attributes::AttributeValue, foundation::Payload, value::Value};

mod setpoints;
pub use setpoints::Setpoints;

pub const THERMOSTAT: u16 = 0x0201;
pub const FAN: u16 = 0x0202;
pub const THERMOSTAT_PROBE: &[u16] = &[0, 0x11, 0x12, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1b, 0x1c];
pub const FAN_PROBE: &[u16] = &[0, 1];

pub fn thermostat_type(device: u16) -> bool {
    matches!(device, 0x0300 | 0x0301 | 0x0307)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Property {
    LocalTemperature,
    CoolingSetpoint,
    HeatingSetpoint,
    SystemMode,
    FanMode,
}
pub const PROPERTIES: [Property; 5] = [
    Property::LocalTemperature,
    Property::CoolingSetpoint,
    Property::HeatingSetpoint,
    Property::SystemMode,
    Property::FanMode,
];

impl Property {
    pub fn name(self) -> &'static str {
        match self {
            Self::LocalTemperature => "local_temperature",
            Self::CoolingSetpoint => "occupied_cooling_setpoint",
            Self::HeatingSetpoint => "occupied_heating_setpoint",
            Self::SystemMode => "system_mode",
            Self::FanMode => "fan_mode",
        }
    }
    pub fn address(self) -> (u16, u16) {
        match self {
            Self::LocalTemperature => (THERMOSTAT, 0),
            Self::CoolingSetpoint => (THERMOSTAT, 0x11),
            Self::HeatingSetpoint => (THERMOSTAT, 0x12),
            Self::SystemMode => (THERMOSTAT, 0x1c),
            Self::FanMode => (FAN, 0),
        }
    }
    fn ty(self) -> u8 {
        if matches!(self, Self::SystemMode | Self::FanMode) {
            0x30
        } else {
            0x29
        }
    }
    pub fn confirmed(self, wire: Value<'_>) -> Option<Json> {
        if wire.data_type().id() != self.ty() {
            return None;
        }
        Some(match self {
            Self::SystemMode | Self::FanMode => self
                .enum_names()
                .iter()
                .find(|(id, _)| wire.as_bytes() == [*id])?
                .1
                .into(),
            _ => degree_json(raw_temperature(wire)?),
        })
    }
    fn enum_names(self) -> &'static [(u8, &'static str)] {
        match self {
            Self::SystemMode => &[
                (0, "off"),
                (1, "auto"),
                (3, "cool"),
                (4, "heat"),
                (5, "emergency_heating"),
                (6, "precooling"),
                (7, "fan_only"),
                (8, "dry"),
                (9, "sleep"),
            ],
            Self::FanMode => &[
                (0, "off"),
                (1, "low"),
                (2, "medium"),
                (3, "high"),
                (4, "on"),
                (5, "auto"),
                (6, "smart"),
            ],
            _ => &[],
        }
    }
}
fn raw_temperature(v: Value<'_>) -> Option<i16> {
    if v.data_type().id() != 0x29 {
        return None;
    }
    let n = i16::from_le_bytes(v.as_bytes().try_into().ok()?);
    (n >= -27315).then_some(n)
}
fn degree_json(n: i16) -> Json {
    (f64::from(n) / 100.).into()
}
fn sequence(v: Value<'_>, max: u8) -> Option<u8> {
    match *v.as_bytes() {
        [b] if v.data_type().id() == 0x30 && b <= max => Some(b),
        _ => None,
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Capabilities {
    thermostat: bool,
    fan: bool,
    system_sequence: Option<u8>,
    fan_sequence: Option<u8>,
    mode_seen: bool,
    fan_seen: bool,
    local_seen: bool,
    heat_seen: bool,
    cool_seen: bool,
    limits: [Option<i16>; 4],
    deadband: Option<i16>,
}

impl Capabilities {
    pub fn from_descriptor(ep: &Endpoint<'_>, device: u16) -> Self {
        let valid = ep.profile == 0x0104 && (1..=240).contains(&ep.id);
        Self {
            thermostat: valid && thermostat_type(device) && ep.servers.contains(&THERMOSTAT),
            // Fan Control also appears on standalone ceiling fans and light/fan endpoints.
            fan: valid && ep.servers.contains(&FAN),
            ..Self::default()
        }
    }
    pub fn active(&self) -> bool {
        self.thermostat || self.fan
    }
    pub fn has_cluster(&self, cluster: u16) -> bool {
        match cluster {
            THERMOSTAT => self.thermostat,
            FAN => self.fan,
            _ => false,
        }
    }
    pub fn record(&mut self, cluster: u16, id: u16, v: Value<'_>) {
        if !self.has_cluster(cluster) {
            return;
        }
        match (cluster, id) {
            (THERMOSTAT, 0x15..=0x18) => self.limits[usize::from(id - 0x15)] = raw_temperature(v),
            (THERMOSTAT, 0x19) => {
                self.deadband = match v.as_bytes() {
                    [n @ 10..=25] if v.data_type().id() == 0x28 => Some(i16::from(*n) * 10),
                    _ => None,
                }
            }
            (THERMOSTAT, 0x1b) => self.system_sequence = sequence(v, 5),
            (FAN, 1) => self.fan_sequence = sequence(v, 4),
            (THERMOSTAT, 0) => self.local_seen = raw_temperature(v).is_some(),
            (THERMOSTAT, 0x11) => self.cool_seen = raw_temperature(v).is_some(),
            (THERMOSTAT, 0x12) => self.heat_seen = raw_temperature(v).is_some(),
            (THERMOSTAT, 0x1c) => self.mode_seen = Property::SystemMode.confirmed(v).is_some(),
            (FAN, 0) => self.fan_seen = Property::FanMode.confirmed(v).is_some(),
            _ => {}
        }
    }
    pub fn observed(&self, p: Property) -> bool {
        self.readable(p)
            && match p {
                Property::LocalTemperature => self.local_seen,
                Property::SystemMode => self.mode_seen,
                Property::FanMode => self.fan_seen,
                _ => true,
            }
    }
    pub fn readable(&self, p: Property) -> bool {
        if !self.has_cluster(p.address().0) {
            return false;
        }
        match p {
            Property::HeatingSetpoint => {
                self.heat_seen && self.system_sequence.is_some_and(|s| s >= 2)
            }
            Property::CoolingSetpoint => {
                self.cool_seen && matches!(self.system_sequence, Some(0 | 1 | 4 | 5))
            }
            _ => true,
        }
    }
    pub fn modes(&self, p: Property) -> &'static [String] {
        // Discovery and the write path share the same device-declared choices.
        static CHOICES: std::sync::LazyLock<[Vec<String>; 8]> = std::sync::LazyLock::new(|| {
            [
                &["off", "cool"][..],
                &["off", "heat"],
                &["off", "cool", "heat"],
                &["off", "low", "medium", "high"],
                &["off", "low", "high"],
                &["off", "low", "medium", "high", "auto"],
                &["off", "low", "high", "auto"],
                &["off", "on", "auto"],
            ]
            .map(|names| names.iter().map(|&s| s.to_owned()).collect())
        });
        let index = match (p, self.system_sequence, self.fan_sequence) {
            (Property::SystemMode, Some(s), _) if self.mode_seen => usize::from(s / 2),
            (Property::FanMode, _, Some(s)) if self.fan_seen => 3 + usize::from(s),
            _ => return &[],
        };
        &CHOICES[index]
    }
    pub fn dual_mode(&self) -> bool {
        matches!(self.system_sequence, Some(4 | 5))
    }
    pub fn setpoint_limits(&self, p: Property) -> Option<(i16, i16)> {
        if !self.readable(p) {
            return None;
        }
        let start = match (p, self.system_sequence) {
            (Property::HeatingSetpoint, Some(2..=5)) => 0,
            (Property::CoolingSetpoint, Some(0 | 1 | 4 | 5)) => 2,
            _ => return None,
        };
        if self.dual_mode() && !(self.heat_seen && self.cool_seen && self.deadband.is_some()) {
            return None;
        }
        let lo = self.limits[start]?;
        let hi = self.limits[start + 1]?;
        (lo < hi).then_some((lo, hi))
    }
    pub fn request(&self, p: Property, ep: u8, seq: u8, value: Option<&Json>) -> Result<Request> {
        ensure!(self.readable(p), "unavailable HVAC property");
        let (cluster, id) = p.address();
        let bytes;
        let payload = if let Some(v) = value {
            bytes = match p {
                Property::SystemMode | Property::FanMode => {
                    let mode = v.as_str().context("expected mode name")?;
                    ensure!(
                        self.modes(p).iter().any(|s| s == mode),
                        "mode unavailable for this endpoint"
                    );
                    vec![
                        p.enum_names()
                            .iter()
                            .find(|(_, name)| *name == mode)
                            .context("unknown mode")?
                            .0,
                    ]
                }
                Property::HeatingSetpoint | Property::CoolingSetpoint => {
                    ensure!(
                        !self.dual_mode(),
                        "dual-mode setpoints require fresh readback"
                    );
                    let (min, max) = self
                        .setpoint_limits(p)
                        .context("confirmed single-mode setpoint limits required")?;
                    let n = v
                        .as_f64()
                        .context("expected temperature in degrees Celsius")?
                        * 100.;
                    ensure!(
                        (f64::from(min)..=f64::from(max)).contains(&n.round())
                            && (n.round() - n).abs() < 1e-8,
                        "setpoint out of range or precision"
                    );
                    (n.round() as i16).to_le_bytes().to_vec()
                }
                Property::LocalTemperature => bail!("local temperature is read-only"),
            };
            Payload::WriteAttributes(vec![AttributeValue {
                id,
                value: Value::from_bytes(p.ty().try_into()?, &bytes)?,
            }])
        } else {
            Payload::ReadAttributes(vec![id])
        };
        Ok(request(
            ep,
            seq,
            cluster,
            FrameType::General,
            payload.command() as u8,
            &payload.encode()?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn value(ty: u8, bytes: &[u8]) -> Value<'_> {
        Value::from_bytes(ty.try_into().unwrap(), bytes).unwrap()
    }
    fn endpoint(servers: &[u16]) -> Endpoint<'_> {
        Endpoint {
            id: 9,
            profile: 0x0104,
            servers,
            clients: &[THERMOSTAT, FAN],
        }
    }
    #[test]
    fn descriptor_roles_and_mode_evidence_gate_controls() {
        for (ep, device) in [
            (endpoint(&[]), 0x301),
            (endpoint(&[THERMOSTAT]), 0x0302),
            (
                Endpoint {
                    profile: 0xc05e,
                    ..endpoint(&[THERMOSTAT, FAN])
                },
                0x301,
            ),
        ] {
            assert!(!Capabilities::from_descriptor(&ep, device).active());
        }
        let mut cap = Capabilities::from_descriptor(&endpoint(&[FAN]), 0x100);
        let fan = Property::FanMode;
        assert!(cap.request(fan, 9, 1, None).is_ok());
        assert!(cap.request(fan, 9, 1, Some(&json!("on"))).is_err());
        cap.record(FAN, 0, value(0x30, &[4]));
        cap.record(FAN, 1, value(0x20, &[4])); // wrong type
        assert!(cap.modes(fan).is_empty());
        cap.record(FAN, 1, value(0x30, &[4]));
        assert_eq!(cap.modes(fan), ["off", "on", "auto"]);
        assert!(cap.request(fan, 9, 1, Some(&json!("low"))).is_err());
        assert!(cap.request(fan, 9, 1, Some(&json!("on"))).is_ok());
    }

    #[test]
    fn setpoint_bounds_are_device_specific_and_dual_mode_is_read_only() {
        let mut cap = Capabilities::from_descriptor(&endpoint(&[THERMOSTAT]), 0x307);
        let p = Property::CoolingSetpoint;
        cap.record(THERMOSTAT, 0x1b, value(0x30, &[0]));
        cap.record(THERMOSTAT, 0x11, value(0x29, &1900i16.to_le_bytes()));
        assert!(cap.request(p, 9, 1, Some(&json!(20))).is_err());
        cap.record(THERMOSTAT, 0x17, value(0x29, &1650i16.to_le_bytes()));
        cap.record(THERMOSTAT, 0x18, value(0x29, &2750i16.to_le_bytes()));
        for n in [16.5, 23.08, 27.5] {
            assert!(cap.request(p, 9, 1, Some(&json!(n))).is_ok());
        }
        for n in [16.49, 27.51, 20.001] {
            assert!(cap.request(p, 9, 1, Some(&json!(n))).is_err());
        }
        cap.record(THERMOSTAT, 0x1b, value(0x30, &[4]));
        assert!(cap.request(p, 9, 1, None).is_ok());
        assert!(cap.request(p, 9, 1, Some(&json!(20))).is_err());
        assert_eq!(
            Property::LocalTemperature.confirmed(value(0x29, &(-55i16).to_le_bytes())),
            Some(json!(-0.55))
        );
        assert!(
            Property::LocalTemperature
                .confirmed(value(0x29, &i16::MIN.to_le_bytes()))
                .is_none()
        );
    }
}
