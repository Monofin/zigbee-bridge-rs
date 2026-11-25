//! Cluster-specific payloads from the pinned catalogue. Dispatch needs the
//! cluster ID from APS as well as both the direction and command ID from ZCL.
use crate::{
    Direction, Frame, FrameType, PayloadError,
    codec::{Input, Result, Wire, record},
};
use alloc::vec::Vec;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Key {
    pub cluster: u16,
    pub direction: Direction,
    pub command: u8,
}

mod fields;
mod schedule;
pub use fields::CountedBytes as BlockData;
pub use fields::{Counted, CountedBytes, ShortString as CharacterString, ShortString as OctetString};
pub use schedule::{ScheduleMode, ScheduleTransition, WeeklySchedule};

#[derive(Clone, Copy)]
enum Presence {
    Equal(u8),
    Bit(u8),
    AtLeast(u8),
    TrailingEqual(u8),
}
impl Presence {
    fn applies(self, control: u8) -> bool {
        match self {
            Self::Equal(n) | Self::TrailingEqual(n) => control == n,
            Self::Bit(mask) => control & mask != 0,
            Self::AtLeast(n) => control >= n,
        }
    }

    fn read<'a, T: Wire<'a>>(self, control: u8, input: &mut Input<'a>) -> Result<Option<T>> {
        if self.applies(control) && !(matches!(self, Self::TrailingEqual(_)) && input.0.is_empty())
        {
            Ok(Some(T::read(input)?))
        } else {
            Ok(None)
        }
    }

    fn write<'a, T: Wire<'a>>(
        self,
        control: u8,
        value: &Option<T>,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        match (self.applies(control), value) {
            (true, Some(value)) => value.write(out),
            (false, None) => Ok(()),
            (true, None) if matches!(self, Self::TrailingEqual(_)) => Ok(()),
            _ => Err(PayloadError::InvalidField(
                "field presence disagrees with discriminator",
            )),
        }
    }
}

macro_rules! field_type {
    ($ty:ty; DefaultZero) => { $ty };
    ($ty:ty) => { $ty };
    ($ty:ty; $($guard:tt)+) => { Option<$ty> };
}
macro_rules! read_field {
    ($src:ident, $ty:ty; Trailing) => {
        if $src.0.is_empty() {
            None
        } else {
            Some(<$ty>::read($src)?)
        }
    };
    ($src:ident, $ty:ty; DefaultZero) => {
        if $src.0.is_empty() {
            0
        } else {
            <$ty>::read($src)?
        }
    };
    ($src:ident, $ty:ty) => {
        <$ty>::read($src)?
    };
    ($src:ident, $ty:ty; $control:ident $kind:ident $arg:literal) => {
        Presence::$kind($arg).read::<$ty>($control, $src)?
    };
}
macro_rules! write_field {
    ($out:ident, $field:ident; Trailing) => {
        if let Some(value) = $field {
            value.write($out)?;
        }
    };
    ($out:ident, $field:ident; DefaultZero) => {
        $field.write($out)?
    };
    ($out:ident, $field:ident) => {
        $field.write($out)?
    };
    ($out:ident, $field:ident; $control:ident $kind:ident $arg:literal) => {
        Presence::$kind($arg).write(*$control, $field, $out)?
    };
}

macro_rules! clusters {
    ($( $cluster:ident = $cid:literal {
        $( $variant:ident = $direction:ident $id:literal {
            $( $field:ident : $ty:ty $( [ $($guard:tt)+ ] )? ),* $(,)?
        } ),+ $(,)?
    } ),+ $(,)?) => {
        $(
            #[derive(Clone, Debug, PartialEq, Eq)]
            #[non_exhaustive]
            pub enum $cluster {
                $( $variant { $( $field: field_type!($ty $(; $($guard)+)?), )* }, )+
            }
        )+

        #[derive(Clone, Debug, PartialEq, Eq)]
        #[non_exhaustive]
        pub enum Command { $( $cluster($cluster), )+ }

        impl Command {
            pub fn name(&self) -> &'static str {
                match self {
                    $($( Self::$cluster($cluster::$variant { .. }) => stringify!($variant), )+)+
                }
            }

            /// Keys with a reviewed R8 wire layout in this build.
            pub const KEYS: &'static [Key] = &[
                $($( Key { cluster: $cid, direction: Direction::$direction, command: $id }, )+)+
            ];

            pub fn key(&self) -> Key {
                match self {
                    $($( Self::$cluster($cluster::$variant { .. }) =>
                        Key { cluster: $cid, direction: Direction::$direction, command: $id }, )+)+
                }
            }

            fn read_body(key: Key, src: &mut Input<'_>) -> Result<Self> {
                match (key.cluster, key.direction, key.command) {
                    $($(
                        ($cid, Direction::$direction, $id) => {
                            $( let $field = read_field!(src, $ty $(; $($guard)+)?); )*
                            Ok(Self::$cluster($cluster::$variant { $( $field, )* }))
                        }
                    )+)+
                    _ => Err(PayloadError::UnsupportedClusterCommand(key)),
                }
            }

            fn write_body(&self, dst: &mut Vec<u8>) -> Result<()> {
                match self {
                    $($( Self::$cluster($cluster::$variant { $( $field, )* }) => {
                        $( write_field!(dst, $field $(; $($guard)+)?); )*
                        Ok(())
                    }, )+)+
                }
            }
        }
    };
}

include!("cluster/generated.rs");

impl Command {
    /// Decode the exact reviewed R8 payload, rejecting unconsumed bytes.
    pub fn decode(cluster: u16, frame: Frame<'_>) -> Result<Self> {
        let (value, rest) = Self::decode_prefix(cluster, frame)?;
        if !rest.is_empty() {
            return Err(PayloadError::TrailingBytes);
        }
        Ok(value)
    }

    /// Decode the known fields and return the remaining bytes for callers
    /// that accept later revisions with appended fields.
    pub fn decode_prefix(cluster: u16, frame: Frame<'_>) -> Result<(Self, &[u8])> {
        if frame.header.frame_type != FrameType::ClusterSpecific {
            return Err(PayloadError::NotClusterSpecific);
        }
        if let Some(code) = frame.header.manufacturer {
            return Err(PayloadError::ManufacturerSpecific(code));
        }
        let key = Key {
            cluster,
            direction: frame.header.direction,
            command: frame.header.command_id,
        };
        let mut input = Input(frame.payload);
        let value = Self::read_body(key, &mut input)?;
        Ok((value, input.0))
    }

    /// Encode a whole payload. On error no partial encoding escapes.
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        self.write_body(&mut bytes)?;
        Ok(bytes)
    }
}
