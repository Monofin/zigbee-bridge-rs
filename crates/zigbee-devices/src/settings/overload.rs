//! SONOFF S60 thresholds are milli-units, packed in current, voltage, power
//! order. The converter's byte-array CHAR_STR already includes its length.
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::{Map, Value, json};

#[derive(Clone, Copy, Deserialize)]
pub(super) struct Limits {
    power: u32,
    current: u32,
}

const GROUPS: [&str; 3] = ["current", "voltage", "power"];

impl Limits {
    pub(super) fn write(self, value: &Value) -> Result<Vec<u8>> {
        let input = value
            .as_object()
            .context("overload protection requires an object")?;
        let mut flags = [0_u8; 3];
        let mut values = Vec::new();
        let mut used = 0;
        for (group, name) in GROUPS.iter().enumerate() {
            let (min, max, step) = match group {
                0 => (100, self.current * 1000, 100),
                1 => (165_000, 277_000, 1000),
                _ => (100, self.power * 1000, 100),
            };
            let mut upper = None;
            for (index, prefix) in ["max", "min"].iter().enumerate() {
                let key = format!("{prefix}_{name}");
                let mandatory = index == 0 && group != 1;
                let enabled = if mandatory {
                    true
                } else {
                    match input.get(&format!("enable_{key}")) {
                        Some(v) => {
                            used += 1;
                            match v.as_str() {
                                Some("ENABLE") => true,
                                Some("DISABLE") => false,
                                _ => anyhow::bail!("expected ENABLE or DISABLE for {key}"),
                            }
                        }
                        None => false,
                    }
                };
                if !enabled {
                    if let Some(n) = input.get(&key) {
                        // Allow roundtrips of a disabled threshold from a report.
                        ensure!(
                            n.as_f64() == Some(0.) || threshold(n, min, max, step).is_some(),
                            "invalid disabled {key}"
                        );
                        ensure!(
                            input.contains_key(&format!("enable_{key}")),
                            "specify enable_{key} for {key}"
                        );
                        used += 1;
                    }
                    continue;
                }
                let n = input
                    .get(&key)
                    .and_then(|v| threshold(v, min, max, step))
                    .with_context(|| format!("missing or invalid {key}"))?;
                used += 1;
                if index == 0 {
                    upper = Some(n);
                } else {
                    ensure!(
                        upper.is_none_or(|u| n < u),
                        "minimum {name} must be less than maximum"
                    );
                }
                flags[group] |= 1 << index;
                values.extend(n.to_le_bytes());
            }
        }
        ensure!(used == input.len(), "unknown overload protection setting");
        let length = u8::try_from(5 + values.len())?;
        let mut bytes = vec![length, 4, length - 2];
        bytes.extend(flags);
        bytes.extend(values);
        Ok(bytes)
    }
}

fn threshold(value: &Value, min: u32, max: u32, step: u32) -> Option<u32> {
    let n = value.as_f64()? * 1000.;
    let raw = n.round();
    ((raw - n).abs() < 1e-6
        && (f64::from(min)..=f64::from(max)).contains(&raw)
        && (raw as u32).is_multiple_of(step))
    .then_some(raw as u32)
}

/// Reports may specify one category or all three. Keep absent categories
/// absent; only an explicit zero mask disables the thresholds of a category.
pub(super) fn report(bytes: &[u8]) -> Option<Map<String, Value>> {
    let (&length, data) = bytes.split_first()?;
    if usize::from(length) != data.len() {
        return None;
    }
    // Some replies carry a status prefix before the block. In either form the
    // subordinate length must cover the masks and every threshold exactly.
    if let Some(map) = block(data) {
        return Some(map);
    }
    block(data.get(1..)?)
}

