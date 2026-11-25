//! Rank only by evidence the catalogue can actually compare. Source predicates
//! and conversion code are never executed by the interview.
use crate::{Descriptor, MODELS, ROWS, Row, SOURCES, fingerprint};
use anyhow::{Context, Result};
use redb::{ReadOnlyDatabase, ReadableDatabase, ReadableMultimapTable, ReadableTable};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path};

#[derive(Serialize)]
pub struct Candidate {
    record: u64,
    source: String,
    revision: String,
    path: String,
    symbol: String,
    name: Option<String>,
    evidence: &'static str,
}

#[derive(Deserialize)]
struct Definition {
    #[serde(flatten)]
    row: Row,
    #[serde(default)]
    zigbee_models: Vec<String>,
    #[serde(default)]
    fingerprints: Vec<serde_json::Value>,
    #[serde(alias = "product")]
    display_model: Option<String>,
}

impl Definition {
    fn model_evidence(&self, model: &str, maker: Option<&str>) -> Option<&'static str> {
        if self.row.source == "z2m" {
            if self.zigbee_models.iter().any(|m| m == model) {
                return Some("wire_model");
            }
            // A shared model such as TS0601 can name hundreds of products.
            // Keep model and manufacturer from the same upstream alternative.
            return self
                .fingerprints
                .iter()
                .filter(|f| f["modelID"].as_str() == Some(model))
                .find(|f| match (maker, f["manufacturerName"].as_str()) {
                    (Some(a), Some(b)) => a == b,
                    _ => true,
                })
                .map(|_| "model_candidate");
        }
        match maker {
            Some(m) if !self.row.manufacturers.is_empty() => self
                .row
                .manufacturers
                .iter()
                .any(|declared| declared == m)
                .then_some("model_and_manufacturer_candidate"),
            _ => Some("model_candidate"),
        }
    }
}

/// Returns a source entry even when its count is zero. Pass no endpoints when
/// the active endpoint list is unknown. Descriptor rules require positive evidence.
pub fn identify(path: &Path, device: &Descriptor) -> Result<BTreeMap<String, Vec<Candidate>>> {
    if !device.endpoints.is_empty() {
        device.validate()?;
    }
    let db = ReadOnlyDatabase::open(path)?;
    let tx = db.begin_read()?;
    let rows = tx.open_table(ROWS)?;
    let mut ids = BTreeMap::new();
    if let Some(model) = &device.model {
        for id in tx.open_multimap_table(MODELS)?.get(model.as_str())? {
            ids.insert(id?.value(), false);
        }
    }
    for item in tx.open_table(fingerprint::INDEX)?.iter()? {
        ids.insert(item?.0.value(), true);
    }
    let mut results: BTreeMap<_, Vec<_>> = SOURCES.map(|s| (s.to_owned(), Vec::new())).into();
    for (id, indexed) in ids {
        let raw = rows.get(id)?.context("missing catalogue record")?;
        let definition: Definition = serde_json::from_str(raw.value())?;
        let evidence = if indexed {
            let rules: fingerprint::Rules = serde_json::from_str(raw.value())?;
            if rules.matches_endpoint(device, definition.row.source == "zha") {
                Some("reviewed_fingerprint")
            } else if device.endpoints.is_empty()
                && device
                    .model
                    .as_ref()
                    .is_some_and(|m| definition.row.models.contains(m))
            {
                definition.model_evidence(
                    device.model.as_deref().unwrap(),
                    device.manufacturer.as_deref(),
                )
            } else {
                None
            }
        } else {
            device
                .model
                .as_deref()
                .and_then(|m| definition.model_evidence(m, device.manufacturer.as_deref()))
        };
        if let Some(evidence) = evidence {
            let row = definition.row;
            results
                .get_mut(&row.source)
                .context("unknown catalogue source")?
                .push(Candidate {
                    record: id,
                    source: row.source,
                    revision: row.revision,
                    path: row.path,
                    symbol: row.symbol,
                    name: definition.display_model,
                    evidence,
                });
        }
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_manufacturer_on_another_fingerprint_cannot_qualify_a_shared_model() -> Result<()> {
        let row: Definition = serde_json::from_value(serde_json::json!({
            "source":"z2m", "revision":"test", "path":"device.ts", "symbol":"fixture",
            "kind":"definition", "models":["shared","other"], "manufacturers":["a","b"],
            "fingerprints":[{"modelID":"shared","manufacturerName":"a"},
                            {"modelID":"other","manufacturerName":"b"}]
        }))?;
        assert_eq!(row.model_evidence("shared", Some("b")), None);
        assert_eq!(
            row.model_evidence("shared", Some("a")),
            Some("model_candidate")
        );
        Ok(())
    }
}
