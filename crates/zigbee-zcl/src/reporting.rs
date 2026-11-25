//! The direction byte selects which configuration fields follow the attribute ID.
use crate::{
    attributes::Failure,
    codec::{Input, PayloadError, Result, Wire, record},
    value::{DataType, Value},
};
use alloc::vec::Vec;
use core::num::NonZeroU8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReportingDirection {
    Reported,
    Received,
}
impl Wire<'_> for ReportingDirection {
    fn read(src: &mut Input<'_>) -> Result<Self> {
        match u8::read(src)? {
            0 => Ok(Self::Reported),
            1 => Ok(Self::Received),
            _ => Err(PayloadError::InvalidField("reporting direction")),
        }
    }
    fn write(&self, dst: &mut Vec<u8>) -> Result<()> {
        dst.push(match self {
            Self::Reported => 0,
            Self::Received => 1,
        });
        Ok(())
    }
}
record!(ReportingKey {
    direction: ReportingDirection,
    id: u16
});

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Configuration<'a> {
    Reported {
        data_type: DataType,
        minimum: u16,
        maximum: u16,
        change: Option<Value<'a>>,
    },
    Received {
        timeout: u16,
    },
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReportingRecord<'a> {
    pub id: u16,
    pub configuration: Configuration<'a>,
}
impl<'a> ReportingRecord<'a> {
    fn read_after_key(key: ReportingKey, src: &mut Input<'a>) -> Result<Self> {
        let configuration = match key.direction {
            ReportingDirection::Received => Configuration::Received {
                timeout: u16::read(src)?,
            },
            ReportingDirection::Reported => {
                let data_type = DataType::read(src)?;
                let minimum = u16::read(src)?;
                let maximum = u16::read(src)?;
                let change = if data_type.is_analog() {
                    Some(Value::read_as(data_type, src)?)
                } else {
                    None
                };
                Configuration::Reported {
                    data_type,
                    minimum,
                    maximum,
                    change,
                }
            }
        };
        Ok(Self {
            id: key.id,
            configuration,
        })
    }
}
impl<'a> Wire<'a> for ReportingRecord<'a> {
    fn read(src: &mut Input<'a>) -> Result<Self> {
        Self::read_after_key(ReportingKey::read(src)?, src)
    }
    fn write(&self, dst: &mut Vec<u8>) -> Result<()> {
        match &self.configuration {
            Configuration::Received { timeout } => {
                ReportingKey {
                    direction: ReportingDirection::Received,
                    id: self.id,
                }
                .write(dst)?;
                timeout.write(dst)
            }
            Configuration::Reported {
                data_type,
                minimum,
                maximum,
                change,
            } => {
                if change.as_ref().map(Value::data_type)
                    != data_type.is_analog().then_some(*data_type)
                {
                    return Err(PayloadError::InvalidField("reportable change type"));
                }
                ReportingKey {
                    direction: ReportingDirection::Reported,
                    id: self.id,
                }
                .write(dst)?;
                data_type.write(dst)?;
                minimum.write(dst)?;
                maximum.write(dst)?;
                if let Some(change) = change {
                    dst.extend_from_slice(change.as_bytes());
                }
                Ok(())
            }
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReportingResult<'a> {
    Success(ReportingRecord<'a>),
    Error(Failure<ReportingKey>),
}
impl<'a> Wire<'a> for ReportingResult<'a> {
    fn read(src: &mut Input<'a>) -> Result<Self> {
        let status = NonZeroU8::new(u8::read(src)?);
        let key = ReportingKey::read(src)?;
        match status {
            Some(status) => Ok(Self::Error(Failure { status, key })),
            None => Ok(Self::Success(ReportingRecord::read_after_key(key, src)?)),
        }
    }
    fn write(&self, dst: &mut Vec<u8>) -> Result<()> {
        match self {
            Self::Success(row) => {
                dst.push(0);
                row.write(dst)
            }
            Self::Error(e) => e.write(dst),
        }
    }
}
