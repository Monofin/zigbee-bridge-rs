//! Services selected by the matched MCU profile, independently of its datapoint map.
use serde::Deserialize;
use zigbee_zcl::{Direction, Frame, FrameType, Header};

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Service {
    BasicSetup,
    AlternateReports,
    GatewayStatus,
    McuVersion,
}

/// The Basic-cluster read used by the tuyaBase configure handler, in wire order.
pub const SETUP_ATTRIBUTES: [u16; 6] = [4, 0, 1, 5, 7, 0xfffe];

pub fn basic_setup(
    maker: &str,
    model: &str,
    ep: &crate::Endpoint<'_>,
    device_type: u16,
) -> anyhow::Result<bool> {
    if super::Button::resolve(maker, model, ep).is_some() {
        return Ok(true);
    }
    // The reviewed valve configures via Basic but controls its relay via On/Off.
    if (maker, model) == ("_TZ3000_o4cjetlm", "TS0001") {
        return Ok(crate::outlet::Outlets::builtin()?
            .resolve(maker, model, ep, device_type)
            .is_some());
    }
    Ok(super::household::Device::resolve(maker, model, ep)?
        .is_some_and(|d| d.supports(Service::BasicSetup)))
}

pub fn incoming(frame: Frame<'_>) -> Option<Service> {
    let h = frame.header;
    if h.frame_type != FrameType::ClusterSpecific
        || h.direction != Direction::ServerToClient
        || h.manufacturer.is_some()
    {
        return None;
    }
    match h.command_id {
        0x25 => Some(Service::GatewayStatus),
        0x11 => Some(Service::McuVersion),
        _ => None,
    }
}

/// ZHA TuyaConnectionStatus: echo the two MCU sequence bytes and append the status.
pub fn gateway_reply(frame: Frame<'_>, online: bool) -> Option<Vec<u8>> {
    let &[first, second] = frame.payload else {
        return None;
    };
    if incoming(frame) != Some(Service::GatewayStatus) {
        return None;
    }
    let mut wire = Vec::with_capacity(6);
    Frame {
        header: Header {
            direction: Direction::ClientToServer,
            disable_default_response: true,
            ..frame.header
        },
        payload: &[first, second, u8::from(online)],
    }
    .encode_into(&mut wire);
    Some(wire)
}

/// A zero version carries no firmware identification.
pub fn version(raw: u8) -> Option<String> {
    (raw != 0).then(|| format!("{}.{}.{}", raw >> 6, (raw >> 4) & 3, raw & 15))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Endpoint, tuya::household::Device};

    #[test]
    fn status_reply_preserves_both_sequences_and_excludes_other_frame_shapes() {
        let f = Frame::decode(&[0x19, 0x42, 0x25, 0x34, 0x12]).unwrap();
        assert_eq!(
            gateway_reply(f, true).unwrap(),
            [0x11, 0x42, 0x25, 0x34, 0x12, 1]
        );
        assert_eq!(
            gateway_reply(f, false).unwrap(),
            [0x11, 0x42, 0x25, 0x34, 0x12, 0]
        );
        for bytes in [
            &[0x11, 7, 0x25, 1, 2][..],
            &[0x18, 7, 0x25, 1, 2],
            &[0x1d, 1, 2, 7, 0x25, 1, 2],
            &[0x19, 7, 0x25, 1],
            &[0x19, 7, 0x25, 1, 2, 1],
        ] {
            assert!(gateway_reply(Frame::decode(bytes).unwrap(), true).is_none());
        }
        assert_eq!(version(0x98).as_deref(), Some("2.1.8"));
        assert_eq!(version(0), None);
    }

    #[test]
    fn modern_reports_and_commissioning_are_independent_of_cover_and_button_services()
    -> anyhow::Result<()> {
        let ep = Endpoint {
            id: 1,
            profile: 0x104,
            servers: &[0, 6, 0xef00],
            clients: &[],
        };
        let soil = Device::resolve("_TZE284_oitavov2", "TS0601", &ep)?.unwrap();
        let radar = Device::resolve("_TZE204_qasjif9e", "TS0601", &ep)?.unwrap();
        let curtain = Device::resolve("_TZE200_cowvfni3", "TS0601", &ep)?.unwrap();
        for cmd in [5, 6, 10] {
            let bytes = [0x19, 7, cmd, 0, 2, 3, 2, 0, 4, 0, 0, 0, 42];
            let frame = Frame::decode(&bytes)?;
            assert_eq!(soil.observe(frame)?["soil_moisture"], 42);
            assert!(radar.accepts(frame));
            assert!(!curtain.accepts(frame));
            assert!(
                soil.observe(Frame::decode(&bytes[..bytes.len() - 1])?)
                    .is_err()
            );
        }
        let valve = Endpoint {
            id: 1,
            profile: 0x104,
            servers: &[0, 6, 0xe000, 0xe001],
            clients: &[],
        };
        assert!(basic_setup("_TZ3000_o4cjetlm", "TS0001", &valve, 256)?);
        assert!(!basic_setup("_TZ3000_o4cjetlm", "TS0001", &ep, 256)?);
        assert!(basic_setup("_TZ3000_qgwcxxws", "TS0041", &ep, 0)?);
        assert!(basic_setup("_TZE284_oitavov2", "TS0601", &ep, 0)?);
        assert!(!basic_setup("_TZE200_cowvfni3", "TS0601", &ep, 0)?);
        assert!(!basic_setup("unlisted", "TS0601", &ep, 0)?);
        assert!(soil.supports(Service::GatewayStatus) && radar.supports(Service::McuVersion));
        assert!(!curtain.supports(Service::GatewayStatus));
        assert!(curtain.time_epoch().is_some() && soil.time_epoch().is_none());
        Ok(())
    }
}
