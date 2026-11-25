//! Signify's Philips2 command and state attribute, derived from the pinned
//! zigbee-herdsman-converters philips.ts. See profiles/LICENSE-MIT-converters.
mod color;
use super::{Effects, Request};
use anyhow::{Context, Result, ensure};
use zigbee_zcl::{Direction, Frame, FrameType, Header, foundation::Payload};

pub const CLUSTER: u16 = 0xfc03;
const MANUFACTURER: u16 = 0x100b;

// These four white lamps have the same scene vocabulary. Colour-only scenes
// such as "fireplace" are absent from their product definitions.
pub const NAMES: &[&str] = &[
    "blink",
    "breathe",
    "okay",
    "channel_change",
    "candle",
    "sunset",
    "sunrise",
    "sparkle",
    "opal",
    "glisten",
    "underwater",
    "cosmos",
    "sunbeam",
    "enchant",
    "none",
    "finish_effect",
    "stop_effect",
    "stop_hue_effect",
];
const SCENES: &[(u8, &str)] = &[
    (0, "none"),
    (1, "candle"),
    (9, "sunrise"),
    (10, "sparkle"),
    (11, "opal"),
    (12, "glisten"),
    (13, "sunset"),
    (14, "underwater"),
    (15, "cosmos"),
    (16, "sunbeam"),
    (17, "enchant"),
];

fn command(endpoint: u8, sequence: u8, payload: &[u8]) -> Request {
    let mut frame = Vec::new();
    Frame {
        header: Header {
            frame_type: FrameType::ClusterSpecific,
            manufacturer: Some(MANUFACTURER),
            direction: Direction::ClientToServer,
            disable_default_response: true,
            sequence,
            command_id: 0,
        },
        payload,
    }
    .encode_into(&mut frame);
    Request {
        endpoint,
        cluster: CLUSTER,
        frame,
    }
}

pub fn set(
    endpoint: u8,
    seq: u8,
    effect: Option<&str>,
    speed: Option<f64>,
) -> Result<Vec<Request>> {
    set_fields(endpoint, seq, effect, speed, None, None)
}

/// Reapply only a scene confirmed on this membership. With no active scene
/// Philips2 expects ordinary XY alone; setting an explicit scene takes priority.
pub fn set_fields(
    endpoint: u8,
    seq: u8,
    effect: Option<&str>,
    speed: Option<f64>,
    color: Option<&serde_json::Value>,
    confirmed_scene: Option<u8>,
) -> Result<Vec<Request>> {
    let speed = speed
        .map(|speed| {
            ensure!((0.0..=1.0).contains(&speed), "effect speed outside 0..1");
            Ok::<_, anyhow::Error>((speed * 255.0) as u8)
        })
        .transpose()?;
    let xy = color.map(color::coordinates).transpose()?;
    let mut steps = Vec::new();
    let mut data = vec![0, 0];
    let scene = if let Some(name) = effect {
        ensure!(NAMES.contains(&name), "unknown effect for this Hue lamp");
        match name {
            "blink" | "breathe" | "okay" | "channel_change" => {
                steps.push(Effects::Identify.set(endpoint, seq, name, None)?);
                None
            }
            "stop_hue_effect" | "none" | "stop_effect" | "finish_effect" => Some(0),
            name => {
                let &(id, _) = SCENES
                    .iter()
                    .find(|(_, n)| *n == name)
                    .context("unknown Hue scene")?;
                data[0] |= 1;
                data.push(1);
                Some(id)
            }
        }
    } else {
        confirmed_scene.filter(|id| xy.is_some() && *id != 0 && SCENES.iter().any(|(n, _)| n == id))
    };
    if let Some(coords) = xy {
        data[0] |= 8;
        for component in coords {
            data.extend(component.to_le_bytes());
        }
    }
    if let Some(id) = scene {
        data[0] |= 0x20;
        data.push(id);
    }
    if let Some(v) = speed {
        data[0] |= 0x80;
        data.push(v);
    }
    if data.len() > 2 {
        steps.push(command(
            endpoint,
            seq.wrapping_add(steps.len() as u8),
            &data,
        ));
    }
    if let Some(name @ ("finish_effect" | "stop_effect")) = effect {
        steps.push(Effects::Identify.set(
            endpoint,
            seq.wrapping_add(steps.len() as u8),
            name,
            None,
        )?);
    }
    ensure!(!steps.is_empty(), "missing Hue effect or speed");
    Ok(steps)
}

#[derive(Default, Debug, PartialEq)]
pub struct State {
    pub scene_id: Option<u8>,
    pub coordinates: Option<[u16; 2]>,
    pub effect: Option<&'static str>,
    pub speed: Option<f64>,
}

pub fn report(frame: Frame<'_>) -> Option<State> {
    if frame.header.direction != Direction::ServerToClient
        || frame.header.manufacturer != Some(MANUFACTURER)
    {
        return None;
    }
    let Payload::ReportAttributes(records) = Payload::decode(frame).ok()? else {
        return None;
    };
    let mut matches = records.into_iter().filter(|r| r.id == 2);
    let value = matches.next()?.value;
    if matches.next().is_some() || value.data_type().id() != 0x41 {
        return None;
    }
    let (&length, data) = value.as_bytes().split_first()?;
    if length == 255 || usize::from(length) != data.len() {
        return None;
    }
    decode(data)
}

