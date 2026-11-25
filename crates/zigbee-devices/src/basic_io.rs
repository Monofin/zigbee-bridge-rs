//! PresentValue on the nine Basic I/O clusters (ZCL R8 §3.14).
use anyhow::{Context, Result, ensure};
use serde_json::{Value as Json, json};
use zigbee_zcl::value::{DataType, Value};

pub const PRESENT: u16 = 0x0055;
const KEYS: [&str; 9] = [
    "analog_input",
    "analog_output",
    "analog_value",
    "binary_input",
    "binary_output",
    "binary_value",
    "multistate_input",
    "multistate_output",
    "multistate_value",
];
const LABEL_KEYS: [&str; 6] = [
    "binary_input_label",
    "binary_output_label",
    "binary_value_label",
    "multistate_input_label",
    "multistate_output_label",
    "multistate_value_label",
];

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Layout {
    cluster: u16,
    pub writable: bool,
    pub unit: Option<&'static str>,
    pub bounds: Option<(f64, f64)>,
    pub resolution: Option<f64>,
    pub count: Option<u16>,
}

#[derive(Clone, Debug)]
pub struct Channel {
    pub layout: Layout,
    labels: Vec<String>,
    minimum: Option<f64>,
    maximum: Option<f64>,
    in_service: bool,
    pub reporting_type: Option<u8>,
}

pub fn supported(cluster: u16) -> bool {
    (0x000c..=0x0014).contains(&cluster)
}

pub fn probe(cluster: u16) -> &'static [u16] {
    match cluster {
        0x000c | 0x000d => &[PRESENT, 0x51, 0x41, 0x45, 0x6a, 0x75, 0x100],
        0x000e => &[PRESENT, 0x51, 0x75, 0x100],
        0x000f..=0x0011 => &[PRESENT, 0x51, 4, 0x2e],
        0x0012..=0x0014 => &[PRESENT, 0x51, 0x4a, 0x0e],
        _ => &[],
    }
}

pub fn output(cluster: u16) -> bool {
    matches!(cluster, 0x000d | 0x000e | 0x0010 | 0x0011 | 0x0013 | 0x0014)
}

fn ty(cluster: u16) -> u8 {
    match cluster {
        0x000c..=0x000e => 0x39,
        0x000f..=0x0011 => 0x10,
        _ => 0x21,
    }
}

impl Layout {
    pub fn cluster(self) -> u16 {
        self.cluster
    }
    pub fn property(self) -> &'static str {
        KEYS[usize::from(self.cluster - 0x000c)]
    }
}

// ZCL §3.14.11.10 references BACnet engineering units. Keep an unfamiliar unit
// unlabelled. ApplicationType can override EngineeringUnits.
fn unit(code: u16) -> Option<&'static str> {
    Some(match code {
        3 => "A",
        5 => "V",
        18 => "Wh",
        19 => "kWh",
        27 => "Hz",
        29 => "%",
        37 => "lx",
        47 => "W",
        48 => "kW",
        53 => "Pa",
        54 => "kPa",
        62 => "°C",
        63 => "K",
        64 => "°F",
        98 => "%",
        _ => return None,
    })
}

// The application group and type determine the units independently of the
// usage index (ZCL R8 §3.14.11.19). Reserved, vendor and "other" types leave
// EngineeringUnits in effect. A defined unitless type suppresses that fallback.
fn engineering_unit(
    cluster: u16,
    application: Option<(u8, &[u8])>,
    fallback: Option<u16>,
) -> Option<&'static str> {
    let fallback = fallback.and_then(unit);
    let Some((0x23, [_, _, kind, group])) = application else {
        return fallback;
    };
    if u16::from(*group) != cluster - 0x000c {
        return fallback;
    }
    match (group, kind) {
        (0 | 1, 0) | (2, 0) => Some("°C"),
        (0 | 1, 1 | 4) => Some("%"),
        (0 | 1, 2) => Some("Pa"),
        (0 | 1, 3) | (2, 3) => Some("L/s"),
        (0 | 1, 5) => Some("ppm"),
        (0 | 1, 6) => Some("rpm"),
        (0 | 1, 7) => Some("A"),
        (0 | 1, 8) => Some("Hz"),
        (0 | 1, 9) => Some("W"),
        (0 | 1, 10) => Some("kW"),
        (0 | 1, 11) => Some("kWh"),
        (0 | 1, 12) | (2, 2) => None,
        (0 | 1, 13) => Some("kJ/kg"),
        (0 | 1, 14) => Some("s"),
        (2, 1) => Some("m²"),
        _ => fallback,
    }
}

