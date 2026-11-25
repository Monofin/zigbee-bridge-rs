//! Confirmed Level Control Options and StartUpCurrentLevel. Options is a
//! bitmap; changing ExecuteIfOff must preserve all other bits.
use super::request;
use crate::lighting::Request;
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Map, Value as Json, json};
use zigbee_zcl::{
    Direction, Frame, FrameType,
    attributes::{AttributeValue, Status},
    foundation::Payload,
    value::Value,
};

pub const PROPERTY: &str = "level_config";
pub const CLUSTER: u16 = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Field {
    Options,
    Startup,
}

impl Field {
    fn id(self) -> u16 {
        match self {
            Self::Options => 0x000f,
            Self::Startup => 0x4000,
        }
    }
    fn ty(self) -> u8 {
        match self {
            Self::Options => 0x18,
            Self::Startup => 0x20,
        }
    }
    pub fn read(self, ep: u8, seq: u8) -> Result<Request> {
        Self::read_fields(ep,seq,&[self])
    }

    pub fn read_both(ep:u8,seq:u8)->Result<Request> {
        Self::read_fields(ep,seq,&[Self::Options,Self::Startup])
    }

    fn read_fields(ep:u8,seq:u8,fields:&[Self])->Result<Request> {
        let payload = Payload::ReadAttributes(fields.iter().map(|f|f.id()).collect());
        Ok(request(
            ep,
            seq,
            CLUSTER,
            FrameType::General,
            payload.command() as u8,
            &payload.encode()?,
        ))
    }
    fn parse(self, value: Value<'_>) -> Option<u8> {
        let [value_byte] = *value.as_bytes() else {
            return None;
        };
        (value.data_type().id() == self.ty()).then_some(value_byte)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Patch {
    pub execute_if_off: Option<bool>,
    startup: Option<u8>,
}
impl Patch {
    pub fn parse(input: &Json) -> Result<Self> {
        let object = input
            .as_object()
            .context("level_config requires an object")?;
        ensure!(
            !object.is_empty()
                && object
                    .keys()
                    .all(|k| matches!(k.as_str(), "execute_if_off" | "current_level_startup")),
            "unknown or empty level configuration"
        );
        let execute_if_off = object
            .get("execute_if_off")
            .map(|v| v.as_bool().context("execute_if_off requires a boolean"))
            .transpose()?;
        let startup = object
            .get("current_level_startup")
            .map(|v| {
                Ok(match v.as_str() {
                    Some("previous") => 255,
                    Some("minimum") => 0,
                    Some(_) => bail!("unknown startup level"),
                    None => u8::try_from(v.as_u64().context("startup level requires an integer")?)?,
                })
            })
            .transpose()?;
        Ok(Self {
            execute_if_off,
            startup,
        })
    }
    pub fn write(self, ep: u8, seq: u8, previous_options: Option<u8>) -> Result<Request> {
        let options = match self.execute_if_off {
            Some(enabled) => {
                Some((previous_options.context("options read required")? & !1) | u8::from(enabled))
            }
            None => None,
        };
        let raw = [
            options.unwrap_or_default(),
            self.startup.unwrap_or_default(),
        ];
        let mut attrs = Vec::new();
        for (index, (field, value)) in [(Field::Options, options), (Field::Startup, self.startup)]
            .into_iter()
            .enumerate()
        {
            if value.is_some() {
                attrs.push(AttributeValue {
                    id: field.id(),
                    value: Value::from_bytes(field.ty().try_into()?, &raw[index..=index])?,
                });
            }
        }
        let cmd = Payload::WriteAttributes(attrs);
        Ok(request(
            ep,
            seq,
            CLUSTER,
            FrameType::General,
            cmd.command() as u8,
            &cmd.encode()?,
        ))
    }
}

#[derive(Clone, Copy, Default)]
pub struct State {
    options: Option<u8>,
    startup: Option<u8>,
}
impl State {
    pub fn options(self) -> Option<u8> {
        self.options
    }
    pub fn merge(&mut self, observed: Self) -> Option<Json> {
        if observed.options.is_none() && observed.startup.is_none() {
            return None;
        }
        self.options = observed.options.or(self.options);
        self.startup = observed.startup.or(self.startup);
        let mut fields = Map::new();
        if let Some(options) = self.options {
            fields.insert("execute_if_off".into(), json!(options & 1 != 0));
        }
        if let Some(n) = self.startup {
            fields.insert(
                "current_level_startup".into(),
                match n {
                    0 => json!("minimum"),
                    255 => json!("previous"),
                    _ => json!(n),
                },
            );
        }
        Some(fields.into())
    }

    /// A read response contributes only the requested attribute. A missing
    /// or failed Options read cannot start a read-modify-write.
    pub fn observe(frame: Frame<'_>, expected: Option<Field>) -> Result<Option<Self>> {
        Self::observe_fields(frame,expected.as_ref().map(std::slice::from_ref))
    }

    pub fn observe_readback(frame:Frame<'_>)->Result<Option<Self>> {
        Self::observe_fields(frame,Some(&[Field::Options,Field::Startup]))
    }

    fn observe_fields(frame:Frame<'_>,expected:Option<&[Field]>)->Result<Option<Self>> {
        if frame.header.direction != Direction::ServerToClient
            || frame.header.manufacturer.is_some()
        {
            return Ok(None);
        }
        let mut state = Self::default();
        let mut set = |id, value| -> Result<()> {
            for (field, slot) in [
                (Field::Options, &mut state.options),
                (Field::Startup, &mut state.startup),
            ] {
                if id == field.id() && expected.is_none_or(|fields| fields.contains(&field)) {
                    let n = field.parse(value).context("invalid level setting type")?;
                    ensure!(slot.replace(n).is_none(), "duplicate level setting");
                }
            }
            Ok(())
        };
        match Payload::decode(frame)? {
            Payload::ReportAttributes(values) if expected.is_none() => {
                for r in values {
                    set(r.id, r.value)?;
                }
            }
            Payload::ReadAttributesResponse(values) if expected.is_some() => {
                let mut matched=false;
                for field in expected.unwrap() {
                    let mut rows=values.iter().filter(|r|r.id==field.id());
                    matched|=rows.next().is_some();
                    ensure!(rows.next().is_none(),"duplicate level configuration response");
                }
                if !matched {return Ok(None);}
                for r in values {
                    if let Status::Success(v) = r.result {
                        set(r.id, v)?;
                    }
                }
            }
            _ => return Ok(None),
        }
        Ok(Some(state))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn combined_readback_uses_only_confirmed_rows_and_preserves_partial_results()->Result<()>{
        let request=Field::read_both(4,23)?;
        assert_eq!(request.frame,[0x10,23,0,15,0,0,0x40]);
        let frame=Frame::decode(&[0x18,23,1,15,0,0x86,0,0x40,0,0x20,77])?;
        assert_eq!(State::observe_readback(frame)?.unwrap().startup,Some(77));
        let unrelated=Frame::decode(&[0x18,23,1,1,0,0x86])?;
        assert!(State::observe_readback(unrelated)?.is_none());
        let duplicate=Frame::decode(&[0x18,23,1,15,0,0x86,15,0,0,0x18,1])?;
        assert!(State::observe_readback(duplicate).is_err());
        Ok(())
    }
    #[test]
    fn options_patch_preserves_unrelated_bits_and_startup_sentinels() -> Result<()> {
        let patch =
            Patch::parse(&json!({"execute_if_off":false,"current_level_startup":"previous"}))?;
        assert!(patch.write(1, 10, None).is_err());
        assert_eq!(
            patch.write(1, 10, Some(0x83))?.frame,
            [0x10, 10, 2, 15, 0, 0x18, 0x82, 0, 0x40, 0x20, 255]
        );
        let startup = Patch::parse(&json!({"current_level_startup":"minimum"}))?;
        assert_eq!(
            startup.write(1, 10, None)?.frame,
            [0x10, 10, 2, 0, 0x40, 0x20, 0]
        );
        for invalid in [
            json!({}),
            json!({"options":2}),
            json!({"execute_if_off":2}),
            json!({"current_level_startup":256}),
            json!({"current_level_startup":1.5}),
        ] {
            assert!(Patch::parse(&invalid).is_err());
        }
        Ok(())
    }

    #[test]
    fn independent_attribute_reads_and_reports_merge_confirmed_fields_only() -> Result<()> {
        let read = [8, 12, 1, 15, 0, 0, 0x18, 2];
        let unsolicited = [8, 13, 10, 0, 0x40, 0x20, 255];
        let frame = Frame::decode(&read)?;
        let mut current = State::default();
        assert!(State::observe(frame, None)?.is_none());
        assert!(State::observe(frame, Some(Field::Startup))?.is_none());
        let snapshot = State::observe(frame, Some(Field::Options))?.unwrap();
        assert_eq!(snapshot.options(), Some(2));
        assert_eq!(
            current.merge(snapshot),
            Some(json!({"execute_if_off":false}))
        );
        assert_eq!(
            current.merge(State::observe(Frame::decode(&unsolicited)?, None)?.unwrap()),
            Some(json!({"execute_if_off":false,"current_level_startup":"previous"}))
        );
        assert!(
            current
                .merge(
                    State::observe(
                        Frame::decode(&[8, 12, 1, 15, 0, 0x86])?,
                        Some(Field::Options)
                    )?
                    .unwrap()
                )
                .is_none()
        );
        assert!(State::observe(Frame::decode(&[8, 12, 10, 15, 0, 0x20, 1])?, None).is_err());
        Ok(())
    }
}