fn decode(data: &[u8]) -> Option<State> {
    let (flags, mut tail) = data.split_at_checked(2)?;
    let flags = u16::from_le_bytes(flags.try_into().ok()?);
    if flags & !0x01ff != 0 {
        return None;
    }
    let mut state = State::default();
    // Gradient colour data precedes speed and gradient parameters on the wire.
    for (bit, length) in [
        (1, 1),
        (2, 1),
        (4, 2),
        (8, 4),
        (16, 2),
        (32, 1),
        (256, 0),
        (128, 1),
        (64, 2),
    ] {
        if flags & bit == 0 {
            continue;
        }
        let length = if bit == 256 {
            usize::from(*tail.first()?) + 1
        } else {
            length
        };
        let (field, rest) = tail.split_at_checked(length)?;
        match bit {
            8 => {
                state.coordinates = Some([
                    u16::from_le_bytes([field[0], field[1]]),
                    u16::from_le_bytes([field[2], field[3]]),
                ])
            }
            32 => {
                state.scene_id = Some(field[0]);
                state.effect = SCENES
                    .iter()
                    .find(|(n, _)| *n == field[0])
                    .map(|(_, name)| *name);
            }
            128 => state.speed = Some(f64::from(field[0]) / 255.0),
            256 if field.len() < 5 || field.len() != 5 + 3 * usize::from(field[1] >> 4) => {
                return None;
            }
            _ => {}
        }
        tail = rest;
    }
    tail.is_empty().then_some(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scene_and_speed_share_a_command_and_identify_stop_follows_hue_stop() {
        let r = set(11, 4, Some("candle"), Some(0.5)).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].frame, [0x15, 0x0b, 0x10, 4, 0, 0xa1, 0, 1, 1, 127]);
        let stop = set(11, 255, Some("stop_effect"), None).unwrap();
        assert_eq!(stop[0].frame, [0x15, 0x0b, 0x10, 255, 0, 0x20, 0, 0]);
        assert_eq!((stop[1].cluster, stop[1].frame[1]), (3, 0));
        assert!(set(11, 0, Some("fireplace"), None).is_err());
    }
    #[test]
    fn color_formats_and_scene_overrides_have_distinct_wire_encodings() {
        use serde_json::json;
        let red = color::coordinates(&json!("#ff0000")).unwrap();
        assert_eq!(red, [(0.7006 * 65535.) as u16, (0.2993 * 65535.) as u16]);
        for same in [
            json!({"hue":0,"saturation":100}),
            json!({"r":255,"g":0,"b":0}),
            json!({"rgb":"255,0,0"}),
            json!({"hsb":"360,100,100"}),
            json!({"hsl":"0,100,50"}),
            json!({"hex":"FF0000"}),
        ] {
            assert_eq!(color::coordinates(&same).unwrap(), red);
        }
        for bad in [
            json!("#0xyz90"),
            json!({"x":10,"y":0}),
            json!({"h":0,"s":101}),
            json!({"rgb":"10,10"}),
            json!({"r":1,"g":2,"b":-1}),
            json!({"h":0,"s":100,"other":3}),
        ] {
            assert!(color::coordinates(&bad).is_err());
        }
        let color = json!({"x":0.5,"y":0.25});
        for inactive in [None, Some(0), Some(0xf0)] {
            let result = set_fields(11, 5, None, None, Some(&color), inactive).unwrap();
            assert_eq!(&result[0].frame[5..], &[8, 0, 255, 127, 255, 63]);
        }
        let explicit = set_fields(11, 5, Some("sunset"), Some(1.0), Some(&color), Some(1)).unwrap();
        assert_eq!(
            &explicit[0].frame[5..],
            &[0xa9, 0, 1, 255, 127, 255, 63, 13, 255]
        );
        let stop = set_fields(11, 5, Some("none"), None, Some(&color), Some(1)).unwrap();
        assert_eq!(&stop[0].frame[5..], &[0x28, 0, 255, 127, 255, 63, 0]);
        let reported = decode(&[0x28, 0, 255, 127, 255, 63, 0xf0]).unwrap();
        assert_eq!(
            (reported.coordinates, reported.scene_id, reported.effect),
            (Some([32767, 16383]), Some(0xf0), None)
        );
    }
    #[test]
    fn state_requires_a_complete_manufacturer_report() {
        let raw = [0x1c, 0x0b, 0x10, 1, 10, 2, 0, 0x41, 5, 0xa1, 0, 1, 1, 255];
        assert_eq!(
            report(Frame::decode(&raw).unwrap()),
            Some(State {
                effect: Some("candle"),
                scene_id: Some(1),
                coordinates: None,
                speed: Some(1.0)
            })
        );
        for i in 5..raw.len() {
            assert!(report(Frame::decode(&raw[..i]).unwrap()).is_none());
        }
        assert!(decode(&[0x80, 1, 0, 255]).is_none());
    }
}