fn label_text(bytes: &[u8]) -> Option<String> {
    if bytes.is_empty() || bytes.len() > 16 || !bytes.iter().all(|c| (0x20..=0x7e).contains(c)) {
        return None;
    }
    Some(std::str::from_utf8(bytes).ok()?.to_owned())
}

fn f32_value(bytes: &[u8]) -> Option<f64> {
    let value = f32::from_le_bytes(bytes.try_into().ok()?);
    value.is_finite().then_some(f64::from(value))
}

fn labels(bytes: &[u8], count: usize) -> Option<Vec<String>> {
    let [0x42, lo, hi, body @ ..] = bytes else {
        return None;
    };
    let mut rest = body;
    if usize::from(u16::from_le_bytes([*lo, *hi])) != count || count > rest.len() {
        return None;
    }
    let mut values = Vec::with_capacity(count);
    for _ in 0..count {
        let (n, next) = rest.split_first()?;
        if *n == 255 {
            return None;
        }
        let (text, next) = next.split_at_checked(usize::from(*n))?;
        values.push(label_text(text)?);
        rest = next;
    }
    rest.is_empty().then_some(values)
}

impl Channel {
    pub fn resolve<'a>(
        cluster: u16,
        access_control: Option<(u8, u8)>,
        attribute: impl Fn(u16) -> Option<(u8, &'a [u8])>,
    ) -> Option<Self> {
        if !supported(cluster) {
            return None;
        }
        let bytes = |id, expected| {
            attribute(id)
                .filter(|(t, _)| *t == expected)
                .map(|(_, v)| v)
        };
        let float = |id| f32_value(bytes(id, 0x39)?);
        let word = |id, t| Some(u16::from_le_bytes(bytes(id, t)?.try_into().ok()?));
        let minimum = float(0x45);
        let maximum = float(0x41);
        let bounds = minimum.zip(maximum);
        let valid_range = [(0x45, minimum), (0x41, maximum)]
            .into_iter()
            .all(|(id, value)| attribute(id).is_none() || value.is_some())
            && bounds.is_none_or(|(lo, hi)| lo <= hi);
        let count = word(0x4a, 0x21).filter(|n| *n != 0);
        let unit = engineering_unit(cluster, attribute(0x100), word(0x75, 0x31));
        let labels = if cluster >= 0x0012 {
            count
                .and_then(|n| labels(bytes(0x0e, 0x48)?, usize::from(n)))
                .unwrap_or_default()
        } else if (0xf..=0x11).contains(&cluster) {
            [0x2e, 4]
                .into_iter()
                .map(|id| {
                    let (len, s) = bytes(id, 0x42)?.split_first()?;
                    if s.len() != usize::from(*len) {
                        return None;
                    }
                    label_text(s)
                })
                .collect::<Option<Vec<_>>>()
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let data_type = ty(cluster);
        let writable = output(cluster)
            && valid_range
            && access_control.is_some_and(|(t, access)| t == data_type && access & 3 == 3)
            && bytes(0x51, 0x10) == Some(&[0][..])
            && (cluster < 0x12 || count.is_some());
        let mut channel = Self {
            layout: Layout {
                cluster,
                writable,
                bounds: bounds.filter(|(min, max)| min < max),
                count,
                unit,
                resolution: float(0x6a).filter(|v| *v > 0.),
            },
            labels,
            minimum,
            maximum,
            reporting_type: access_control
                .filter(|(t, access)| *t == data_type && access & 4 != 0)
                .map(|(t, _)| t),
            in_service: bytes(0x51, 0x10) == Some(&[0][..]),
        };
        channel.layout.writable &= bytes(PRESENT, data_type)
            .and_then(|b| Value::from_bytes(DataType::try_from(data_type).ok()?, b).ok())
            .and_then(|v| channel.decode(v))
            .is_some();
        Some(channel)
    }
    /// OutOfService decouples the output from the physical actuator. Block
    /// later writes as soon as the server reports that condition.
    pub fn service_report(&mut self, value: Value<'_>) {
        self.in_service = value.data_type().id() == 0x10 && value.as_bytes() == [0];
    }

    pub fn label_property(&self) -> Option<&'static str> {
        (!self.labels.is_empty()).then(|| LABEL_KEYS[usize::from(self.layout.cluster - 0x000f)])
    }
    pub fn label(&self, state: &Json) -> Option<&str> {
        let i = if let Some(on) = state.as_bool() {
            usize::from(on)
        } else {
            usize::try_from(state.as_u64()?).ok()?.checked_sub(1)?
        };
        self.labels.get(i).map(String::as_str)
    }
    pub fn decode(&self, value: Value<'_>) -> Option<Json> {
        let bytes = value.as_bytes();
        let t = ty(self.layout.cluster);
        if value.data_type().id() != t {
            return None;
        }
        Some(match t {
            0x39 => json!(f32_value(bytes)?),
            0x10 => json!(match bytes {
                [0] => false,
                [1] => true,
                _ => return None,
            }),
            _ => {
                let n = u16::from_le_bytes(bytes.try_into().ok()?);
                if n == 0 || self.layout.count.is_some_and(|max| n > max) {
                    return None;
                }
                json!(n)
            }
        })
    }
    pub fn write(&self, input: &Json) -> Result<Vec<u8>> {
        let layout = self.layout;
        ensure!(layout.writable, "PresentValue write access unconfirmed");
        ensure!(self.in_service, "I/O object is out of service");
        let t = ty(layout.cluster);
        let mut body = vec![PRESENT as u8, 0, t];
        match t {
            0x39 => {
                let n = input
                    .as_f64()
                    .or_else(|| input.as_str()?.parse().ok())
                    .context("expected a number")?;
                ensure!(
                    (n as f32).is_finite()
                        && self.minimum.is_none_or(|min| n >= min)
                        && self.maximum.is_none_or(|max| n <= max),
                    "outside analog range"
                );
                body.extend((n as f32).to_le_bytes());
            }
            0x10 => body.push(match input {
                Json::Bool(b) => u8::from(*b),
                Json::String(s) if s == "ON" => 1,
                Json::String(s) if s == "OFF" => 0,
                _ => anyhow::bail!("expected a boolean or ON/OFF"),
            }),
            _ => {
                let n = input
                    .as_u64()
                    .and_then(|n| u16::try_from(n).ok())
                    .or_else(|| input.as_str()?.parse().ok())
                    .context("expected a state number")?;
                ensure!(
                    n >= 1 && layout.count.is_some_and(|max| n <= max),
                    "outside multistate range"
                );
                body.extend(n.to_le_bytes());
            }
        }
        Ok(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn channel(c: u16, access: Option<(u8, u8)>, attrs: &[(u16, u8, &[u8])]) -> Channel {
        Channel::resolve(c, access, |id| {
            attrs.iter().find(|a| a.0 == id).map(|a| (a.1, a.2))
        })
        .unwrap()
    }
    fn value<'a>(t: u8, b: &'a [u8]) -> Value<'a> {
        Value::from_bytes(t.try_into().unwrap(), b).unwrap()
    }

