//! Public equipment attributes, ZCL R8 §§3.4, 3.9, 5.3, 6.2, 6.5–6.6, 7.2–7.5, 8.4.
//! The generated scalar catalogue owns the wire type. This table supplies device semantics.
pub mod commands;
pub mod commissioning;
pub mod level_output;
pub mod schedule;

pub const DOOR_LOCK: u16 = 0x0101;
pub const SECURITY_LEVEL: u16 = 0x0034;

use crate::{lighting::Request, standard_controls::request};
use anyhow::{Context, Result, ensure};
use serde_json::{Value as Json, json};
use zigbee_zcl::{
    FrameType, attributes::AttributeValue, foundation::Payload, standard_attributes::wire_type,
    value::Value,
};

#[derive(Clone, Copy)]
pub enum Format {
    /// Empty entries mark reserved wire codes.
    Enum(&'static [&'static str]),
    Bit {
        mask: u8,
        names: &'static [&'static str; 2],
    },
    Number {
        min: i32,
        max: i32,
        divisor: u16,
        unit: Option<&'static str>,
    },
}

#[derive(Clone, Copy)]
pub struct Field {
    pub cluster: u16,
    pub id: u16,
    pub name: &'static str,
    pub format: Format,
    pub mandatory: bool,
    pub writable: bool,
    pub reportable: bool,
}

const fn number(min: i32, max: i32, divisor: u16, unit: Option<&'static str>) -> Format {
    Format::Number {
        min,
        max,
        divisor,
        unit,
    }
}

const fn field(cluster: u16, id: u16, name: &'static str, format: Format) -> Field {
    Field {
        cluster,
        id,
        name,
        format,
        mandatory: true,
        writable: false,
        reportable: false,
    }
}

impl Field {
    const fn optional(mut self) -> Self {
        self.mandatory = false;
        self
    }
    const fn reportable(mut self) -> Self {
        self.reportable = true;
        self
    }
    const fn writable(mut self) -> Self {
        self.writable = true;
        self
    }

    pub fn decode(&self, value: Value<'_>) -> Option<Json> {
        let n = raw_number(self.cluster, self.id, value)?;
        // These FF values explicitly withdraw the last known position or lock state.
        match (self.cluster, self.id, n) {
            (0x0101, 0 | 3, 255) => return Some(json!("unknown")),
            (0x0103, 10, 255) => return Some(Json::Null),
            _ => {}
        }
        match self.format {
            Format::Enum(names) => names
                .get(usize::try_from(n).ok()?)
                .filter(|s| !s.is_empty())
                .map(|s| json!(s)),
            Format::Bit { mask, names } => {
                Some(json!(names[usize::from(n & i32::from(mask) != 0)]))
            }
            Format::Number {
                min, max, divisor, ..
            } if (min..=max).contains(&n) => Some(if divisor == 1 {
                json!(n)
            } else {
                json!(f64::from(n) / f64::from(divisor))
            }),
            _ => None,
        }
    }

    fn input(&self, input: &Json) -> Result<u16> {
        ensure!(self.writable, "equipment property is read-only");
        match self.format {
            Format::Enum(names) => enum_code(names, input),
            Format::Bit { names, .. } => enum_code(names, input),
            Format::Number {
                min,
                max,
                divisor: 1,
                ..
            } => {
                let n = input.as_i64().context("expected an integer setting")?;
                ensure!(
                    (i64::from(min)..=i64::from(max)).contains(&n),
                    "setting out of range"
                );
                Ok(u16::try_from(n)?)
            }
            _ => anyhow::bail!("unsupported equipment write"),
        }
    }

