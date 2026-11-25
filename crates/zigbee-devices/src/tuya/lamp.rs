//! Tuya's two set-only commands on the otherwise standard Color Control server.
use crate::{lighting::Request, standard_controls::COLOR_CONTROL};
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::LazyLock;
use zigbee_zcl::{Direction, Frame, FrameType, Header};

pub const STARTUP: &str = "color_power_on_behavior";
pub const DND: &str = "do_not_disturb";
pub static MODES: LazyLock<[String; 3]> =
    LazyLock::new(|| ["initial", "previous", "customized"].map(String::from));

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Lamp {
    White,
    Colour,
}

#[derive(Clone, Copy)]
pub struct Command(Kind);

#[derive(Clone, Copy)]
enum Kind {
    DoNotDisturb(bool),
    Startup(u8),
}

impl Lamp {
    pub fn supports_color(self) -> bool {
        matches!(self, Self::Colour)
    }
    pub fn command(self, property: &str, value: &Value) -> Result<Command> {
        match property {
            DND => Ok(Command(Kind::DoNotDisturb(
                value.as_bool().context("expected boolean")?,
            ))),
            STARTUP if self.supports_color() => {
                let code = MODES
                    .iter()
                    .position(|m| Some(m.as_str()) == value.as_str())
                    .context("unknown lamp startup mode")?;
                Ok(Command(Kind::Startup(code as u8)))
            }
            _ => bail!("unsupported Tuya lamp setting"),
        }
    }
}

impl Command {
    pub fn id(self) -> u8 {
        match self.0 {
            Kind::DoNotDisturb(_) => 0xfa,
            Kind::Startup(_) => 0xf9,
        }
    }
    pub fn state(self) -> (&'static str, Value) {
        match self.0 {
            Kind::DoNotDisturb(value) => (DND, json!(value)),
            Kind::Startup(code) => (STARTUP, json!(MODES[usize::from(code)])),
        }
    }
    pub fn request(self, endpoint: u8, sequence: u8) -> Request {
        let payload = match self.0 {
            Kind::DoNotDisturb(enabled) => vec![u8::from(enabled)],
            Kind::Startup(mode) => {
                let mut payload = vec![0; 12];
                payload[1] = mode;
                payload
            }
        };
        let mut frame = Vec::new();
        Frame {
            header: Header {
                frame_type: FrameType::ClusterSpecific,
                direction: Direction::ClientToServer,
                manufacturer: None,
                disable_default_response: false,
                sequence,
                command_id: self.id(),
            },
            payload: &payload,
        }
        .encode_into(&mut frame);
        Request {
            endpoint,
            cluster: COLOR_CONTROL,
            frame,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn commands_match_vendor_payloads_and_request_acknowledgement() {
        let custom = Lamp::Colour.command(STARTUP, &json!("customized")).unwrap();
        let wire = custom.request(1, 4).frame;
        assert_eq!(&wire[..5], &[1, 4, 0xf9, 0, 2]);
        assert_eq!(&wire[5..], &[0; 10]);
        assert_eq!(custom.state(), (STARTUP, json!("customized")));
        assert_eq!(
            Lamp::White
                .command(DND, &json!(true))
                .unwrap()
                .request(1, 7)
                .frame,
            [1, 7, 0xfa, 1]
        );
        assert!(Lamp::White.command(STARTUP, &json!("initial")).is_err());
    }
}