    #[test]
    fn all_objects_have_distinct_keys_and_only_proven_outputs_allow_writes() {
        for (offset, &key) in KEYS.iter().enumerate() {
            let c = 0x000c + offset as u16;
            let t = ty(c);
            let v: &[u8] = match t {
                0x10 => &[0],
                0x21 => &[1, 0],
                _ => &[0, 0, 0, 0],
            };
            let attrs = [
                (PRESENT, t, v),
                (0x51, 0x10, &[0][..]),
                (0x4a, 0x21, &[1, 0][..]),
            ];
            let proven = channel(c, Some((t, 7)), &attrs);
            assert_eq!(proven.layout.property(), key);
            assert_eq!(proven.layout.writable, output(c));
            assert_eq!(proven.reporting_type, Some(t));
            assert!(channel(c, Some((t, 3)), &attrs).reporting_type.is_none());
            for access in [None, Some((t, 5)), Some((0x20, 7))] {
                assert!(!channel(c, access, &attrs).layout.writable);
            }
            assert!(!channel(c, Some((t, 7)), &attrs[1..]).layout.writable);
        }
    }

    #[test]
    fn analog_units_range_and_real_f32_values_follow_the_endpoint() {
        let zero = 0f32.to_le_bytes();
        let limit = 5.5f32.to_le_bytes();
        let nans = f32::NAN.to_le_bytes();
        let attrs = [
            (PRESENT, 0x39, &zero[..]),
            (0x51, 0x10, &[0]),
            (0x41, 0x39, &limit),
            (0x75, 0x31, &[64, 0]),
            (0x100, 0x23, &[1, 0, 0, 1]),
        ];
        let analog = channel(0xd, Some((0x39, 3)), &attrs);
        assert_eq!(analog.layout.unit, Some("°C")); // AO temperature overrides Fahrenheit.
        assert_eq!(channel(0xd, None, &attrs[..4]).layout.unit, Some("°F"));
        assert_eq!(
            analog.write(&json!(5.5)).unwrap(),
            [0x55, 0, 0x39, 0, 0, 0xb0, 0x40]
        );
        assert!(analog.write(&json!(6)).is_err() && analog.write(&json!("NaN")).is_err());
        assert!(analog.decode(value(0x39, &nans)).is_none());
        let mut bad = attrs.to_vec();
        bad.push((0x45, 0x39, &nans));
        assert!(!channel(0xd, Some((0x39, 3)), &bad).layout.writable);
    }

