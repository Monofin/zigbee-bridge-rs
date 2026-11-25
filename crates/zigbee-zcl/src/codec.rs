//! Shared positional fields. A list consumes its enclosing payload.
use alloc::vec::Vec;
use core::{fmt, num::NonZeroU8};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PayloadError {
    NotGeneral,
    NotClusterSpecific,
    ManufacturerSpecific(u16),
    UnsupportedClusterCommand(crate::cluster::Key),
    UnassignedCommand(u8),
    Truncated,
    TrailingBytes,
    UnassignedDataType(u8),
    InvalidField(&'static str),
}
impl fmt::Display for PayloadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotClusterSpecific => f.write_str("expected a cluster-specific command"),
            Self::ManufacturerSpecific(code) => {
                write!(f, "manufacturer-specific namespace {code:#06x}")
            }
            Self::UnsupportedClusterCommand(key) => {
                write!(f, "unsupported cluster command {key:?}")
            }
            Self::NotGeneral => f.write_str("expected a general command"),
            Self::UnassignedCommand(n) => write!(f, "unassigned general command {n:#04x}"),
            Self::UnassignedDataType(n) => write!(f, "unassigned data type {n:#04x}"),
            Self::Truncated => f.write_str("truncated payload"),
            Self::TrailingBytes => f.write_str("unexpected trailing bytes"),
            Self::InvalidField(field) => write!(f, "invalid {field}"),
        }
    }
}
impl core::error::Error for PayloadError {}
pub(crate) type Result<T> = core::result::Result<T, PayloadError>;

pub(crate) struct Input<'a>(pub &'a [u8]);
impl<'a> Input<'a> {
    pub fn take(&mut self, size: usize) -> Result<&'a [u8]> {
        let (head, tail) = self
            .0
            .split_at_checked(size)
            .ok_or(PayloadError::Truncated)?;
        self.0 = tail;
        Ok(head)
    }
}

pub(crate) trait Wire<'a>: Sized {
    fn read(src: &mut Input<'a>) -> Result<Self>;
    fn write(&self, dst: &mut Vec<u8>) -> Result<()>;
}

impl Wire<'_> for u8 {
    fn read(src: &mut Input<'_>) -> Result<Self> {
        Ok(src.take(1)?[0])
    }
    fn write(&self, dst: &mut Vec<u8>) -> Result<()> {
        dst.push(*self);
        Ok(())
    }
}
macro_rules! little_endian {
    ($($ty:ty),+) => {$(
        impl Wire<'_> for $ty {
            fn read(src: &mut Input<'_>) -> Result<Self> {
                let (bytes, rest) = src.0.split_first_chunk().ok_or(PayloadError::Truncated)?;
                src.0 = rest;
                Ok(Self::from_le_bytes(*bytes))
            }
            fn write(&self, dst: &mut Vec<u8>) -> Result<()> {
                dst.extend_from_slice(&self.to_le_bytes());
                Ok(())
            }
        }
    )+};
}
little_endian!(u16, u32, u64, i8, i16);
impl Wire<'_> for bool {
    fn read(src: &mut Input<'_>) -> Result<Self> {
        match u8::read(src)? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(PayloadError::InvalidField("boolean")),
        }
    }
    fn write(&self, dst: &mut Vec<u8>) -> Result<()> {
        u8::from(*self).write(dst)
    }
}
impl Wire<'_> for NonZeroU8 {
    fn read(src: &mut Input<'_>) -> Result<Self> {
        Self::new(u8::read(src)?).ok_or(PayloadError::InvalidField("failure status"))
    }
    fn write(&self, dst: &mut Vec<u8>) -> Result<()> {
        self.get().write(dst)
    }
}
impl<'a, T: Wire<'a>> Wire<'a> for Vec<T> {
    fn read(src: &mut Input<'a>) -> Result<Self> {
        let mut values = Vec::new();
        while !src.0.is_empty() {
            values.push(T::read(src)?);
        }
        Ok(values)
    }
    fn write(&self, dst: &mut Vec<u8>) -> Result<()> {
        for value in self {
            value.write(dst)?;
        }
        Ok(())
    }
}

// Fixed positional records derive both directions from the same field order.
macro_rules! record {
    ($name:ident $(<$life:lifetime>)? { $( $field:ident : $ty:ty ),+ $(,)? }) => {
        #[derive(Clone, Debug, PartialEq, Eq)]
        pub struct $name $(<$life>)? {
            $( pub $field: $ty, )+
        }

        impl<'a> Wire<'a> for $name $(<$life>)? {
            fn read(src: &mut Input<'a>) -> Result<Self> {
                Ok(Self { $( $field: Wire::read(src)?, )+ })
            }
            fn write(&self, dst: &mut Vec<u8>) -> Result<()> {
                $( self.$field.write(dst)?; )+
                Ok(())
            }
        }
    }
}
pub(crate) use record;
