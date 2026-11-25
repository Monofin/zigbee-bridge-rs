//! R8 §§6.3.2.3.2 and 6.3.2.4.1. Both weekly schedule commands share one
//! payload. Its mode byte controls every transition in that payload.
use super::Presence;
use crate::codec::{Input, PayloadError, Result, Wire};
use alloc::vec::Vec;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScheduleMode(u8);

impl ScheduleMode {
    pub const HEAT: Self = Self(1);
    pub const COOL: Self = Self(2);
    pub const HEAT_AND_COOL: Self = Self(3);

    /// Reserved bits are retained. Bits zero and one select the setpoint fields.
    pub fn bits(self) -> u8 {
        self.0
    }
}

impl TryFrom<u8> for ScheduleMode {
    type Error = PayloadError;
    fn try_from(bits: u8) -> Result<Self> {
        if bits & Self::HEAT_AND_COOL.0 == 0 {
            return Err(PayloadError::InvalidField("weekly schedule mode"));
        }
        Ok(Self(bits))
    }
}

impl Wire<'_> for ScheduleMode {
    fn read(src: &mut Input<'_>) -> Result<Self> {
        u8::read(src)?.try_into()
    }
    fn write(&self, dst: &mut Vec<u8>) -> Result<()> {
        self.0.write(dst)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScheduleTransition {
    pub minutes_since_midnight: u16,
    /// Setpoints use hundredths of a degree Celsius.
    pub heat_setpoint: Option<i16>,
    pub cool_setpoint: Option<i16>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WeeklySchedule {
    pub days: u8,
    pub mode: ScheduleMode,
    pub transitions: Vec<ScheduleTransition>,
}

impl WeeklySchedule {
    /// R8 requires longer schedules to be split across commands.
    pub const MAX_TRANSITIONS: usize = 10;

    fn count(n: usize) -> Result<u8> {
        if n > Self::MAX_TRANSITIONS {
            return Err(PayloadError::InvalidField("weekly schedule count"));
        }
        Ok(n as u8)
    }
}

impl Wire<'_> for WeeklySchedule {
    fn read(src: &mut Input<'_>) -> Result<Self> {
        let count = Self::count(usize::from(u8::read(src)?))?;
        let days = u8::read(src)?;
        let mode = ScheduleMode::read(src)?;
        let mut transitions = Vec::with_capacity(usize::from(count));
        for _ in 0..count {
            transitions.push(ScheduleTransition {
                minutes_since_midnight: u16::read(src)?,
                heat_setpoint: Presence::Bit(ScheduleMode::HEAT.0).read(mode.0, src)?,
                cool_setpoint: Presence::Bit(ScheduleMode::COOL.0).read(mode.0, src)?,
            });
        }
        Ok(Self {
            days,
            mode,
            transitions,
        })
    }

    fn write(&self, dst: &mut Vec<u8>) -> Result<()> {
        Self::count(self.transitions.len())?.write(dst)?;
        self.days.write(dst)?;
        self.mode.write(dst)?;
        for transition in &self.transitions {
            transition.minutes_since_midnight.write(dst)?;
            Presence::Bit(ScheduleMode::HEAT.0).write(
                self.mode.0,
                &transition.heat_setpoint,
                dst,
            )?;
            Presence::Bit(ScheduleMode::COOL.0).write(
                self.mode.0,
                &transition.cool_setpoint,
                dst,
            )?;
        }
        Ok(())
    }
}
