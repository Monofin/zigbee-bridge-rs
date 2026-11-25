//! Philips2 uses CIE xy for effect base colour. Its integer scaling differs
//! from the standard Color Control cluster. Conversion matches the pinned
//! zigbee-herdsman-converters color.ts (MIT, see the profile notices).
use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;

pub(super) fn coordinates(value: &Value) -> Result<[u16; 2]> {
    let number = |key: &str, max: f64| {
        value
            .get(key)
            .and_then(Value::as_f64)
            .filter(|v| (0.0..=max).contains(v))
            .with_context(|| format!("invalid effect color component {key}"))
    };
    let xy = if let Some(text) = value.as_str() {
        rgb_to_xy(hex(text, true)?)
    } else if let Some(fields) = value.as_object() {
        if fields.contains_key("x") || fields.contains_key("y") {
            ensure!(fields.len() == 2, "expected only x and y");
            [number("x", 1.)?, number("y", 1.)?]
        } else if let Some(text) = fields.get("hex").and_then(Value::as_str) {
            ensure!(fields.len() == 1, "expected only hex");
            rgb_to_xy(hex(text, false)?)
        } else if let Some(text) = fields.get("rgb").and_then(Value::as_str) {
            ensure!(fields.len() == 1, "expected only rgb");
            rgb_to_xy(triplet(text, [255., 255., 255.])?.map(|n| n / 255.))
        } else if fields.contains_key("r") || fields.contains_key("g") {
            ensure!(fields.len() == 3, "expected RGB components");
            rgb_to_xy(
                [number("r", 255.)?, number("g", 255.)?, number("b", 255.)?].map(|n| n / 255.),
            )
        } else {
            let (h, s, v) = if let Some(key) = ["hsl", "hsb", "hsv"]
                .into_iter()
                .find(|k| fields.contains_key(*k))
            {
                ensure!(fields.len() == 1, "expected a single color representation");
                let [h, s, v] = triplet(
                    fields[key]
                        .as_str()
                        .context("expected comma-separated components")?,
                    [360., 100., 100.],
                )?;
                hsv(h, s, v, key == "hsl")
            } else {
                let long = fields.contains_key("hue") || fields.contains_key("saturation");
                let (h, s) = if long {
                    ("hue", "saturation")
                } else {
                    ("h", "s")
                };
                ensure!(
                    fields.contains_key(h) || fields.contains_key(s),
                    "expected hex, RGB, xy or hue/saturation"
                );
                let level = if long {
                    "value"
                } else if fields.contains_key("l") {
                    "l"
                } else if fields.contains_key("b") {
                    "b"
                } else {
                    "v"
                };
                ensure!(
                    fields.keys().all(|k| [h, s, level].contains(&k.as_str())),
                    "unknown color component"
                );
                let optional = |key, max, default| {
                    if fields.contains_key(key) {
                        number(key, max)
                    } else {
                        Ok(default)
                    }
                };
                hsv(
                    optional(h, 360., 0.)?,
                    optional(s, 100., 100.)?,
                    optional(level, 100., 100.)?,
                    level == "l",
                )
            };
            let chroma = v * s;
            let sector = (h % 360.) / 60.;
            let x = chroma * (1. - (sector % 2. - 1.).abs());
            let rgb = match sector as u8 {
                0 => [chroma, x, 0.],
                1 => [x, chroma, 0.],
                2 => [0., chroma, x],
                3 => [0., x, chroma],
                4 => [x, 0., chroma],
                _ => [chroma, 0., x],
            };
            rgb_to_xy(rgb.map(|n| n + v - chroma))
        }
    } else {
        bail!("expected a color string or object")
    };
    Ok(xy.map(|v| (v * 65_535.) as u16))
}

fn hsv(h: f64, s: f64, value: f64, lightness: bool) -> (f64, f64, f64) {
    let (s, v) = if lightness {
        let v = value + s * value.min(100. - value) / 100.;
        (if v == 0. { 0. } else { 200. * (1. - value / v) }, v)
    } else {
        (s, value)
    };
    (h, s / 100., v / 100.)
}

fn hex(text: &str, require_hash: bool) -> Result<[f64; 3]> {
    let digits = match text.strip_prefix('#') {
        Some(s) => s,
        None if !require_hash => text,
        None => bail!("expected #RRGGBB"),
    };
    ensure!(
        digits.len() == 6 && digits.bytes().all(|b| b.is_ascii_hexdigit()),
        "expected six hex digits"
    );
    let rgb = u32::from_str_radix(digits, 16)?.to_be_bytes();
    Ok([rgb[1], rgb[2], rgb[3]].map(|v| f64::from(v) / 255.))
}

fn triplet(s: &str, bounds: [f64; 3]) -> Result<[f64; 3]> {
    let mut iter = s.split(',');
    let mut result = [0.; 3];
    for (value, max) in result.iter_mut().zip(bounds) {
        *value = iter
            .next()
            .context("missing color component")?
            .trim()
            .parse()?;
        ensure!(
            (0.0..=max).contains(value),
            "color component outside its range"
        );
    }
    ensure!(iter.next().is_none(), "too many color components");
    Ok(result)
}

fn rgb_to_xy(rgb: [f64; 3]) -> [f64; 2] {
    let [r, g, b] = rgb.map(|n| {
        if n > 0.04045 {
            ((n + 0.055) / 1.055).powf(2.4)
        } else {
            n / 12.92
        }
    });
    let x = 0.664511 * r + 0.154324 * g + 0.162028 * b;
    let y = 0.283881 * r + 0.668433 * g + 0.047685 * b;
    let z = 0.000088 * r + 0.072310 * g + 0.986039 * b;
    let sum = x + y + z;
    if sum == 0. {
        [0., 0.]
    } else {
        [x, y].map(|v| (v / sum * 10_000.).round() / 10_000.)
    }
}
