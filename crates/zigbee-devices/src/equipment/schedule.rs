//! Thermostat weekly schedule operations (ZCL R8 §6.3.2.3.2–3).
//! One request owns a set of distinct days. Each Set replaces one complete
//! day and requires a Default Response followed by an independent Get.
use super::commands::encode;
use crate::lighting::Request;
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use serde_json::{Value as Json, json};
use zigbee_zcl::{
    Direction, Frame, FrameType,
    cluster::{Command, ScheduleMode, ScheduleTransition, Thermostat, WeeklySchedule},
};

const DAYS: [&str; 8] = [
    "sunday",
    "monday",
    "tuesday",
    "wednesday",
    "thursday",
    "friday",
    "saturday",
    "away",
];

#[derive(Clone, Copy, Default)]
pub struct Capabilities {
    pub weekly: u8,
    pub daily: u8,
    sequence: Option<u8>,
    heat: Option<(i16, i16)>,
    cool: Option<(i16, i16)>,
    deadband: Option<i16>,
}

impl Capabilities {
    pub fn resolve<'a>(get: impl Fn(u16) -> Option<(u8, &'a [u8])>) -> Self {
        let byte = |id| match get(id) {
            Some((0x20, [v])) => Some(*v),
            _ => None,
        };
        let signed = |id| match get(id) {
            Some((0x29, [a, b])) => Some(i16::from_le_bytes([*a, *b])),
            _ => None,
        };
        // AbsMin/Max define the physical limits for schedule commands. Local
        // configured bounds, when available, can only tighten them.
        let limits = |lo, hi, soft_lo, soft_hi| {
            let min = signed(lo)?.max(signed(soft_lo).unwrap_or(i16::MIN));
            let max = signed(hi)?.min(signed(soft_hi).unwrap_or(i16::MAX));
            (min >= -27315 && min <= max).then_some((min, max))
        };
        Self {
            weekly: byte(0x21).unwrap_or(0),
            daily: byte(0x22).unwrap_or(0),
            sequence: match get(0x1b) {
                Some((0x30, [v @ 0..=5])) => Some(*v),
                _ => None,
            },
            heat: limits(3, 4, 0x15, 0x16),
            cool: limits(5, 6, 0x17, 0x18),
            deadband: match get(0x19) {
                Some((0x28, [v @ 0..=25])) => Some(i16::from(*v) * 10),
                _ => None,
            },
        }
    }
    pub fn supported(self) -> bool {
        self.daily > 0 && self.weekly > 0
    }
    fn writable_mode(self) -> Option<ScheduleMode> {
        match self.sequence? {
            0 | 1 if self.cool.is_some() => Some(ScheduleMode::COOL),
            2 | 3 if self.heat.is_some() => Some(ScheduleMode::HEAT),
            4 | 5 if self.heat.is_some() && self.cool.is_some() && self.deadband.is_some() => {
                Some(ScheduleMode::HEAT_AND_COOL)
            }
            _ => None,
        }
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    pages: Vec<Page>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Page {
    day: String,
    mode: String,
    transitions: Option<Vec<Transition>>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Transition {
    minute: u16,
    setpoint: Option<f64>,
    heat: Option<f64>,
    cool: Option<f64>,
}

fn setpoint(value: Option<f64>, limits: Option<(i16, i16)>) -> Result<i16> {
    let scaled = value.context("missing setpoint")? * 100.;
    let rounded = scaled.round();
    let (lo, hi) = limits.context("thermostat setpoint limits unavailable")?;
    ensure!(
        scaled.is_finite()
            && (scaled - rounded).abs() < 1e-7
            && (f64::from(lo)..=f64::from(hi)).contains(&rounded),
        "setpoint outside thermostat limits or finer than 0.01 °C"
    );
    Ok(rounded as i16)
}

#[derive(Debug)]
pub struct Day {
    pub days: u8,
    pub mode: ScheduleMode,
    pub edits: Option<Vec<ScheduleTransition>>,
}
impl Day {
    pub fn request(&self, ep: u8, seq: u8, write: bool) -> Result<Request> {
        let command = if write {
            Thermostat::SetWeeklySchedule {
                schedule: WeeklySchedule {
                    days: self.days,
                    mode: self.mode,
                    transitions: self.edits.clone().context("missing edit")?,
                },
            }
        } else {
            Thermostat::GetWeeklySchedule {
                days_to_return: self.days,
                mode_to_return: self.mode,
            }
        };
        let mut req = encode(ep, seq, Command::Thermostat(command))?;
        if write {
            req.frame[0] &= !0x10;
        } // Always request the required application-level acknowledgment.
        Ok(req)
    }
    pub fn observe(&self, frame: Frame<'_>) -> Option<Vec<ScheduleTransition>> {
        if frame.header.direction != Direction::ServerToClient
            || frame.header.manufacturer.is_some()
            || frame.header.frame_type != FrameType::ClusterSpecific
        {
            return None;
        }
        let Command::Thermostat(Thermostat::CurrentWeeklySchedule { schedule }) =
            Command::decode(0x201, frame).ok()?
        else {
            return None;
        };
        if schedule.days != self.days || schedule.mode != self.mode {
            return None;
        }
        Some(schedule.transitions)
    }
    pub fn json(&self, rows: &[ScheduleTransition]) -> Json {
        let mode = if self.mode == ScheduleMode::HEAT {
            "heat"
        } else if self.mode == ScheduleMode::COOL {
            "cool"
        } else {
            "both"
        };
        let temps = |temp: i16| json!(f64::from(temp) / 100.);
        let transitions = rows
            .iter()
            .map(|r| {
                let mut v = json!({"minute":r.minutes_since_midnight});
                if mode == "both" {
                    v["heat"] = r.heat_setpoint.map(temps).into();
                    v["cool"] = r.cool_setpoint.map(temps).into();
                } else {
                    v["setpoint"] = r.heat_setpoint.or(r.cool_setpoint).map(temps).into();
                }
                v
            })
            .collect::<Vec<_>>();
        json!({"day":DAYS[self.days.trailing_zeros() as usize],"mode":mode,"transitions":transitions})
    }
}

pub fn pages(input: &Json, write: bool, caps: Capabilities) -> Result<Vec<Day>> {
    ensure!(caps.supported(), "thermostat schedule capacity unconfirmed");
    let input: Input = serde_json::from_value(input.clone())?;
    ensure!(
        (1..=8).contains(&input.pages.len()),
        "select one to eight distinct days"
    );
    let mut days = 0;
    let mut count = 0usize;
    let mut result = Vec::new();
    for p in input.pages {
        let day = 1
            << DAYS
                .iter()
                .position(|d| *d == p.day)
                .context("unknown schedule day")?;
        ensure!(days & day == 0, "duplicate schedule day");
        days |= day;
        let mode = match p.mode.as_str() {
            "heat" => ScheduleMode::HEAT,
            "cool" => ScheduleMode::COOL,
            "both" => ScheduleMode::HEAT_AND_COOL,
            _ => bail!("unknown schedule mode"),
        };
        if let Some(sequence) = caps.sequence {
            let supported = match sequence {
                0 | 1 => ScheduleMode::COOL,
                2 | 3 => ScheduleMode::HEAT,
                _ => ScheduleMode::HEAT_AND_COOL,
            };
            ensure!(
                mode.bits() & !supported.bits() == 0,
                "schedule mode unavailable on this thermostat"
            );
        }
        let edits = if write {
            ensure!(
                caps.writable_mode() == Some(mode),
                "edit must cover all operating modes within confirmed limits"
            );
            let rows = p.transitions.context("missing daily transitions")?;
            ensure!(
                !rows.is_empty()
                    && rows.len() <= usize::from(caps.daily)
                    && rows.len() <= WeeklySchedule::MAX_TRANSITIONS,
                "each edited day must fit the thermostat and a single replace-day command"
            );
            let mut transitions = Vec::new();
            for row in rows {
                let prior = transitions
                    .last()
                    .map(|r: &ScheduleTransition| r.minutes_since_midnight);
                ensure!(
                    row.minute < 1440 && prior.is_none_or(|n| row.minute > n),
                    "transition times must be distinct and increasing"
                );
                let (heat, cool) = match (mode, row.setpoint, row.heat, row.cool) {
                    (ScheduleMode::HEAT, Some(n), None, None) => {
                        (Some(setpoint(Some(n), caps.heat)?), None)
                    }
                    (ScheduleMode::COOL, Some(n), None, None) => {
                        (None, Some(setpoint(Some(n), caps.cool)?))
                    }
                    (ScheduleMode::HEAT_AND_COOL, None, Some(h), Some(c)) => {
                        let h = setpoint(Some(h), caps.heat)?;
                        let c = setpoint(Some(c), caps.cool)?;
                        ensure!(
                            i32::from(c) - i32::from(h)
                                >= i32::from(caps.deadband.context("unknown deadband")?),
                            "heat/cool deadband violated"
                        );
                        (Some(h), Some(c))
                    }
                    _ => bail!("supply setpoint for a single mode, heat and cool for both"),
                };
                transitions.push(ScheduleTransition {
                    minutes_since_midnight: row.minute,
                    heat_setpoint: heat,
                    cool_setpoint: cool,
                });
            }
            count += transitions.len();
            ensure!(
                count <= usize::from(caps.weekly),
                "edit exceeds weekly transition capacity"
            );
            Some(transitions)
        } else {
            ensure!(
                p.transitions.is_none(),
                "a schedule query cannot include edits"
            );
            None
        };
        result.push(Day {
            days: day,
            mode,
            edits,
        });
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn caps() -> Capabilities {
        Capabilities {
            weekly: 6,
            daily: 3,
            sequence: Some(4),
            heat: Some((1500, 2500)),
            cool: Some((1800, 3000)),
            deadband: Some(200),
        }
    }
    fn input(rows: Json) -> Json {
        json!({"pages":[{"day":"monday","mode":"both","transitions":rows}]})
    }
    #[test]
    fn only_complete_days_with_supported_modes_limits_and_capacity_can_be_edited() -> Result<()> {
        let good = json!({"minute":360,"heat":19.25,"cool":24});
        let valid = input(json!([good]));
        let plan = pages(&valid, true, caps())?;
        let req = plan[0].request(4, 9, true)?;
        assert_eq!(req.frame, [1, 9, 1, 1, 2, 3, 104, 1, 0x85, 7, 0x60, 9]);
        for bad in [
            input(json!([])),
            input(json!([good, good])),
            input(json!([{"minute":0,"heat":27,"cool":29}])),
            input(json!([{"minute":1440,"heat":18,"cool":26}])),
            input(json!([{"minute":0,"heat":19.123,"cool":22}])),
            input(json!([{"minute":0,"heat":20,"cool":21}])),
            input(json!([{"minute":0,"heat":18,"cool":22,"setpoint":20}])),
            json!({"pages":[{"day":"monday","mode":"heat","transitions":[{"minute":0,"setpoint":20}]}]}),
            json!({"pages":[valid["pages"][0],valid["pages"][0]]}),
        ] {
            assert!(pages(&bad, true, caps()).is_err());
        }
        assert!(
            pages(
                &valid,
                true,
                Capabilities {
                    weekly: 0,
                    ..caps()
                }
            )
            .is_err()
        );
        let two = |row: &Json| json!({"pages":[{"day":"monday","mode":"both","transitions":[row]},{"day":"tuesday","mode":"both","transitions":[row]}]});
        assert!(
            pages(
                &two(&good),
                true,
                Capabilities {
                    weekly: 1,
                    ..caps()
                }
            )
            .is_err()
        );
        assert!(
            pages(
                &valid,
                true,
                Capabilities {
                    heat: None,
                    ..caps()
                }
            )
            .is_err()
        );
        Ok(())
    }
}