fn block(data: &[u8]) -> Option<Map<String, Value>> {
    let (&mode, rest) = data.split_first()?;
    let (&length, rest) = rest.split_first()?;
    if usize::from(length) != rest.len() {
        return None;
    }
    let categories = match mode {
        1 => 0..1,
        2 => 1..2,
        3 => 2..3,
        4 => 0..3,
        _ => return None,
    };
    let (flags, mut tail) = rest.split_at_checked(categories.len())?;
    let mut out = Map::new();
    for (group, &mask) in categories.zip(flags) {
        if mask > 3 {
            return None;
        }
        for (index, prefix) in ["max", "min"].iter().enumerate() {
            let key = format!("{prefix}_{}", GROUPS[group]);
            let enabled = mask & (1 << index) != 0;
            let n = if enabled {
                let (num, remain) = tail.split_at_checked(4)?;
                tail = remain;
                u32::from_le_bytes(num.try_into().ok()?)
            } else {
                0
            };
            if index == 1 || group == 1 {
                out.insert(
                    format!("enable_{key}"),
                    json!(if enabled { "ENABLE" } else { "DISABLE" }),
                );
            }
            out.insert(key, json!(f64::from(n) / 1000.));
        }
    }
    tail.is_empty().then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    const LIMITS: Limits = Limits {
        power: 3250,
        current: 14,
    };
    // Bytes returned by buildOverloadProtectionPayload at the pinned converter revision.
    const MINIMAL: &[u8] = &[13, 4, 11, 1, 0, 1, 0xb0, 0x36, 0, 0, 0x50, 0x97, 0x31, 0];
    const FULL: &[u8] = &[
        29, 4, 27, 3, 3, 3, 0xb0, 0x36, 0, 0, 100, 0, 0, 0, 8, 0x3a, 4, 0, 0x88, 0x84, 2, 0, 0x50,
        0x97, 0x31, 0, 100, 0, 0, 0,
    ];
    #[test]
    fn both_pinned_wire_shapes_round_trip_with_exact_limits_and_flags() {
        for raw in [MINIMAL, FULL] {
            let config = Value::Object(report(raw).unwrap());
            assert_eq!(config["max_current"], 14.);
            assert_eq!(config["max_power"], 3250.);
            assert_eq!(LIMITS.write(&config).unwrap(), raw);
            let mut wrapped = vec![raw[0] + 1, 0];
            wrapped.extend_from_slice(&raw[1..]);
            assert_eq!(report(raw), report(&wrapped));
            for i in 0..raw.len() {
                assert!(report(&raw[..i]).is_none());
            }
        }
        let mut bad = MINIMAL.to_vec();
        bad[2] -= 1;
        assert!(report(&bad).is_none());
        bad = MINIMAL.to_vec();
        bad[3] = 4;
        assert!(report(&bad).is_none());
        let mut bad = MINIMAL.to_vec();
        bad.extend([1]);
        bad[0] += 1;
        bad[2] += 1;
        assert!(report(&bad).is_none());
        let n = report(&[7, 2, 5, 2, 0x88, 0x84, 2, 0]).unwrap();
        assert_eq!(n["min_voltage"], 165.);
        assert_eq!(n["enable_max_voltage"], "DISABLE");
        assert!(!n.contains_key("max_current"));
        let mut config = Value::Object(report(FULL).unwrap());
        for (key, value) in [
            ("max_current", json!(14.1)),
            ("max_power", json!(3250.01)),
            ("min_current", json!(14)),
            ("min_power", json!(3250)),
            ("min_voltage", json!(277)),
            ("min_voltage", json!(164)),
            ("max_voltage", json!(275.5)),
            ("enable_min_power", json!(true)),
            ("max_power", json!(0)),
            ("max_current", json!(-1)),
            ("unknown", json!(1)),
        ] {
            let mut invalid = config.clone();
            invalid[key] = value;
            assert!(LIMITS.write(&invalid).is_err(), "{key}");
        }
        config.as_object_mut().unwrap().remove("max_current");
        assert!(LIMITS.write(&config).is_err());
        assert!(
            LIMITS
                .write(&json!({"max_power":3250,"max_current":14,"min_voltage":220}))
                .is_err()
        );
    }
}
