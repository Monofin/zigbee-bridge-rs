//! Build a replaceable reference catalogue. Its rows are upstream claims. They
//! never become members of a coordinator or grant a device permission to write.
use anyhow::{Context, Result, ensure};
use redb::{
    Database, MultimapTableDefinition, ReadOnlyDatabase, ReadableDatabase, ReadableMultimapTable,
    ReadableTable, TableDefinition,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{self, BufRead, BufReader, Write},
    path::{Component, Path},
};
use tempfile::NamedTempFile;

mod fingerprint;
mod identify;
mod merged;
pub use fingerprint::{Descriptor, Endpoint, query as fingerprints};
pub use identify::identify;
pub use merged::export as merged;
const ROWS: TableDefinition<u64, &str> = TableDefinition::new("records_v1");
const MODELS: MultimapTableDefinition<&str, u64> = MultimapTableDefinition::new("wire_model_v1");
const MANIFEST: TableDefinition<&str, &str> = TableDefinition::new("sources_v1");
const SOURCES: [&str; 3] = ["zha", "deconz", "z2m"];

#[derive(Deserialize)]
struct Row {
    source: String,
    revision: String,
    path: String,
    symbol: String,
    kind: String,
    manufacturers: Vec<String>,
    models: Vec<String>,
}
impl Row {
    fn validate(&self, expected: &str) -> Result<()> {
        ensure!(
            self.source == expected,
            "expected {expected}, found {}",
            self.source
        );
        ensure!(
            self.revision.len() == 40 && self.revision.bytes().all(|c| c.is_ascii_hexdigit()),
            "invalid source revision"
        );
        ensure!(
            !self.path.is_empty()
                && Path::new(&self.path)
                    .components()
                    .all(|c| matches!(c, Component::Normal(_))),
            "invalid source path"
        );
        ensure!(
            !self.symbol.is_empty() && !self.kind.is_empty(),
            "missing symbol or kind"
        );
        Ok(())
    }
}
#[derive(Serialize, Deserialize)]
struct SourceSummary {
    source: String,
    revision: String,
    records: u64,
}

pub fn build(path: &Path, inputs: [&Path; 3]) -> Result<()> {
    let absolute = std::path::absolute(path)?;
    let dir = absolute.parent().context("missing destination directory")?;
    let stage = NamedTempFile::new_in(dir)?;
    let db = Database::create(stage.path())?;
    let tx = db.begin_write()?;
    let mut summaries = Vec::new();
    {
        let mut rows = tx.open_table(ROWS)?;
        let mut models = tx.open_multimap_table(MODELS)?;
        let mut fingerprints = tx.open_table(fingerprint::INDEX)?;
        let mut manifest = tx.open_table(MANIFEST)?;
        let mut next = 0u64;
        for (source, input) in SOURCES.into_iter().zip(inputs) {
            let mut revision = None;
            let start = next;
            for (number, line) in BufReader::new(File::open(input)?).lines().enumerate() {
                let line = line?;
                let r: Row = serde_json::from_str(&line)
                    .with_context(|| format!("{} line {}", input.display(), number + 1))?;
                r.validate(source)
                    .with_context(|| format!("{} line {}", input.display(), number + 1))?;
                ensure!(
                    revision.get_or_insert(r.revision.clone()) == &r.revision,
                    "mixed {source} revisions"
                );
                for model in &r.models {
                    models.insert(model.as_str(), next)?;
                }
                if fingerprint::indexable(&r, &line)
                    .with_context(|| format!("{} line {}", input.display(), number + 1))?
                {
                    fingerprints.insert(next, ())?;
                }
                // Keep all source-specific qualifiers alongside the search facets.
                rows.insert(next, line.as_str())?;
                next += 1;
            }
            let summary = SourceSummary {
                source: source.into(),
                revision: revision.context("empty catalogue input")?,
                records: next - start,
            };
            manifest.insert(source, serde_json::to_string(&summary)?.as_str())?;
            summaries.push(summary);
        }
    }
    tx.commit()?;
    drop(db);
    stage.as_file().sync_all()?;
    stage.persist_noclobber(path)?;
    File::open(dir)?.sync_all()?;
    println!("{}", serde_json::to_string_pretty(&summaries)?);
    Ok(())
}