    #[test]
    fn binary_and_multistate_labels_are_printable_and_states_are_one_based() {
        let attrs = [
            (PRESENT, 0x21, &[1, 0][..]),
            (0x51, 0x10, &[0]),
            (0x4a, 0x21, &[2, 0]),
            (
                0x0e,
                0x48,
                &[0x42, 2, 0, 3, b'F', b'a', b'n', 2, b'A', b'C'],
            ),
        ];
        let mut multi = channel(0x13, Some((0x21, 3)), &attrs);
        assert_eq!(multi.label(&json!(2)), Some("AC"));
        assert!(multi.write(&json!(0)).is_err() && multi.write(&json!(3)).is_err());
        assert!(multi.decode(value(0x21, &[3, 0])).is_none());
        multi.service_report(value(0x10, &[1]));
        assert!(multi.write(&json!(1)).is_err());
        multi.service_report(value(0x10, &[0]));
        assert_eq!(multi.write(&json!(2)).unwrap(), [0x55, 0, 0x21, 2, 0]);
        let binary = channel(
            0xf,
            None,
            &[(0x2e, 0x42, &[1, b'N']), (4, 0x42, &[1, b'Y'])],
        );
        assert_eq!(binary.label(&json!(false)), Some("N"));
        assert!(binary.decode(value(0x10, &[255])).is_none());
        assert!(
            channel(0xf, None, &[(0x2e, 0x42, &[1, b'N']), (4, 0x42, &[1, 1])])
                .label_property()
                .is_none()
        );
    }
}
