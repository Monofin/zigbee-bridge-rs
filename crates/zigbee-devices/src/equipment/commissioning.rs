//! Device-dependent R8 §§5.3.2.2, 6.2.2.2 and 7.2.2.1 settings.
//! One write per fresh, complete read. The device remains authoritative.
use super::{Field, PUMP_CONTROL, raw_number};
use crate::{lighting::Request, standard_controls::request};
use anyhow::{Context, Result, bail, ensure};
use serde_json::Value as Json;
use zigbee_zcl::{
    FrameType,
    attributes::{AttributeValue, ReadResult, Status},
    foundation::Payload,
    standard_attributes::wire_type,
    value::Value,
};

#[derive(Clone, Copy)]
enum Guard {
    ShadeLimit,
    ShadeDirection,
    ShadeMode,
    BallastMinimum,
    BallastMaximum,
    PumpControl,
}

impl Guard {
    fn for_field(f: &Field) -> Option<Self> {
        match (f.cluster, f.id, f.name) {
            (0x100, 0x10, _) => Some(Self::ShadeLimit),
            (0x100, 2, "shade_forward_direction") => Some(Self::ShadeDirection),
            (0x100, 0x11, _) => Some(Self::ShadeMode),
            (0x301, 0x10, _) => Some(Self::BallastMinimum),
            (0x301, 0x11, _) => Some(Self::BallastMaximum),
            (0x200, 0x21, _) => Some(Self::PumpControl),
            _ => None,
        }
    }
}

#[derive(Clone, Copy)]
pub struct Setting {
    guard: Guard,
    pub field: &'static Field,
    code: u16,
}

impl Setting {
    pub fn required(field: &Field) -> bool {
        Guard::for_field(field).is_some()
    }
    pub fn prepare(field: &'static Field, input: &Json) -> Result<Option<Self>> {
        Guard::for_field(field)
            .map(|guard| {
                Ok(Self {
                    guard,
                    field,
                    code: field.input(input)?,
                })
            })
            .transpose()
    }
    fn attributes(self) -> &'static [u16] {
        match self.guard {
            Guard::BallastMinimum | Guard::BallastMaximum => &[0, 1, 0x10, 0x11],
            Guard::ShadeDirection | Guard::ShadeMode => &[2, 0x11],
            Guard::ShadeLimit => &[0, 2, 0x10, 0x11],
            Guard::PumpControl => match self.code {
                0 => &[0x21, 0x12, 7, 8],
                1 => &[0x21, 0x12, 3, 4],
                2 => &[0x21, 0x12, 5, 6],
                3 => &[0x21, 0x12, 9, 10],
                5 => &[0x21, 0x12, 11, 12],
                _ => &[0x21, 0x12],
            },
        }
    }
    pub fn read_constraints(self, ep: u8, seq: u8) -> Result<Request> {
        let p = Payload::ReadAttributes(self.attributes().to_vec());
        Ok(request(
            ep,
            seq,
            self.field.cluster,
            FrameType::General,
            p.command() as u8,
            &p.encode()?,
        ))
    }
    pub fn write(self, ep: u8, seq: u8, records: &[ReadResult<'_>]) -> Result<Request> {
        let ids = self.attributes();
        ensure!(
            ids.len() == records.len()
                && ids
                    .iter()
                    .all(|id| records.iter().filter(|r| r.id == *id).count() == 1),
            "incomplete or ambiguous constraints"
        );
        let number = |id| -> Result<i32> {
            let value = records
                .iter()
                .find(|r| r.id == id)
                .context("missing device constraint")?;
            let Status::Success(v) = value.result else {
                bail!("device constraint unavailable")
            };
            raw_number(self.field.cluster, id, v).context("incorrect constraint type")
        };
        let target = i32::from(self.code);
        let mut code = self.code;
        match self.guard {
            Guard::BallastMinimum | Guard::BallastMaximum => {
                let (low, high, min, max) = (number(0)?, number(1)?, number(0x10)?, number(0x11)?);
                ensure!(
                    1 <= low && low <= min && min <= max && max <= high && high <= 254,
                    "inconsistent ballast levels"
                );
                let limits = match self.guard {
                    Guard::BallastMinimum => low..=max,
                    _ => min..=high,
                };
                ensure!(limits.contains(&target), "outside current ballast limits");
            }
            Guard::ShadeLimit | Guard::ShadeDirection | Guard::ShadeMode => {
                let (status, mode) = (number(2)?, number(0x11)?);
                ensure!(
                    matches!(mode, 0 | 1) && status & 2 == 0,
                    "unknown shade mode or motor adjusting"
                );
                match self.guard {
                    Guard::ShadeLimit => {
                        let (physical, current) = (number(0)?, number(0x10)?);
                        ensure!(
                            (1..65535).contains(&physical)
                                && (1..65535).contains(&current)
                                && target <= physical,
                            "outside confirmed shade travel"
                        );
                    }
                    Guard::ShadeDirection => {
                        // Only bit 3 changes. Carry operational, moving, direction and reserved bits through.
                        code = (status as u16 & !8) | self.code << 3;
                    }
                    _ => {}
                }
            }
            Guard::PumpControl => {
                let (current, effective) = (number(0x21)?, number(0x12)?);
                let known = |n: i32| {
                    usize::try_from(n)
                        .ok()
                        .and_then(|index| PUMP_CONTROL.get(index))
                        .is_some_and(|s| !s.is_empty())
                };
                ensure!(
                    known(current) && known(effective),
                    "unknown pump control mode"
                );
                if current != target && effective != target {
                    // R8 defines a pair of supported setpoint bounds for each physical control algorithm.
                    let [_, _, low, high] = *ids else {
                        bail!("automatic pump mode has not been observed")
                    };
                    let (min, max) = (number(low)?, number(high)?);
                    ensure!(
                        supported_range(self.code, min, max),
                        "invalid pump control limits"
                    );
                }
            }
        }
        let ty = wire_type(self.field.cluster, self.field.id).context("missing setting type")?;
        let bytes = code.to_le_bytes();
        let width = if ty == 0x21 { 2 } else { 1 };
        let value = Value::from_bytes(ty.try_into()?, &bytes[..width])?;
        let p = Payload::WriteAttributes(vec![AttributeValue {
            id: self.field.id,
            value,
        }]);
        Ok(request(
            ep,
            seq,
            self.field.cluster,
            FrameType::General,
            p.command() as u8,
            &p.encode()?,
        ))
    }
}