pub fn query(file: &Path, model: Option<&str>, full: bool) -> Result<()> {
    let db = ReadOnlyDatabase::open(file)?;
    let tx = db.begin_read()?;
    let mut out = io::BufWriter::new(io::stdout().lock());
    if let Some(model) = model {
        let index = tx.open_multimap_table(MODELS)?;
        let rows = tx.open_table(ROWS)?;
        for id in index.get(model)? {
            let row = rows.get(id?.value())?.context("missing indexed record")?;
            if full {
                writeln!(out, "{}", row.value())?;
            } else {
                let r: Row = serde_json::from_str(row.value())?;
                serde_json::to_writer(
                    &mut out,
                    &serde_json::json!({"source":r.source,"revision":r.revision,"path":r.path,"symbol":r.symbol,"kind":r.kind,"manufacturers":r.manufacturers}),
                )?;
                writeln!(out)?;
            }
        }
    } else {
        let manifest = tx.open_table(MANIFEST)?;
        for source in SOURCES {
            writeln!(
                out,
                "{}",
                manifest.get(source)?.context("missing source")?.value()
            )?;
        }
    }
    out.flush()?;
    Ok(())
}

/// Count each named converter once per Z2M definition. Anonymous callbacks
/// have no stable symbol and remain available on their individual records.
pub fn converters(db: &Path) -> Result<()> {
    #[derive(Deserialize)]
    struct Converter {
        symbol: Option<String>,
    }
    #[derive(Deserialize)]
    struct Definition {
        source: String,
        #[serde(default)]
        from_zigbee: Vec<Converter>,
        #[serde(default)]
        to_zigbee: Vec<Converter>,
    }
    let db = ReadOnlyDatabase::open(db)?;
    let tx = db.begin_read()?;
    let mut counts = BTreeMap::new();
    for row in tx.open_table(ROWS)?.iter()? {
        let (_, value) = row?;
        let d: Definition = serde_json::from_str(value.value())?;
        if d.source != "z2m" {
            continue;
        }
        for (direction, converters) in [("from_zigbee", d.from_zigbee), ("to_zigbee", d.to_zigbee)]
        {
            let symbols: BTreeSet<_> = converters.into_iter().filter_map(|c| c.symbol).collect();
            for symbol in symbols {
                *counts.entry((direction, symbol)).or_insert(0usize) += 1;
            }
        }
    }
    let mut counts: Vec<_> = counts.into_iter().collect();
    counts.sort_by(|(left, n), (right, m)| m.cmp(n).then_with(|| left.cmp(right)));
    let mut out = io::BufWriter::new(io::stdout().lock());
    for ((direction, symbol), definitions) in counts {
        serde_json::to_writer(
            &mut out,
            &serde_json::json!({"direction":direction,"symbol":symbol,"definitions":definitions}),
        )?;
        writeln!(out)?;
    }
    out.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn build_rejects_mixed_revisions_without_replacing_a_database() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let inputs = SOURCES.map(|s| dir.path().join(s));
        let paths = inputs.each_ref().map(|p| p.as_path());
        let row = |source: &str, rev: &str| {
            serde_json::json!({
            "source":source,"path":"devices/a","symbol":"A","kind":"definition","revision":rev,
            "models":["TS0601"],"manufacturers":[],"fingerprints":[{"modelID":"TS0601","manufacturerName":"review_me","endpoints":[{"ID":1}]}]
        }).to_string()
        };
        for (s, p) in SOURCES.into_iter().zip(paths) {
            std::fs::write(p, row(s, &"a".repeat(40)))?;
        }
        let target = dir.path().join("index");
        build(&target, paths)?;
        assert!(build(&target, paths).is_err());
        writeln!(
            File::options().append(true).open(paths[2])?,
            "\n{}",
            row("z2m", &"b".repeat(40))
        )?;
        let failed = dir.path().join("incomplete");
        assert!(build(&failed, paths).is_err());
        assert!(!failed.exists());
        let tx = ReadOnlyDatabase::open(target)?.begin_read()?;
        let table = tx.open_table(ROWS)?;
        let index = tx.open_multimap_table(MODELS)?;
        let ids = index
            .get("TS0601")?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        assert_eq!(ids.len(), 3);
        for id in ids {
            let raw = table.get(id.value())?.unwrap();
            let v: serde_json::Value = serde_json::from_str(raw.value())?;
            assert_eq!(v["fingerprints"][0]["manufacturerName"], "review_me");
        }
        Ok(())
    }
}
