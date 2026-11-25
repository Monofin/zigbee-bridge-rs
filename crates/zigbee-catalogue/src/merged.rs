//! A wire model is a lookup key. Each candidate retains its original match
//! conditions, including conditions which cannot be evaluated by this tool.
use super::{MODELS, ROWS};
use anyhow::{Context, Result, ensure};
use redb::{
    ReadOnlyDatabase, ReadTransaction, ReadableDatabase, ReadableMultimapTable, ReadableTable,
};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use std::{
    collections::BTreeMap,
    io::{self, Write},
    path::Path,
};

type References = BTreeMap<String, Vec<u64>>;

#[derive(Serialize, Default)]
struct Model {
    candidates: References,
}

#[derive(Serialize)]
struct Merged {
    version: u8,
    models: BTreeMap<String, Model>,
    unkeyed: References,
    // Store each original record once even when several model strings refer to it.
    records: BTreeMap<u64, Box<RawValue>>,
}

#[derive(Deserialize)]
struct Keys {
    source: String,
    models: Vec<String>,
}

impl Merged {
    fn read(tx: &ReadTransaction, only: Option<&str>) -> Result<Self> {
        let mut result = Self {
            version: 1,
            models: BTreeMap::new(),
            unkeyed: BTreeMap::new(),
            records: BTreeMap::new(),
        };
        let rows = tx.open_table(ROWS)?;
        if let Some(model) = only {
            for id in tx.open_multimap_table(MODELS)?.get(model)? {
                let id = id?.value();
                let raw = rows
                    .get(id)?
                    .context("model index refers to missing record")?;
                result.add(id, raw.value(), Some(model))?;
            }
        } else {
            for row in rows.iter()? {
                let (id, raw) = row?;
                result.add(id.value(), raw.value(), None)?;
            }
        }
        Ok(result)
    }

    fn add(&mut self, id: u64, raw: &str, only: Option<&str>) -> Result<()> {
        let Keys { source, mut models } = serde_json::from_str(raw)?;
        if let Some(key) = only {
            ensure!(
                models.iter().any(|m| m == key),
                "model index disagrees with source record"
            );
            models.retain(|m| m == key);
        }
        models.sort_unstable();
        models.dedup();
        if models.is_empty() {
            self.unkeyed.entry(source).or_default().push(id);
        } else {
            for key in models {
                self.models
                    .entry(key)
                    .or_default()
                    .candidates
                    .entry(source.clone())
                    .or_default()
                    .push(id);
            }
        }
        self.records
            .insert(id, RawValue::from_string(raw.to_owned())?);
        Ok(())
    }
}

pub fn export(path: &Path, only: Option<&str>) -> Result<()> {
    let db = ReadOnlyDatabase::open(path)?;
    let merged = Merged::read(&db.begin_read()?, only)?;
    let mut out = io::BufWriter::new(io::stdout().lock());
    serde_json::to_writer(&mut out, &merged)?;
    writeln!(out)?;
    out.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_model_keys_share_records_without_combining_fingerprints() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let input = super::super::SOURCES.map(|s| dir.path().join(s));
        for (i, path) in input.iter().enumerate() {
            let s = super::super::SOURCES[i];
            let models = match s {
                "zha" => vec!["TS0601", "ts0601", "TS0601 ", "TS0601"],
                "deconz" => vec![],
                _ => vec!["TS0601", ""],
            };
            let row = serde_json::json!({"source":s,"revision":"a".repeat(40),"path":"fixtures/definition","symbol":"variant","kind":"test","manufacturers":["first","second"],"models":models,"fingerprints":[{"manufacturerName":"first","endpoints":[{"ID":1}]},{"manufacturerName":"second","endpoints":[{"ID":2}]}]});
            std::fs::write(path, row.to_string())?;
        }
        let path = dir.path().join("ref.redb");
        super::super::build(&path, input.each_ref().map(|p| p.as_path()))?;
        let db = ReadOnlyDatabase::open(path)?;
        let tx = db.begin_read()?;
        let all = Merged::read(&tx, None)?;
        assert_eq!(all.records.len(), 3);
        assert_eq!(all.models.len(), 4);
        assert_eq!(all.models["TS0601"].candidates["zha"], [0]);
        assert_eq!(all.models["TS0601"].candidates["z2m"], [2]);
        assert_eq!(all.models["ts0601"].candidates["zha"], [0]);
        assert_eq!(all.models["TS0601 "].candidates["zha"], [0]);
        assert_eq!(all.models[""].candidates["z2m"], [2]);
        assert_eq!(all.unkeyed["deconz"], [1]);
        let one = Merged::read(&tx, Some("TS0601"))?;
        assert_eq!(one.models.len(), 1);
        assert_eq!(
            one.models["TS0601"].candidates,
            all.models["TS0601"].candidates
        );
        assert_eq!(one.records.len(), 2);
        let original = std::fs::read_to_string(&input[2])?;
        assert_eq!(one.records[&2].get(), original);
        assert!(Merged::read(&tx, Some("no such model"))?.records.is_empty());
        Ok(())
    }
}