fn supported_range(mode: u16, min: i32, max: i32) -> bool {
    let range = match mode {
        1 | 2 => -32767..=32767,
        5 => -27315..=32767,
        _ => 0..=65534,
    };
    range.contains(&min) && range.contains(&max) && min <= max
}

/// Stable physical bounds guide discovery. Every actual write still reads current constraints.
#[derive(Clone, Copy, Default)]
pub struct Capabilities {
    pub shade_travel: Option<u16>,
    pub ballast_levels: Option<(u8, u8)>,
    pump_modes: u8,
}

impl Capabilities {
    pub fn resolve<'a>(get: impl Fn(u16, u16) -> Option<(u8, &'a [u8])>) -> Self {
        let number = |c, a| {
            let (ty, bytes) = get(c, a)?;
            raw_number(c, a, Value::from_bytes(ty.try_into().ok()?, bytes).ok()?)
        };
        let shade_travel = number(0x100, 0)
            .filter(|n| (1..65535).contains(n))
            .map(|n| n as u16);
        let ballast_levels = number(0x301, 0)
            .zip(number(0x301, 1))
            .filter(|(lo, hi)| 1 <= *lo && lo <= hi && *hi <= 254)
            .map(|(lo, hi)| (lo as u8, hi as u8));
        let mut pump_modes = 0u8;
        for a in [0x12, 0x21] {
            if let Some(n) = number(0x200, a)
                && (0..8).contains(&n)
                && !PUMP_CONTROL[n as usize].is_empty()
            {
                pump_modes |= 1 << n;
            }
        }
        for (mode, lo, hi) in [(0, 7, 8), (1, 3, 4), (2, 5, 6), (3, 9, 10), (5, 11, 12)] {
            if let Some((min, max)) = number(0x200, lo).zip(number(0x200, hi))
                && supported_range(mode, min, max)
            {
                pump_modes |= 1 << mode;
            }
        }
        Self {
            shade_travel,
            ballast_levels,
            pump_modes,
        }
    }
    pub fn pump_choices(self) -> &'static [String] {
        use std::sync::OnceLock;
        static CHOICES: [OnceLock<Vec<String>>; 256] = [const { OnceLock::new() }; 256];
        CHOICES[usize::from(self.pump_modes)].get_or_init(|| {
            PUMP_CONTROL
                .iter()
                .enumerate()
                .filter(|(i, _)| self.pump_modes & (1 << i) != 0)
                .map(|(_, s)| (*s).to_owned())
                .collect()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use zigbee_zcl::Frame;
    fn setting(name: &str, value: Json) -> Result<Setting> {
        let f = super::super::FIELDS
            .iter()
            .find(|f| f.name == name)
            .unwrap();
        assert!(f.request(2, 0, Some(&value)).is_err());
        Setting::prepare(f, &value)?.context("expected guarded setting")
    }
    fn apply(s: Setting, values: &[(u16, u8, &[u8])]) -> Result<Vec<u8>> {
        let records = values
            .iter()
            .map(|(id, ty, data)| {
                Ok(ReadResult {
                    id: *id,
                    result: Status::Success(Value::from_bytes((*ty).try_into()?, data)?),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(s.write(3, 2, &records)?.frame)
    }
    #[test]
    fn ballast_bounds_require_a_complete_consistent_live_snapshot() -> Result<()> {
        let s = setting("ballast_maximum_level", json!(23))?;
        let mut rows = [
            (0, 0x20, &[10][..]),
            (1, 0x20, &[200]),
            (0x10, 0x20, &[20]),
            (0x11, 0x20, &[100]),
        ];
        assert_eq!(apply(s, &rows)?, [0x10, 2, 2, 0x11, 0, 0x20, 23]);
        assert!(apply(s, &rows[..3]).is_err());
        rows[2].2 = &[24];
        assert!(apply(s, &rows).is_err());
        rows[2].2 = &[9];
        assert!(apply(s, &rows).is_err());
        rows[2].2 = &[20];
        rows[0].1 = 0x30;
        assert!(apply(s, &rows).is_err());
        rows[0] = rows[2];
        assert!(apply(s, &rows).is_err());
        Ok(())
    }
    #[test]
    fn shade_direction_preserves_status_and_closed_limit_uses_physical_steps() -> Result<()> {
        let s = setting("shade_forward_direction", json!("opening"))?;
        let mut status = [(2, 0x18, &[0x85][..]), (0x11, 0x30, &[1])];
        assert_eq!(apply(s, &status)?, [0x10, 2, 2, 2, 0, 0x18, 0x8d]);
        status[0].2 = &[3];
        assert!(apply(s, &status).is_err());
        status[0].2 = &[1];
        status[1].2 = &[0xff];
        assert!(apply(s, &status).is_err());
        let limit = setting("shade_closed_limit", json!(601))?;
        let rows = [
            (0, 0x21, &[0x58, 2][..]),
            (2, 0x18, &[1]),
            (0x10, 0x21, &[0x58, 2]),
            (0x11, 0x30, &[0]),
        ];
        assert!(apply(limit, &rows).is_err());
        let s = setting("shade_mode", json!("configure"))?;
        assert_eq!(
            apply(s, &[(2, 0x18, &[1]), (0x11, 0x30, &[0])])?,
            [0x10, 2, 2, 0x11, 0, 0x30, 1]
        );
        Ok(())
    }
    #[test]
    fn pump_mode_requires_an_observed_algorithm_or_valid_capability_pair() -> Result<()> {
        let mode = setting("pump_control_mode", json!("constant_temperature"))?;
        assert_eq!(
            Frame::decode(&mode.read_constraints(2, 1)?.frame)?.payload,
            [0x21, 0, 0x12, 0, 11, 0, 12, 0]
        );
        let mut rows = [
            (0x21, 0x30, &[0][..]),
            (0x12, 0x30, &[0]),
            (11, 0x29, &[0, 0]),
            (12, 0x29, &[100, 0]),
        ];
        assert_eq!(apply(mode, &rows)?, [0x10, 2, 2, 0x21, 0, 0x30, 5]);
        rows[2].2 = &[0, 0x80];
        assert!(apply(mode, &rows).is_err());
        rows[2].2 = &[101, 0];
        assert!(apply(mode, &rows).is_err());
        let automatic = setting("pump_control_mode", json!("automatic"))?;
        assert!(apply(automatic, &rows[..2]).is_err());
        rows[1].2 = &[7];
        assert_eq!(
            apply(automatic, &rows[..2])?,
            [0x10, 2, 2, 0x21, 0, 0x30, 7]
        );
        assert!(setting("pump_control_mode", json!("")).is_err());
        Ok(())
    }
}
