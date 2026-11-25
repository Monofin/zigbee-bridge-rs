//! OTA support declared by the pinned converter catalogue. Image selection
//! and per-device transfer authorization belong to the OTA server.
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

#[derive(Deserialize)]
struct Profiles {
    models: BTreeSet<String>,
    manufacturers: BTreeMap<String, BTreeSet<String>>,
}

static PROFILES: LazyLock<Profiles> = LazyLock::new(|| {
    serde_json::from_str(include_str!("../profiles/ota.json")).expect("valid built-in OTA profiles")
});

pub fn declared(model: &str, manufacturer: Option<&str>) -> bool {
    PROFILES.models.contains(model)
        || manufacturer
            .and_then(|maker| PROFILES.manufacturers.get(maker))
            .is_some_and(|models| models.contains(model))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manufacturer_selects_between_devices_sharing_a_model() {
        assert!(declared("TS0601", Some("_TZE284_oitavov2")));
        assert!(!declared("TS0601", Some("_TZE284_sgabhwa6")));
        assert!(!declared("TS0601", None));
    }
}
