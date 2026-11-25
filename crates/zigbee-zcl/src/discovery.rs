//! Discovery records use the same page layout for every element type.
use crate::codec::{Input, Result, Wire, record};
use alloc::vec::Vec;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Discover<I> {
    pub start: I,
    pub maximum: u8,
}
impl<'a, I: Wire<'a>> Wire<'a> for Discover<I> {
    fn read(src: &mut Input<'a>) -> Result<Self> {
        Ok(Self {
            start: I::read(src)?,
            maximum: u8::read(src)?,
        })
    }
    fn write(&self, dst: &mut Vec<u8>) -> Result<()> {
        self.start.write(dst)?;
        self.maximum.write(dst)
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Page<I> {
    pub complete: bool,
    pub entries: Vec<I>,
}
impl<'a, I: Wire<'a>> Wire<'a> for Page<I> {
    fn read(src: &mut Input<'a>) -> Result<Self> {
        Ok(Self {
            complete: bool::read(src)?,
            entries: Vec::read(src)?,
        })
    }
    fn write(&self, dst: &mut Vec<u8>) -> Result<()> {
        self.complete.write(dst)?;
        self.entries.write(dst)
    }
}
record!(AttributeInfo {
    id: u16,
    data_type: u8
});
record!(ExtendedAttributeInfo {
    attribute: AttributeInfo,
    access: u8
});
