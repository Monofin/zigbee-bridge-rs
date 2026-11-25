//! Interpret an interviewed endpoint. The caller owns network membership,
//! sequences, scheduling and transmission; these functions perform no I/O.
pub mod basic_io;
pub mod cover;
pub mod equipment;
pub mod hvac;
pub mod input;
pub mod ias;
pub mod light_index;
pub mod lighting;
pub mod measurement;
pub mod metering;
pub mod ota;
pub mod outlet;
pub mod settings;
pub mod standard_controls;
pub mod tuya;
use anyhow::{Result, bail, ensure};
use serde::Deserialize;
use std::collections::BTreeMap;
use zigbee_zcl::{
    cluster::{Command, LevelControl, OnOff},
    value::Value,
};

pub struct Endpoint<'a> {
    pub id: u8,
    pub profile: u16,
    pub servers: &'a [u16],
    pub clients: &'a [u16],
}
/// Required interview evidence for a reviewed product-specific mapping.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fingerprint {
    pub manufacturer: String,
    pub model: String,
    pub endpoint: u8,
    pub profile: u16,
    pub servers: Vec<u16>,
}
impl Fingerprint {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.manufacturer.is_empty()
                && !self.model.is_empty()
                && (1..=240).contains(&self.endpoint)
                && !self.servers.is_empty(),
            "incomplete device fingerprint"
        );
        Ok(())
    }
    pub fn matches(&self, manufacturer: &str, model: &str, ep: &Endpoint<'_>) -> bool {
        self.manufacturer == manufacturer
            && self.model == model
            && self.endpoint == ep.id
            && self.profile == ep.profile
            && self.servers.iter().all(|id| ep.servers.contains(id))
    }
}

/// Device types considered by ZHA's default standard-light rule, from the
/// pinned reference in the data file. Quirk-specific overrides are separate.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StandardLights {
    pub source: String,
    profile_device_types: BTreeMap<u16, Vec<u16>>,
}
impl StandardLights {
    pub fn builtin() -> Result<Self> {
        Ok(serde_json::from_str(include_str!(
            "../profiles/lights.json"
        ))?)
    }
    pub fn matches(&self, ep: &Endpoint<'_>, device_type: u16) -> bool {
        // ZLL permits the common HA profile in the descriptor. These ZLL
        // lighting IDs are unambiguous there. 0x0000 and 0x0200 also name
        // an HA switch and shade, respectively, so keep those profile-scoped.
        let profile = if ep.profile == 0x0104 && matches!(device_type, 0x0110 | 0x0210 | 0x0220) {
            0xc05e
        } else {
            ep.profile
        };
        ep.has_standard_server(6)
            && self
                .profile_device_types
                .get(&profile)
                .is_some_and(|types| types.contains(&device_type))
    }
}

impl Endpoint<'_> {
    fn has_standard_server(&self, cluster: u16) -> bool {
        (1..=240).contains(&self.id)
            && matches!(self.profile, 0x0104 | 0xc05e)
            && self.servers.contains(&cluster)
    }
    /// Candidates inferred from the reported server list. An individual
    /// attribute observation still requires a correlated read or report.
    pub fn standard_features(&self) -> impl Iterator<Item = StandardFeature> + '_ {
        [StandardFeature::OnOff, StandardFeature::Brightness]
            .into_iter()
            .filter(|f| self.has_standard_server(f.cluster()))
    }
    fn has_ha_server(&self, cluster: u16) -> bool {
        (1..=240).contains(&self.id) && self.profile == 0x0104 && self.servers.contains(&cluster)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StandardFeature {
    OnOff,
    Brightness,
}
impl StandardFeature {
    pub fn cluster(self) -> u16 {
        match self {
            Self::OnOff => 6,
            Self::Brightness => 8,
        }
    }
    pub fn property(self) -> &'static str {
        match self {
            Self::OnOff => "state",
            Self::Brightness => "brightness",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    On(bool),
    Brightness(u8),
}
impl State {
    fn feature(self) -> StandardFeature {
        match self {
            Self::On(_) => StandardFeature::OnOff,
            Self::Brightness(_) => StandardFeature::Brightness,
        }
    }
}

/// Decode the mandatory current-state attributes. Unknown/sentinel values
/// remain unavailable; a command request never produces an observation.
pub fn observe(
    endpoint: &Endpoint<'_>,
    cluster: u16,
    attribute: u16,
    manufacturer_code: Option<u16>,
    value: Value<'_>,
) -> Option<State> {
    if manufacturer_code.is_some() || !endpoint.has_standard_server(cluster) || attribute != 0 {
        return None;
    }
    match (cluster, value.data_type().id(), value.as_bytes()) {
        (6, 0x10, [0]) => Some(State::On(false)),
        (6, 0x10, [1]) => Some(State::On(true)),
        (8, 0x20, [level @ 0..=254]) => Some(State::Brightness(*level)),
        _ => None,
    }
}

/// Build a standard On/Off or absolute Level Control command for the verified
/// endpoint. Transition time uses the ZCL unit of one tenth of a second.
pub fn command(endpoint: &Endpoint<'_>, requested: State, transition_ds: u16) -> Result<Command> {
    ensure!(
        endpoint.has_standard_server(requested.feature().cluster()),
        "required server missing on endpoint"
    );
    Ok(match requested {
        State::On(true) => Command::OnOff(OnOff::On {}),
        State::On(false) => Command::OnOff(OnOff::Off {}),
        State::Brightness(255) => bail!("reserved brightness"),
        State::Brightness(level) => Command::LevelControl(LevelControl::MoveToLevel {
            level,
            transition_time: transition_ds,
            option_mask: 0,
            option_override: 0,
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_remote_cannot_be_addressed_as_a_light() {
        let remote = Endpoint {
            id: 1,
            profile: 0x104,
            servers: &[],
            clients: &[6, 8],
        };
        assert!(command(&remote, State::On(true), 0).is_err());
        let light = Endpoint {
            servers: &[6, 8],
            ..remote
        };
        let types = StandardLights::builtin().unwrap();
        assert!(types.matches(&light, 0x0101));
        assert!(!types.matches(&remote, 0x0101));
        assert!(!types.matches(&light, 0x0051));
        assert!(types.matches(&light, 0x0210));
        assert!(!types.matches(&remote, 0x0220));
        assert!(!types.matches(&light, 0x0200));
        assert!(types.matches(&Endpoint { profile: 0xc05e, ..light }, 0x0200));
        let request = command(&light, State::Brightness(1), 3).unwrap();
        assert_eq!(request.encode().unwrap()[0], 1);
        let bytes = [1];
        let v = Value::from_bytes(0x10.try_into().unwrap(), &bytes).unwrap();
        assert_eq!(observe(&light, 6, 0, None, v), Some(State::On(true)));
        assert_eq!(observe(&remote, 6, 0, None, v), None);
        assert_eq!(observe(&light, 8, 0, None, v), None);
    }
}