    /// Coupled settings use a fresh snapshot before reaching the write encoder.
    pub fn request(&self, endpoint: u8, seq: u8, write: Option<&Json>) -> Result<Request> {
        let bytes;
        let payload = if let Some(input) = write {
            ensure!(
                !commissioning::Setting::required(self),
                "equipment write requires current constraints"
            );
            bytes = [u8::try_from(self.input(input)?)?];
            let ty = wire_type(self.cluster, self.id)
                .context("missing wire type")?
                .try_into()?;
            Payload::WriteAttributes(vec![AttributeValue {
                id: self.id,
                value: Value::from_bytes(ty, &bytes)?,
            }])
        } else {
            Payload::ReadAttributes(vec![self.id])
        };
        Ok(request(
            endpoint,
            seq,
            self.cluster,
            FrameType::General,
            payload.command() as u8,
            &payload.encode()?,
        ))
    }
}

fn raw_number(cluster: u16, id: u16, value: Value<'_>) -> Option<i32> {
    let ty = wire_type(cluster, id)?;
    if ty != value.data_type().id() {
        return None;
    }
    Some(match (ty, value.as_bytes()) {
        (0x10 | 0x18 | 0x20 | 0x30, [n]) => i32::from(*n),
        (0x19 | 0x21, [a, b]) => i32::from(u16::from_le_bytes([*a, *b])),
        (0x29, [a, b]) => i32::from(i16::from_le_bytes([*a, *b])),
        (0x22, [a, b, c]) => i32::from_le_bytes([*a, *b, *c, 0]),
        _ => return None,
    })
}

fn enum_code(names: &[&str], input: &Json) -> Result<u16> {
    let name = input.as_str().context("expected a setting name")?;
    ensure!(!name.is_empty(), "empty setting name");
    Ok(names
        .iter()
        .position(|s| *s == name)
        .context("unknown setting")?
        .try_into()?)
}

const PUMP_CONTROL: &[&str] = &[
    "constant_speed",
    "constant_pressure",
    "proportional_pressure",
    "constant_flow",
    "",
    "constant_temperature",
    "",
    "automatic",
];

use Format::Enum;

