//! Wire-model applicability for the reviewed default-light family. Load the
//! trusted, generated index once and share it between coordinator sessions.
use crate::{Endpoint, StandardLights};
use anyhow::{Result, ensure};
use serde::Deserialize;
use std::{collections::BTreeMap, io::Read};

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Access {
    pub report: bool,
    pub read: bool,
    pub write: bool,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LightAccess {
    pub state: Access,
    pub brightness: Access,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Selector {
    family: String,
    path: String,
    symbol: String,
}

#[derive(Deserialize)]
struct Index {
    schema: u8,
    revisions: BTreeMap<String, String>,
    families: BTreeMap<String, LightAccess>,
    selectors: BTreeMap<String, Selector>,
}

pub struct LightIndex {
    index: Index,
    types: StandardLights,
}

impl LightIndex {
    /// The file is produced by light_index.py from the locked source trees.
    /// Configure it with the same trust and ownership as coordinator profiles.
    pub fn from_reader(input: impl Read) -> Result<Self> {
        let index: Index = serde_json::from_reader(input)?;
        ensure!(index.schema == 1, "unsupported light applicability schema");
        ensure!(
            ["zha", "deconz", "z2m"].iter().all(|source| {
                index.revisions.get(*source).is_some_and(|rev| {
                    rev.len() == 40 && rev.bytes().all(|c| c.is_ascii_hexdigit())
                })
            }),
            "light index has incomplete source provenance"
        );
        for (model, selector) in &index.selectors {
            ensure!(
                !model.is_empty()
                    && !selector.path.is_empty()
                    && !selector.symbol.is_empty()
                    && index.families.contains_key(&selector.family),
                "incomplete light selector"
            );
        }
        Ok(Self {
            index,
            types: StandardLights::builtin()?,
        })
    }

    /// Pass the count from a complete interview. A caller with missing
    /// descriptors cannot establish single-endpoint applicability.
    pub fn resolve(
        &self,
        wire_model: &str,
        ep: &Endpoint<'_>,
        device_type: u16,
        light_endpoint_count: usize,
    ) -> Option<&LightAccess> {
        if light_endpoint_count != 1
            || !self.types.matches(ep, device_type)
            || !ep.servers.contains(&0)
            || !ep.servers.contains(&8)
        {
            return None;
        }
        let selector = self.index.selectors.get(wire_model)?;
        self.index.families.get(&selector.family)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn require_exact_model_complete_light_and_independent_access() {
        let revision = "a".repeat(40);
        let index = json!({
            "schema":1,"revisions":{"z2m":revision,"zha":revision,"deconz":revision},
            "families":{"test":{"state":{"report":true,"read":false,"write":true},
                                "brightness":{"report":false,"read":true,"write":false}}},
            "selectors":{"lamp ":{"family":"test","path":"example.ts","symbol":"definitions[0]"}}
        });
        let index = LightIndex::from_reader(index.to_string().as_bytes()).unwrap();
        let ep = Endpoint {
            id: 1,
            profile: 260,
            servers: &[0, 6, 8],
            clients: &[],
        };
        let found = index.resolve("lamp ", &ep, 257, 1).unwrap();
        assert!(found.state.report && found.state.write && !found.state.read);
        assert!(found.brightness.read && !found.brightness.write && !found.brightness.report);
        assert!(index.resolve("lamp", &ep, 257, 1).is_none());
        assert!(index.resolve("lamp ", &ep, 0, 1).is_none());
        assert!(index.resolve("lamp ", &ep, 257, 2).is_none());
        assert!(
            index
                .resolve(
                    "lamp ",
                    &Endpoint {
                        servers: &[0, 6],
                        ..ep
                    },
                    257,
                    1
                )
                .is_none()
        );
    }
}
