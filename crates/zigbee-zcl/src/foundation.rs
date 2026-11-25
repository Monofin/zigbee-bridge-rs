//! ZCL R8 Table 2-3 assigns an identifier and a payload layout to each command.
use crate::{
    Frame, FrameType,
    attributes::{
        AttributeValue, DefaultResponse, ReadResult, SelectedAttribute, StructuredRead,
        StructuredWrite, WriteStatus,
    },
    codec::{Input, PayloadError, Result, Wire},
    discovery::{AttributeInfo, Discover, ExtendedAttributeInfo, Page},
    reporting::{ReportingKey, ReportingRecord, ReportingResult},
};
use alloc::vec::Vec;

// The same schema drives identification, dispatch and encoding. Commands with
// identical payloads keep their distinct IDs and reuse the same Rust type.
macro_rules! commands {
    ($( $id:literal => $name:ident ( $body:ty ) ),+ $(,)?) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        #[repr(u8)]
        #[non_exhaustive]
        pub enum GeneralCommand { $( $name = $id ),+ }
        impl TryFrom<u8> for GeneralCommand {
            type Error = u8;

            fn try_from(id: u8) -> core::result::Result<Self, u8> {
                match id {
                    $( $id => Ok(Self::$name), )+
                    other => Err(other),
                }
            }
        }

        #[derive(Clone, Debug, PartialEq, Eq)]
        #[non_exhaustive]
        pub enum Payload<'a> { $( $name($body) ),+ }

        impl<'a> Payload<'a> {
            pub fn command(&self) -> GeneralCommand {
                match self { $( Self::$name(_) => GeneralCommand::$name, )+ }
            }

            fn read_body(cmd: GeneralCommand, src: &mut Input<'a>) -> Result<Self> {
                match cmd { $( GeneralCommand::$name => Ok(Self::$name(Wire::read(src)?)), )+ }
            }

            fn write_body(&self, dst: &mut Vec<u8>) -> Result<()> {
                match self { $( Self::$name(body) => body.write(dst), )+ }
            }
        }
    }
}
commands! {
    0x00 => ReadAttributes(Vec<u16>),
    0x01 => ReadAttributesResponse(Vec<ReadResult<'a>>),
    0x02 => WriteAttributes(Vec<AttributeValue<'a>>),
    0x03 => WriteAttributesUndivided(Vec<AttributeValue<'a>>),
    0x04 => WriteAttributesResponse(WriteStatus<u16>),
    0x05 => WriteAttributesNoResponse(Vec<AttributeValue<'a>>),
    0x06 => ConfigureReporting(Vec<ReportingRecord<'a>>),
    0x07 => ConfigureReportingResponse(WriteStatus<ReportingKey>),
    0x08 => ReadReportingConfiguration(Vec<ReportingKey>),
    0x09 => ReadReportingConfigurationResponse(Vec<ReportingResult<'a>>),
    0x0a => ReportAttributes(Vec<AttributeValue<'a>>),
    0x0b => DefaultResponse(DefaultResponse),
    0x0c => DiscoverAttributes(Discover<u16>),
    0x0d => DiscoverAttributesResponse(Page<AttributeInfo>),
    0x0e => ReadAttributesStructured(Vec<StructuredRead>),
    0x0f => WriteAttributesStructured(Vec<StructuredWrite<'a>>),
    0x10 => WriteAttributesStructuredResponse(WriteStatus<SelectedAttribute>),
    0x11 => DiscoverCommandsReceived(Discover<u8>),
    0x12 => DiscoverCommandsReceivedResponse(Page<u8>),
    0x13 => DiscoverCommandsGenerated(Discover<u8>),
    0x14 => DiscoverCommandsGeneratedResponse(Page<u8>),
    0x15 => DiscoverAttributesExtended(Discover<u16>),
    0x16 => DiscoverAttributesExtendedResponse(Page<ExtendedAttributeInfo>),
}

impl<'a> Payload<'a> {
    pub fn decode(frame: Frame<'a>) -> Result<Self> {
        if frame.header.frame_type != FrameType::General {
            return Err(PayloadError::NotGeneral);
        }
        let command = GeneralCommand::try_from(frame.header.command_id)
            .map_err(PayloadError::UnassignedCommand)?;
        let mut input = Input(frame.payload);
        if input.0.is_empty() {
            return Err(PayloadError::Truncated);
        }
        let decoded = Self::read_body(command, &mut input)?;
        if !input.0.is_empty() {
            return Err(PayloadError::TrailingBytes);
        }
        Ok(decoded)
    }

    /// Return a complete payload, leaving no partial output on error.
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        self.write_body(&mut bytes)?;
        if bytes.is_empty() {
            return Err(PayloadError::InvalidField("empty command"));
        }
        Ok(bytes)
    }
}
