//! Colour temperature on an RGB lamp, using the pinned converter's black-body
//! lookup, 10 K bins and inverse approximation. See profiles/LICENSING.md.
use super::Setting;
use std::ops::RangeInclusive;

const TABLE: &[u8; 1901 * 4] = include_bytes!("../../profiles/kelvin-xy.bin");

pub(super) fn coordinates(mireds: u16) -> Option<(u16, u16)> {
    let kelvin = (1_000_000.0 / f64::from(mireds)).round() as usize;
    let offset = (kelvin.checked_sub(1000)? / 10) * 4;
    let [xl, xh, yl, yh] = *TABLE.get(offset..offset + 4)? else {
        return None;
    };
    Some((u16::from_le_bytes([xl, xh]), u16::from_le_bytes([yl, yh])))
}

pub(super) fn mireds(color: Setting, range: &RangeInclusive<u16>) -> Option<Setting> {
    let Setting::Xy(x, y) = color else {
        return None;
    };
    let x = f64::from(x) / 65535.;
    let y = f64::from(y) / 65535.;
    let n = (x - 0.332) / (0.1858 - y);
    let kelvin = (437.0 * n.powi(3) + 3601.0 * n.powi(2) + 6861.0 * n + 5517.0).abs();
    if kelvin == 0.0 || !kelvin.is_finite() {
        return None;
    }
    Some(Setting::ColorTemperature(
        ((1_000_000.0 / kelvin).round() as u16).clamp(*range.start(), *range.end()),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Endpoint,
        lighting::{Lighting, Property},
    };
    use zigbee_zcl::Frame;

    #[test]
    fn rgb_lamp_uses_xy_for_both_set_and_read() -> anyhow::Result<()> {
        let ep = Endpoint {
            id: 1,
            profile: 260,
            servers: &[0, 3, 6, 8, 0x300],
            clients: &[],
        };
        let lamp = Lighting::new(&ep, Some(15)).with_temperature_via_xy((153, 500))?;
        let get = lamp.get(Property::ColorTemperature, 12)?;
        assert_eq!(get.frame, [0, 12, 0, 8, 0, 3, 0, 4, 0]);
        let r = lamp.set(Setting::ColorTemperature(250), 4, 12)?;
        let (x, y) = (24932u16, 24690u16);
        assert_eq!(&r.frame[..3], &[1, 12, 7]);
        assert_eq!(
            &r.frame[3..7],
            &[x as u8, (x >> 8) as u8, y as u8, (y >> 8) as u8]
        );
        let raw = [
            8,
            12,
            1,
            3,
            0,
            0,
            0x21,
            x as u8,
            (x >> 8) as u8,
            4,
            0,
            0,
            0x21,
            y as u8,
            (y >> 8) as u8,
            8,
            0,
            0,
            0x30,
            1,
        ];
        let read = lamp
            .read_response(Property::ColorTemperature, 12, 0x300, Frame::decode(&raw)?)?
            .unwrap();
        assert_eq!(read, Setting::Xy(x, y));
        assert!(matches!(
            lamp.temperature_from_color(read),
            Some(Setting::ColorTemperature(248..=252))
        ));
        assert!(
            lamp.read_response(Property::ColorTemperature, 13, 0x300, Frame::decode(&raw)?)?
                .is_none()
        );
        assert!(lamp.set(Setting::ColorTemperature(152), 0, 10).is_err());
        assert!(
            Lighting::new(&ep, Some(15))
                .get(Property::ColorTemperature, 0)
                .is_err()
        );
        Ok(())
    }
}
