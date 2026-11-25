use crate::codec::{Input, PayloadError, Result, Wire};
use alloc::vec::Vec;

/// A list preceded by its element count. All 256 possible counts are meaningful.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Counted<T>(pub Vec<T>);

impl<'a, T: Wire<'a>> Wire<'a> for Counted<T> {
    fn read(src: &mut Input<'a>) -> Result<Self> {
        let count = u8::read(src)?;
        (0..count)
            .map(|_| T::read(src))
            .collect::<Result<_>>()
            .map(Self)
    }

    fn write(&self, dst: &mut Vec<u8>) -> Result<()> {
        let count =
            u8::try_from(self.0.len()).map_err(|_| PayloadError::InvalidField("element count"))?;
        count.write(dst)?;
        self.0.write(dst)
    }
}

/// Octets with an 8-bit length, used by OTA blocks and scene field sets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CountedBytes(pub Vec<u8>);

impl Wire<'_> for CountedBytes {
    fn read(src: &mut Input<'_>) -> Result<Self> {
        let size = u8::read(src)?;
        Ok(Self(src.take(usize::from(size))?.to_vec()))
    }

    fn write(&self, dst: &mut Vec<u8>) -> Result<()> {
        let size =
            u8::try_from(self.0.len()).map_err(|_| PayloadError::InvalidField("block length"))?;
        dst.push(size);
        dst.extend_from_slice(&self.0);
        Ok(())
    }
}

/// Short ZCL octet strings and character strings share the length and non-value encodings.
/// Preserve the bytes for callers that need a particular character encoding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShortString {
    Bytes(Vec<u8>),
    NonValue,
}

impl Wire<'_> for ShortString {
    fn read(src: &mut Input<'_>) -> Result<Self> {
        match u8::read(src)? {
            0xff => Ok(Self::NonValue),
            n => Ok(Self::Bytes(src.take(usize::from(n))?.to_vec())),
        }
    }

    fn write(&self, dst: &mut Vec<u8>) -> Result<()> {
        match self {
            Self::NonValue => dst.push(0xff),
            Self::Bytes(bytes) => {
                let n = u8::try_from(bytes.len())
                    .ok()
                    .filter(|&n| n != 0xff)
                    .ok_or(PayloadError::InvalidField("short string length"))?;
                dst.push(n);
                dst.extend_from_slice(bytes);
            }
        }
        Ok(())
    }
}
