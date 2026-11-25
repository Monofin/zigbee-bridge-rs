//! ZCL R8 §§6.3, 7.5, 8.3 and 8.4. Commands are scoped to the advertised server.
use crate::{lighting::Request, standard_controls::request};
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use serde_json::{Value as Json, json};
use zigbee_zcl::{
    Frame, FrameType,
    cluster::{
        BarrierControl, Command, IasWarningDevice, ScheduleMode, Thermostat, WeeklySchedule,
    },
};

pub use crate::ias::ace::CLUSTER as PANEL;
const DAYS: &[&str] = &[
    "sunday",
    "monday",
    "tuesday",
    "wednesday",
    "thursday",
    "friday",
    "saturday",
    "away",
];
const WARNINGS: &[&str] = &[
    "stop",
    "burglar",
    "fire",
    "emergency",
    "police_panic",
    "fire_panic",
    "emergency_panic",
];

pub(super) fn encode(ep: u8, seq: u8, cmd: Command) -> Result<Request> {
    let key = cmd.key();
    Ok(request(
        ep,
        seq,
        key.cluster,
        FrameType::ClusterSpecific,
        key.command,
        &cmd.encode()?,
    ))
}

pub fn panel_query(ep: u8, seq: u8) -> Request {
    request(ep, seq, PANEL, FrameType::ClusterSpecific, 0x07, &[])
}

/// A status change is unsolicited. A Get Panel Status Response requires a matching request.
pub fn panel_status(frame: Frame<'_>, awaiting: bool) -> Option<&'static str> {
    crate::ias::ace::PanelStatus::decode(frame, awaiting).map(|s|s.state)
}

pub fn move_barrier(ep: u8, seq: u8, partial: bool, input: &Json) -> Result<Request> {
    use BarrierControl::*;
    let cmd = match input.as_str() {
        Some("stop") => BarrierControlStop {},
        Some("open") => BarrierControlGoToPercent { percent_open: 100 },
        Some("close") => BarrierControlGoToPercent { percent_open: 0 },
        _ => {
            let pos = input
                .as_u64()
                .context("expected open, close, stop or a percentage")?;
            ensure!(
                pos == 0 || pos == 100 || partial && pos < 100,
                "unsupported barrier position"
            );
            BarrierControlGoToPercent {
                percent_open: pos as u8,
            }
        }
    };
    encode(ep, seq, Command::BarrierControl(cmd))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Warning {
    mode: String,
    duration: u16,
    level: u8,
    #[serde(default)]
    strobe_duty_cycle: u8,
    #[serde(default)]
    strobe_level: u8,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Squawk {
    mode: String,
    level: u8,
    #[serde(default)]
    strobe: bool,
}

pub fn warning(ep: u8, seq: u8, max: Option<u16>, input: &Json) -> Result<Request> {
    use IasWarningDevice::StartWarning;
    let cmd = if input.as_str() == Some("stop") {
        StartWarning {
            warning_info: 0,
            warning_duration: 0,
            strobe_duty_cycle: 0,
            strobe_level: 0,
        }
    } else {
        let v: Warning = serde_json::from_value(input.clone())?;
        let mode = WARNINGS
            .iter()
            .position(|m| *m == v.mode)
            .context("unknown warning mode")? as u8;
        ensure!(
            mode != 0 || v.strobe_duty_cycle > 0,
            "use stop to silence the siren"
        );
        ensure!(
            v.duration > 0 && max.is_some_and(|limit| v.duration <= limit),
            "duration exceeds confirmed siren limit"
        );
        ensure!(
            v.level <= 3 && v.strobe_level <= 3 && v.strobe_duty_cycle <= 100,
            "invalid siren intensity"
        );
        StartWarning {
            warning_info: (mode << 4) | (u8::from(v.strobe_duty_cycle > 0) << 2) | v.level,
            warning_duration: v.duration,
            strobe_duty_cycle: v.strobe_duty_cycle,
            strobe_level: v.strobe_level,
        }
    };
    encode(ep, seq, Command::IasWarningDevice(cmd))
}

pub fn squawk(ep: u8, seq: u8, input: &Json) -> Result<Request> {
    let v: Squawk = serde_json::from_value(input.clone())?;
    let mode = match v.mode.as_str() {
        "armed" => 0,
        "disarmed" => 1,
        _ => bail!("invalid squawk mode"),
    };
    ensure!(v.level <= 3, "invalid squawk level");
    encode(
        ep,
        seq,
        Command::IasWarningDevice(IasWarningDevice::Squawk {
            squawk_info: (mode << 4) | (u8::from(v.strobe) << 3) | v.level,
        }),
    )
}

/// Restrict a query to one day and one mode. Expose the returned page as such.
#[derive(Clone, Copy)]
pub struct ScheduleQuery {
    day: u8,
    mode: ScheduleMode,
}

impl ScheduleQuery {
    pub fn parse(input: &Json) -> Result<Self> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Selection {
            day: String,
            mode: String,
        }
        let v: Selection = serde_json::from_value(input.clone())?;
        let index = DAYS
            .iter()
            .position(|d| *d == v.day)
            .context("invalid schedule day")?;
        let mode = match v.mode.as_str() {
            "heat" => ScheduleMode::HEAT,
            "cool" => ScheduleMode::COOL,
            _ => bail!("invalid schedule mode"),
        };
        Ok(Self {
            day: 1 << index,
            mode,
        })
    }
    pub fn request(self, ep: u8, seq: u8) -> Result<Request> {
        encode(
            ep,
            seq,
            Command::Thermostat(Thermostat::GetWeeklySchedule {
                days_to_return: self.day,
                mode_to_return: self.mode,
            }),
        )
    }
    pub fn response(self, frame: Frame<'_>) -> Option<Json> {
        let Command::Thermostat(Thermostat::CurrentWeeklySchedule {
            schedule:
                WeeklySchedule {
                    days,
                    mode,
                    transitions,
                },
        }) = Command::decode(0x0201, frame).ok()?
        else {
            return None;
        };
        if days != self.day || mode != self.mode {
            return None;
        }
        let mut rows = Vec::new();
        let mut previous = None;
        for t in transitions {
            let minute = t.minutes_since_midnight;
            if minute >= 1440 || previous.is_some_and(|n| n >= minute) {
                return None;
            }
            previous = Some(minute);
            let temp = t.heat_setpoint.or(t.cool_setpoint)?;
            if temp < -27315 {
                return None;
            }
            rows.push(json!({"minute":minute,"setpoint":f64::from(temp)/100.}));
        }
        Some(
            json!({"day":DAYS[self.day.trailing_zeros() as usize],"mode":if mode==ScheduleMode::HEAT{"heat"}else{"cool"},"transitions":rows}),
        )
    }
}
