//! The pinned Innr definitions select light_onoff_brightness with
//! turnsOffAtBrightness1. The quirk changes the outgoing absolute level only.
use super::{Endpoint, Setting};
use anyhow::{Result, ensure};

/// Provenance and regeneration are in tools/devices/innr_levels.py.
const MODELS: &str = include_str!("../../profiles/innr-level-one.txt");

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LightingQuirk(());

impl LightingQuirk {
    pub fn resolve(
        manufacturer: &str,
        model: &str,
        ep: &Endpoint<'_>,
        device_type: u16,
    ) -> Option<Self> {
        (manufacturer == "innr"
            && MODELS.lines().any(|name| name == model)
            && ep.id == 1
            && ep.profile == 0x0104
            && device_type == 0x0101
            && [0, 6, 8].iter().all(|c| ep.servers.contains(c)))
        .then_some(Self(()))
    }

    pub(super) fn absolute_level(self, level: u8) -> u8 {
        if level == 1 { 2 } else { level }
    }

    /// Both state and brightness were explicitly supplied. The caller validates
    /// both fields before invoking this, and sends exactly one command.
    pub fn combined_setting(
        self,
        on: bool,
        level: u8,
        transition_specified: bool,
    ) -> Result<Setting> {
        ensure!(level < 255, "reserved brightness");
        Ok(if on {
            Setting::Brightness(self.absolute_level(level.max(1)))
        } else if level == 0 || transition_specified {
            Setting::Brightness(0)
        } else {
            Setting::State(false)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lighting::{Lighting, Property};
    use zigbee_zcl::{Frame, FrameType};

    #[test]
    fn selectors_require_basic_identity_and_dimmable_server_on_endpoint_one() {
        let ep = Endpoint {
            id: 1,
            profile: 0x104,
            servers: &[0, 6, 8],
            clients: &[],
        };
        for model in ["BF 263", "PL 110", "UC 110"] {
            assert!(LightingQuirk::resolve("innr", model, &ep, 0x101).is_some());
        }
        assert!(LightingQuirk::resolve("Innr", "BF 263", &ep, 0x101).is_none());
        assert!(LightingQuirk::resolve("innr", "BF 265", &ep, 0x101).is_none());
        assert!(LightingQuirk::resolve("innr", "BF 263", &ep, 0x102).is_none());
        for ep in [
            Endpoint { id: 2, ..ep },
            Endpoint {
                servers: &[0, 6],
                clients: &[8],
                ..ep
            },
            Endpoint {
                profile: 0xc05e,
                ..ep
            },
        ] {
            assert!(LightingQuirk::resolve("innr", "BF 263", &ep, 0x101).is_none());
        }
    }

    #[test]
    fn floor_applies_only_to_absolute_set_not_observations() -> anyhow::Result<()> {
        let ep = Endpoint {
            id: 1,
            profile: 0x104,
            servers: &[0, 6, 8],
            clients: &[],
        };
        let quirk = LightingQuirk::resolve("innr", "BF 263", &ep, 0x101).unwrap();
        let lamp = Lighting::new(&ep, None).with_quirk(quirk);
        for (setting, actual) in [(0, 0), (1, 2), (2, 2), (254, 254)] {
            let op = lamp.set(Setting::Brightness(setting), 7, 13)?;
            let frame = Frame::decode(&op.frame)?;
            assert_eq!(frame.header.command_id, 4);
            assert_eq!(frame.payload, [actual, 7, 0, 0, 0]);
        }
        let report = Frame::decode(&[8, 7, 10, 0, 0, 0x20, 1])?;
        let read = Frame::decode(&[8, 8, 1, 0, 0, 0, 0x20, 1])?;
        assert_eq!(lamp.observations(8, report)?, [Setting::Brightness(1)]);
        assert_eq!(
            lamp.read_response(Property::Brightness, 8, 8, read)?,
            Some(Setting::Brightness(1))
        );
        assert_eq!(lamp.get(Property::Brightness, 10)?.frame, [0, 10, 0, 0, 0]);

        for level in [0, 1] {
            assert_eq!(
                quirk.combined_setting(true, level, false)?,
                Setting::Brightness(2)
            );
        }
        for (level, transition, expected) in [
            (0, false, Setting::Brightness(0)),
            (19, true, Setting::Brightness(0)),
            (19, false, Setting::State(false)),
        ] {
            assert_eq!(quirk.combined_setting(false, level, transition)?, expected);
        }
        assert!(quirk.combined_setting(false, 255, false).is_err());
        let plain = Lighting::new(&ep, None).set(Setting::Brightness(1), 0, 15)?;
        let frame = Frame::decode(&plain.frame)?;
        assert_eq!(
            (frame.header.frame_type, frame.payload[0]),
            (FrameType::ClusterSpecific, 1)
        );
        Ok(())
    }
}