pub static FIELDS: &[Field] = &[
    field(
        0x0002,
        0,
        "internal_temperature",
        number(-200, 200, 1, Some("°C")),
    ),
    field(
        0x0002,
        1,
        "internal_temperature_minimum",
        number(-200, 200, 1, Some("°C")),
    )
    .optional(),
    field(
        0x0002,
        2,
        "internal_temperature_maximum",
        number(-200, 200, 1, Some("°C")),
    )
    .optional(),
    field(
        0x0007,
        0,
        "switch_type",
        Enum(&["toggle", "momentary", "multifunction"]),
    ),
    field(
        0x0007,
        0x10,
        "switch_actions",
        Enum(&["on_off", "off_on", "toggle"]),
    )
    .writable(),
    field(
        0x0100,
        0,
        "shade_physical_closed_limit",
        number(1, 65534, 1, None),
    )
    .optional(),
    field(0x0100, 1, "shade_motor_step", number(0, 254, 10, Some("°"))).optional(),
    field(0x0100, 2, "shade_status_flags", number(0, 15, 1, None)),
    field(
        0x0100,
        2,
        "shade_forward_direction",
        Format::Bit {
            mask: 8,
            names: &["closing", "opening"],
        },
    )
    .writable(),
    field(
        0x0100,
        0x10,
        "shade_closed_limit",
        number(1, 65534, 1, None),
    )
    .writable(),
    field(0x0100, 0x11, "shade_mode", Enum(&["normal", "configure"])).writable(),
    field(
        0x0101,
        0,
        "lock_state",
        Enum(&["not_fully_locked", "locked", "unlocked"]),
    )
    .reportable(),
    field(
        0x0101,
        3,
        "door_state",
        Enum(&[
            "open",
            "closed",
            "jammed",
            "forced_open",
            "unspecified_error",
        ]),
    )
    .optional()
    .reportable(),
    field(
        0x0103,
        1,
        "barrier_moving_state",
        Enum(&["stopped", "closing", "opening"]),
    )
    .reportable(),
    field(0x0103, 2, "barrier_safety_flags", number(0, 15, 1, None)).reportable(),
    field(0x0103, 10, "barrier_position", number(0, 100, 1, Some("%"))).reportable(),
    field(
        0x0200,
        0x11,
        "pump_effective_operation_mode",
        Enum(&["normal", "minimum", "maximum", "local"]),
    ),
    field(
        0x0200,
        0x12,
        "pump_effective_control_mode",
        Enum(PUMP_CONTROL),
    ),
    field(
        0x0200,
        0x20,
        "pump_operation_mode",
        Enum(&["normal", "minimum", "maximum", "local"]),
    )
    .writable(),
    field(0x0200, 0x21, "pump_control_mode", Enum(PUMP_CONTROL))
        .optional()
        .writable(),
    field(0x0200, 0x10, "pump_status_flags", number(0, 511, 1, None))
        .optional()
        .reportable(),
    field(
        0x0200,
        0x13,
        "pump_capacity",
        number(0, 32767, 200, Some("%")),
    )
    .reportable(),
    field(0x0200, 0x14, "pump_speed", number(0, 65534, 1, Some("rpm"))).optional(),
    field(
        0x0200,
        0x15,
        "pump_lifetime_running_hours",
        number(0, 16777214, 1, Some("h")),
    )
    .optional(),
    field(
        0x0200,
        0x16,
        "pump_power",
        number(0, 16777214, 1, Some("W")),
    )
    .optional(),
    field(
        0x0203,
        0,
        "dehumidifier_humidity",
        number(0, 100, 1, Some("%")),
    )
    .optional(),
    field(
        0x0203,
        1,
        "dehumidification_cooling",
        number(0, 100, 1, Some("%")),
    )
    .reportable(),
    field(
        0x0203,
        0x10,
        "dehumidification_setpoint",
        number(30, 100, 1, Some("%")),
    )
    .writable(),
    field(
        0x0203,
        0x11,
        "dehumidifier_humidity_mode",
        Enum(&["local", "network"]),
    )
    .optional(),
    field(
        0x0203,
        0x12,
        "dehumidification_lockout",
        Enum(&["disabled", "allowed"]),
    )
    .optional(),
    field(
        0x0203,
        0x13,
        "dehumidification_hysteresis",
        number(2, 20, 1, Some("%")),
    )
    .writable(),
    field(
        0x0203,
        0x14,
        "dehumidification_max_cool",
        number(20, 100, 1, Some("%")),
    )
    .writable(),
    field(
        0x0203,
        0x15,
        "dehumidifier_humidity_display",
        Enum(&["hidden", "visible"]),
    )
    .optional()
    .writable(),
    field(
        0x0204,
        0,
        "temperature_display_mode",
        Enum(&["celsius", "fahrenheit"]),
    )
    .writable(),
    field(
        0x0204,
        1,
        "thermostat_keypad_lockout",
        Enum(&[
            "none", "level_1", "level_2", "level_3", "level_4", "level_5",
        ]),
    ),
    field(
        0x0204,
        2,
        "thermostat_schedule_programming",
        Enum(&["enabled", "disabled"]),
    )
    .optional()
    .writable(),
    field(
        0x0301,
        0,
        "ballast_physical_minimum_level",
        number(1, 254, 1, None),
    ),
    field(
        0x0301,
        1,
        "ballast_physical_maximum_level",
        number(1, 254, 1, None),
    ),
    field(0x0301, 2, "ballast_status_flags", number(0, 3, 1, None)).optional(),
    field(
        0x0301,
        0x10,
        "ballast_minimum_level",
        number(1, 254, 1, None),
    )
    .writable(),
    field(
        0x0301,
        0x11,
        "ballast_maximum_level",
        number(1, 254, 1, None),
    )
    .writable(),
    field(
        0x0301,
        0x20,
        "ballast_lamp_quantity",
        number(0, 254, 1, None),
    )
    .optional(),
    field(
        0x0502,
        0,
        "siren_max_duration",
        number(0, 65534, 1, Some("s")),
    ),
];

