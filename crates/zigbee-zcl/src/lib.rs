//! Zigbee Cluster Library R8 wire formats. Callers provide a complete APS
//! payload. Network addresses, transport state and device policy belong to
//! the caller. Frame decoding borrows the payload and never allocates.
//!
//! ```
//! use zigbee_zcl::{Frame, foundation::Payload};
//!
//! let packet = [0x00, 0x2a, 0x00, 0x34, 0x12];
//! let frame = Frame::decode(&packet)?;
//! let command = Payload::decode(frame)?;
//! assert_eq!(command, Payload::ReadAttributes(vec![0x1234]));
//! assert_eq!(command.encode()?, frame.payload);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod cluster;
pub mod device_type;
mod codec;
pub use codec::PayloadError;
pub mod attributes;
pub mod discovery;
pub mod foundation;
pub mod reporting;
pub mod standard_attributes;
pub mod value;

use alloc::vec::Vec;
use core::fmt;
use foundation::GeneralCommand;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameType {
    General,
    ClusterSpecific,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    ClientToServer,
    ServerToClient,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub frame_type: FrameType,
    pub manufacturer: Option<u16>,
    pub direction: Direction,
    pub disable_default_response: bool,
    pub sequence: u8,
    pub command_id: u8,
}

impl Header {
    /// The command byte of a cluster-specific frame has a separate namespace.
    pub fn general_command(self) -> Option<GeneralCommand> {
        match self.frame_type {
            FrameType::General => GeneralCommand::try_from(self.command_id).ok(),
            FrameType::ClusterSpecific => None,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum FrameError {
    TruncatedHeader,
    ReservedControl(u8),
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TruncatedHeader => f.write_str("truncated ZCL frame header"),
            Self::ReservedControl(byte) => write!(f, "reserved R8 frame control {byte:#04x}"),
        }
    }
}

impl core::error::Error for FrameError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frame<'a> {
    pub header: Header,
    pub payload: &'a [u8],
}

impl<'a> Frame<'a> {
    /// Decode the R8 header. Unassigned command identifiers and their payload
    /// remain available to captures and extensions.
    pub fn decode(bytes: &'a [u8]) -> Result<Self, FrameError> {
        let (&control, rest) = bytes.split_first().ok_or(FrameError::TruncatedHeader)?;
        let frame_type = match control & 0xe3 {
            0 => FrameType::General,
            1 => FrameType::ClusterSpecific,
            _ => return Err(FrameError::ReservedControl(control)),
        };
        let (manufacturer, rest) = if control & 0x04 != 0 {
            let (code, rest) = rest
                .split_first_chunk::<2>()
                .ok_or(FrameError::TruncatedHeader)?;
            (Some(u16::from_le_bytes(*code)), rest)
        } else {
            (None, rest)
        };
        let (&[sequence, command_id], payload) = rest
            .split_first_chunk::<2>()
            .ok_or(FrameError::TruncatedHeader)?;
        let direction = if control & 8 == 0 {
            Direction::ClientToServer
        } else {
            Direction::ServerToClient
        };
        Ok(Self {
            header: Header {
                frame_type,
                manufacturer,
                direction,
                disable_default_response: control & 0x10 != 0,
                sequence,
                command_id,
            },
            payload,
        })
    }

    pub fn encode_into(&self, out: &mut Vec<u8>) {
        let h = self.header;
        let mut control = u8::from(h.frame_type == FrameType::ClusterSpecific);
        control |= u8::from(h.manufacturer.is_some()) << 2;
        control |= u8::from(h.direction == Direction::ServerToClient) << 3;
        control |= u8::from(h.disable_default_response) << 4;
        out.push(control);
        if let Some(code) = h.manufacturer {
            out.extend_from_slice(&code.to_le_bytes());
        }
        out.extend_from_slice(&[h.sequence, h.command_id]);
        out.extend_from_slice(self.payload);
    }
}
