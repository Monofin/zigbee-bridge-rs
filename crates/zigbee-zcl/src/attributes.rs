//! Attribute records shared by reads, writes, reports and structured access.
use crate::{
    codec::{Input, PayloadError, Result, Wire, record},
    value::Value,
};
use alloc::vec::Vec;
use core::num::NonZeroU8;

record!(AttributeValue<'a>{id:u16,value:Value<'a>});
record!(ReadResult<'a>{id:u16,result:Status<Value<'a>>});
record!(DefaultResponse {
    command_id: u8,
    status: u8
});

/// A successful record carries its value. An unsuccessful record ends at the status byte.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status<T> {
    Success(T),
    Error(NonZeroU8),
}
impl<'a, T: Wire<'a>> Wire<'a> for Status<T> {
    fn read(src: &mut Input<'a>) -> Result<Self> {
        match NonZeroU8::new(u8::read(src)?) {
            None => Ok(Self::Success(T::read(src)?)),
            Some(status) => Ok(Self::Error(status)),
        }
    }
    fn write(&self, dst: &mut Vec<u8>) -> Result<()> {
        match self {
            Self::Success(value) => {
                dst.push(0);
                value.write(dst)
            }
            Self::Error(e) => e.write(dst),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure<K> {
    pub status: NonZeroU8,
    pub key: K,
}
impl<'a, K: Wire<'a>> Wire<'a> for Failure<K> {
    fn read(src: &mut Input<'a>) -> Result<Self> {
        Ok(Self {
            status: Wire::read(src)?,
            key: K::read(src)?,
        })
    }
    fn write(&self, dst: &mut Vec<u8>) -> Result<()> {
        self.status.write(dst)?;
        self.key.write(dst)
    }
}
/// All-success has a single zero byte. Any other response lists only failures.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WriteStatus<K> {
    Success,
    Failures(Vec<Failure<K>>),
}
impl<'a, K: Wire<'a>> Wire<'a> for WriteStatus<K> {
    fn read(src: &mut Input<'a>) -> Result<Self> {
        if src.0 == [0] {
            src.take(1)?;
            Ok(Self::Success)
        } else if src.0.is_empty() {
            Err(PayloadError::Truncated)
        } else {
            Ok(Self::Failures(Vec::read(src)?))
        }
    }
    fn write(&self, dst: &mut Vec<u8>) -> Result<()> {
        match self {
            Self::Success => 0u8.write(dst),
            Self::Failures(items) if items.is_empty() => {
                Err(PayloadError::InvalidField("empty failure list"))
            }
            Self::Failures(items) => items.write(dst),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    Replace,
    Add,
    Remove,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Selector {
    pub operation: Operation,
    pub indexes: Vec<u16>,
}
impl<'a> Wire<'a> for Selector {
    fn read(src: &mut Input<'a>) -> Result<Self> {
        let indicator = u8::read(src)?;
        let operation = match indicator >> 4 {
            0 => Operation::Replace,
            1 => Operation::Add,
            2 => Operation::Remove,
            _ => return Err(PayloadError::InvalidField("selector operation")),
        };
        let indexes = (0..(indicator & 15))
            .map(|_| u16::read(src))
            .collect::<Result<_>>()?;
        Ok(Self { operation, indexes })
    }
    fn write(&self, dst: &mut Vec<u8>) -> Result<()> {
        if self.indexes.len() > 15 {
            return Err(PayloadError::InvalidField("selector depth"));
        }
        let op = match self.operation {
            Operation::Replace => 0,
            Operation::Add => 0x10,
            Operation::Remove => 0x20,
        };
        dst.push(op | self.indexes.len() as u8);
        self.indexes.write(dst)
    }
}
record!(SelectedAttribute {
    id: u16,
    selector: Selector
});
record!(StructuredWrite<'a>{attribute:SelectedAttribute,value:Value<'a>});

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StructuredRead {
    pub id: u16,
    pub indexes: Vec<u16>,
}
impl<'a> Wire<'a> for StructuredRead {
    fn read(src: &mut Input<'a>) -> Result<Self> {
        let selected = SelectedAttribute::read(src)?;
        if selected.selector.operation != Operation::Replace {
            return Err(PayloadError::InvalidField("read selector operation"));
        }
        Ok(Self {
            id: selected.id,
            indexes: selected.selector.indexes,
        })
    }
    fn write(&self, dst: &mut Vec<u8>) -> Result<()> {
        self.id.write(dst)?;
        if self.indexes.len() > 15 {
            return Err(PayloadError::InvalidField("selector depth"));
        }
        dst.push(self.indexes.len() as u8);
        self.indexes.write(dst)
    }
}