#[cfg(test)]
mod tests {
    use super::*;
    use commands::*;
    use zigbee_zcl::Frame;
    fn field(name: &str) -> &'static Field {
        FIELDS.iter().find(|f| f.name == name).unwrap()
    }

    #[test]
    fn equipment_wire_types_ranges_scaling_and_indeterminate_lock() -> Result<()> {
        for f in FIELDS {
            assert!(wire_type(f.cluster, f.id).is_some(), "{}", f.name);
        }
        let decode = |name, ty: u8, bytes: &[u8]| {
            field(name).decode(Value::from_bytes(ty.try_into().unwrap(), bytes).unwrap())
        };
        assert_eq!(
            decode("pump_capacity", 0x29, &20001_i16.to_le_bytes()),
            Some(json!(100.005))
        );
        assert_eq!(decode("pump_capacity", 0x29, &i16::MIN.to_le_bytes()), None);
        assert_eq!(
            decode("internal_temperature", 0x29, &(-40_i16).to_le_bytes()),
            Some(json!(-40))
        );
        assert_eq!(decode("lock_state", 0x20, &[1]), None);
        assert_eq!(decode("lock_state", 0x30, &[255]), Some(json!("unknown")));
        assert_eq!(decode("barrier_position", 0x20, &[255]), Some(Json::Null));
        assert_eq!(decode("pump_power", 0x22, &[2, 1, 1]), Some(json!(65794)));
        assert_eq!(decode("thermostat_keypad_lockout", 0x30, &[6]), None);
        let set = field("dehumidification_setpoint");
        assert!(set.request(7, 9, Some(&json!(29))).is_err());
        let req = set.request(7, 9, Some(&json!(55)))?;
        assert_eq!(req.frame, [0x10, 9, 2, 0x10, 0, 0x20, 55]);
        assert!(
            field("lock_state")
                .request(1, 2, Some(&json!("unlocked")))
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn equipment_barrier_and_warning_commands_observe_device_capabilities() -> Result<()> {
        assert!(move_barrier(3, 2, false, &json!(50)).is_err());
        assert_eq!(
            move_barrier(3, 2, false, &json!("close"))?.frame,
            [0x11, 2, 0, 0]
        );
        assert_eq!(
            move_barrier(3, 2, true, &json!(52))?.frame,
            [0x11, 2, 0, 52]
        );
        assert_eq!(
            move_barrier(3, 2, false, &json!("stop"))?.frame,
            [0x11, 2, 1]
        );
        let v = json!({"mode":"burglar","duration":50,"level":1,"strobe_duty_cycle":45,"strobe_level":2});
        assert!(warning(3, 2, None, &v).is_err());
        assert!(warning(3, 2, Some(49), &v).is_err());
        assert_eq!(
            warning(3, 2, Some(50), &v)?.frame,
            [0x11, 2, 0, 0x15, 50, 0, 45, 2]
        );
        assert_eq!(
            warning(3, 2, None, &json!("stop"))?.frame,
            [0x11, 2, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(
            squawk(3, 2, &json!({"mode":"disarmed","level":2,"strobe":true}))?.frame,
            [0x11, 2, 1, 0x1a]
        );
        Ok(())
    }

    #[test]
    fn equipment_status_and_schedule_require_the_right_response() -> Result<()> {
        let status = [0x19, 10, 5, 3, 0, 0, 0];
        assert!(panel_status(Frame::decode(&status)?, false).is_none());
        assert_eq!(
            panel_status(Frame::decode(&status)?, true),
            Some("armed_away")
        );
        assert!(panel_status(Frame::decode(&status[..6])?, true).is_none());
        let q = ScheduleQuery::parse(&json!({"day":"monday","mode":"heat"}))?;
        assert_eq!(q.request(8, 3)?.frame, [0x11, 3, 2, 2, 1]);
        let mut f = [0x19, 3, 0, 1, 2, 1, 104, 1, 0x34, 8];
        assert_eq!(
            q.response(Frame::decode(&f)?),
            Some(json!({"day":"monday","mode":"heat",
            "transitions":[{"minute":360,"setpoint":21.0}]}))
        );
        f[4] = 4;
        assert!(q.response(Frame::decode(&f)?).is_none());
        f[4] = 2;
        f[7] = 6;
        assert!(q.response(Frame::decode(&f)?).is_none());
        Ok(())
    }
}
