//! R8 Table 2-11. Typed value boundaries are checked while the original wire
//! representation (including non-values, NaNs and odd-width integers) is retained.
use crate::codec::{Input, PayloadError, Result, Wire};
use alloc::vec::Vec;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DataType(u8);

impl DataType {
    pub fn id(self) -> u8 {
        self.0
    }
    pub fn is_analog(self) -> bool {
        matches!(self.0,0x20..=0x2f|0x38..=0x3a|0xe0..=0xe2)
    }
    fn fixed(self) -> Option<usize> {
        Some(match self.0 {
            0 | 0xff => 0,
            0x08..=0x0f | 0x18..=0x2f => usize::from(self.0 & 7) + 1,
            0x10 | 0x30 => 1,
            0x31 | 0x38 | 0xe8 | 0xe9 => 2,
            0x39 | 0xe0..=0xe2 | 0xea => 4,
            0x3a | 0xf0 => 8,
            0xf1 => 16,
            _ => return None,
        })
    }
}

impl TryFrom<u8> for DataType {
    type Error = PayloadError;
    fn try_from(id: u8) -> Result<Self> {
        let data_type = Self(id);
        if data_type.fixed().is_some() || matches!(id, 0x41..=0x44 | 0x48 | 0x4c | 0x50 | 0x51) {
            Ok(data_type)
        } else {
            Err(PayloadError::UnassignedDataType(id))
        }
    }
}
impl Wire<'_> for DataType {
    fn read(src: &mut Input<'_>) -> Result<Self> {
        Self::try_from(u8::read(src)?)
    }
    fn write(&self, dst: &mut Vec<u8>) -> Result<()> {
        self.0.write(dst)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Value<'a> {
    data_type: DataType,
    bytes: &'a [u8],
}
impl core::fmt::Debug for Value<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Value")
            .field("data_type", &self.data_type)
            .field("octets", &self.bytes.len())
            .finish()
    }
}
impl<'a> Value<'a> {
    /// Read a type tag and one value at the start of a vendor record stream.
    pub fn split_typed(bytes: &'a [u8]) -> Result<(Self, &'a [u8])> {
        let mut input = Input(bytes);
        let value = Self::read(&mut input)?;
        Ok((value, input.0))
    }

    pub fn from_bytes(data_type: DataType, bytes: &'a [u8]) -> Result<Self> {
        let mut src = Input(bytes);
        let value = Self::read_as(data_type, &mut src)?;
        if !src.0.is_empty() {
            return Err(PayloadError::TrailingBytes);
        }
        Ok(value)
    }
    pub fn data_type(&self) -> DataType {
        self.data_type
    }
    pub fn as_bytes(&self) -> &'a [u8] {
        self.bytes
    }

    pub(crate) fn read_as(data_type: DataType, src: &mut Input<'a>) -> Result<Self> {
        let original = src.0;
        // A structure supplies each member's type on the wire. Homogeneous
        // collections supply it once. Keep nesting on the heap, off the call stack.
        let mut parents = Vec::new();
        let mut group = (Some(data_type), 1u16);
        loop {
            let (element, count) = group;
            if count == 0 {
                match parents.pop() {
                    Some(parent) => {
                        group = parent;
                        continue;
                    }
                    None => break,
                }
            }
            if let Some(width) = element.and_then(DataType::fixed) {
                let size = width
                    .checked_mul(usize::from(count))
                    .ok_or(PayloadError::Truncated)?;
                src.take(size)?;
                group.1 = 0;
                continue;
            }
            group.1 -= 1;
            let kind = match element {
                Some(t) => t,
                None => DataType::read(src)?,
            };
            if let Some(width) = kind.fixed() {
                src.take(width)?;
                continue;
            }
            match kind.id() {
                0x41 | 0x42 => {
                    let n = u8::read(src)?;
                    if n != u8::MAX {
                        src.take(usize::from(n))?;
                    }
                }
                0x43 | 0x44 => {
                    let n = u16::read(src)?;
                    if n != u16::MAX {
                        src.take(usize::from(n))?;
                    }
                }
                _ => {
                    let child = if kind.id() == 0x4c {
                        None
                    } else {
                        Some(DataType::read(src)?)
                    };
                    let n = u16::read(src)?;
                    if n != 0 && n != u16::MAX {
                        if group.1 != 0 {
                            parents.push(group);
                        }
                        group = (child, n);
                    }
                }
            }
        }
        Ok(Self {
            data_type,
            bytes: &original[..original.len() - src.0.len()],
        })
    }
}
impl<'a> Wire<'a> for Value<'a> {
    fn read(src: &mut Input<'a>) -> Result<Self> {
        Self::read_as(DataType::read(src)?, src)
    }
    fn write(&self, dst: &mut Vec<u8>) -> Result<()> {
        self.data_type.write(dst)?;
        dst.extend_from_slice(self.bytes);
        Ok(())
    }
}
