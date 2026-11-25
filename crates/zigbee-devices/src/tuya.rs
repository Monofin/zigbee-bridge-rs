//! Tuya MCU datapoint records, after the command's transaction prefix.
//! Preserve the raw record even when its type or meaning is unknown.
mod button;
pub mod household;
pub mod lamp;
pub mod services;
pub use services::Service;
mod time;
pub use time::{TimeEpoch, TimeError};
pub use button::{Button, ButtonAction};

use crate::{Endpoint, Fingerprint};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Debug, PartialEq, Eq)]
pub struct Datapoint<'a> {
    pub id: u8,
    pub kind: u8,
    pub bytes: &'a [u8],
}
pub fn decode(mut payload: &[u8]) -> Result<Vec<Datapoint<'_>>> {
    let mut records = Vec::new();
    while !payload.is_empty() {
        let (&[id, kind, hi, lo], tail) = payload
            .split_first_chunk::<4>()
            .context("truncated Tuya header")?;
        let (bytes, rest) = tail
            .split_at_checked(u16::from_be_bytes([hi, lo]).into())
            .context("truncated Tuya value")?;
        records.push(Datapoint { id, kind, bytes });
        payload = rest;
    }
    Ok(records)
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum Feature {
    Illuminance,
    IntervalTime,
}
impl Feature {
    pub fn unit(self) -> &'static str {
        match self {
            Self::Illuminance => "lx",
            Self::IntervalTime => "s",
        }
    }
}
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Codec {
    ValueU32Be,
}
impl Codec {
    fn read(&self, dp: &Datapoint<'_>) -> Option<u32> {
        match self {
            Self::ValueU32Be if dp.kind == 2 => Some(u32::from_be_bytes(dp.bytes.try_into().ok()?)),
            _ => None,
        }
    }
    fn encode(&self, id: u8, value: u32) -> Vec<u8> {
        match self {
            Self::ValueU32Be => {
                let mut bytes = vec![id, 2, 0, 4];
                bytes.extend(value.to_be_bytes());
                bytes
            }
        }
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Mapping {
    dp: u8,
    feature: Feature,
    codec: Codec,
    write_range: Option<[u32; 2]>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    fingerprint: Fingerprint,
    mappings: Vec<Mapping>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profiles {
    version: u8,
    source: String,
    profiles: Vec<Profile>,
}
impl Profiles {
    pub fn builtin() -> Result<Self> {
        Self::load(include_str!("../profiles/tuya.json"))
    }
    pub fn source(&self) -> &str {
        &self.source
    }
    pub fn load(json: &str) -> Result<Self> {
        let p: Self = serde_json::from_str(json)?;
        ensure!(
            p.version == 1 && !p.source.is_empty(),
            "unknown schema or missing provenance"
        );
        let mut fingerprints = BTreeSet::new();
        for def in &p.profiles {
            let f = &def.fingerprint;
            f.validate()?;
            ensure!(f.servers.contains(&0xef00), "missing Tuya server");
            ensure!(
                fingerprints.insert((&f.manufacturer, &f.model, f.profile, f.endpoint)),
                "ambiguous fingerprint"
            );
            let mut ids = BTreeSet::new();
            let mut outputs = Vec::new();
            for m in &def.mappings {
                ensure!(ids.insert(m.dp), "duplicate datapoint");
                if let Some([lo, hi]) = m.write_range {
                    ensure!(
                        m.feature == Feature::IntervalTime
                            && lo <= hi
                            && !outputs.contains(&m.feature),
                        "unsupported write mapping or invalid bounds"
                    );
                    outputs.push(m.feature);
                }
            }
        }
        Ok(p)
    }
    pub fn resolve(
        &self,
        manufacturer: &str,
        model: &str,
        endpoint: &Endpoint<'_>,
    ) -> Option<&Profile> {
        self.profiles
            .iter()
            .find(|p| p.fingerprint.matches(manufacturer, model, endpoint))
    }
}
impl Profile {
    pub fn features(&self) -> impl Iterator<Item = (u8, Feature, Option<[u32; 2]>)> + '_ {
        self.mappings
            .iter()
            .map(|m| (m.dp, m.feature, m.write_range))
    }
    pub fn observation(&self, dp: &Datapoint<'_>) -> Option<(Feature, u32)> {
        let m = self.mappings.iter().find(|m| m.dp == dp.id)?;
        Some((m.feature, m.codec.read(dp)?))
    }
    /// Return a record for a proposed write. The caller supplies the command
    /// envelope only after resolving the target's current membership.
    pub fn write(&self, feature: Feature, value: u32) -> Result<Vec<u8>> {
        let m = self
            .mappings
            .iter()
            .find(|m| m.feature == feature && m.write_range.is_some())
            .context("feature has no write mapping")?;
        let [lo, hi] = m.write_range.context("no range")?;
        ensure!((lo..=hi).contains(&value), "outside device range");
        Ok(m.codec.encode(m.dp, value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn identical_bytes_have_model_specific_meaning_and_authority() -> Result<()> {
        let eps = Endpoint {
            id: 1,
            profile: 0x104,
            servers: &[0xef00],
            clients: &[],
        };
        let p = Profiles::builtin()?;
        let raw = [104, 2, 0, 4, 0, 0, 0, 30, 99, 0xfa, 0, 1, 0xaa];
        let dp = decode(&raw)?;
        assert_eq!(dp[1].bytes, &[0xaa]);
        assert!(decode(&raw[..raw.len() - 1]).is_err());
        let light = p.resolve("_TZE204_qasjif9e", "TS0601", &eps).unwrap();
        let timer = p.resolve("_TZE204_laokfqwu", "TS0601", &eps).unwrap();
        assert_eq!(light.observation(&dp[0]), Some((Feature::Illuminance, 30)));
        assert_eq!(timer.observation(&dp[0]), Some((Feature::IntervalTime, 30)));
        assert_eq!(timer.write(Feature::IntervalTime, 30)?, raw[..8]);
        assert!(light.write(Feature::Illuminance, 30).is_err());
        assert!(timer.write(Feature::IntervalTime, 3601).is_err());
        assert!(p.resolve("", "TS0601", &eps).is_none());
        assert!(
            p.resolve(
                "_TZE204_laokfqwu",
                "TS0601",
                &Endpoint {
                    servers: &[],
                    ..eps
                }
            )
            .is_none()
        );
        assert!(timer.observation(&Datapoint { kind: 1, ..dp[0] }).is_none());
        Ok(())
    }
}
