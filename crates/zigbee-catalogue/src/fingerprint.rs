//! Offline comparison of the reviewed endpoint and Green Power commissioning selectors.
use super::{ROWS, Row};
use anyhow::{Context, Result, ensure};
use redb::{ReadOnlyDatabase, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{self, Write},
    path::Path,
};

pub(super) const INDEX: TableDefinition<u64, ()> =
    TableDefinition::new("fingerprint_candidates_v1");

#[derive(Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Endpoint {
    #[serde(rename = "ID", alias = "id")]
    pub id: u8,
    #[serde(rename = "profileID", alias = "profile")]
    pub profile: Option<u16>,
    #[serde(rename = "deviceID", alias = "device_type")]
    pub device: Option<u16>,
    #[serde(rename = "inputClusters", alias = "servers")]
    pub servers: Option<BTreeSet<u16>>,
    #[serde(rename = "outputClusters", alias = "clients")]
    pub clients: Option<BTreeSet<u16>>,
}

#[derive(Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Descriptor {
    #[serde(rename = "modelID", alias = "model")]
    pub model: Option<String>,
    #[serde(rename = "manufacturerName", alias = "manufacturer")]
    pub manufacturer: Option<String>,
    #[serde(rename = "manufacturerID")]
    pub manufacturer_id: Option<u16>,
    #[serde(rename = "type")]
    pub role: Option<String>,
    #[serde(rename = "powerSource")]
    pub power: Option<String>,
    pub endpoints: Vec<Endpoint>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Commissioning {
    #[serde(rename = "greenPowerDeviceID")]
    device: u8,
    #[serde(rename = "sourceID")]
    source: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Range {
    #[serde(rename = "greenPowerDeviceID")]
    device: u8,
    #[serde(rename = "sourceIDRange")]
    source: [u32; 2],
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Rule {
    Endpoint(Descriptor),
    GreenPower(Range),
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Interview {
    Endpoint(Descriptor),
    GreenPower(Commissioning),
}

fn satisfies<T: PartialEq>(required: &Option<T>, observed: &Option<T>) -> bool {
    required.is_none() || required == observed
}

impl Descriptor {
    pub(super) fn validate(&self) -> Result<()> {
        ensure!(!self.endpoints.is_empty(), "missing endpoint evidence");
        let mut ids = BTreeSet::new();
        for ep in &self.endpoints {
            ensure!(ids.insert(ep.id), "duplicate endpoint {}", ep.id);
        }
        Ok(())
    }

    fn matches(&self, device: &Descriptor, zha: bool) -> bool {
        if !(satisfies(&self.model, &device.model)
            && satisfies(&self.manufacturer, &device.manufacturer)
            && satisfies(&self.manufacturer_id, &device.manufacturer_id)
            && satisfies(&self.role, &device.role)
            && satisfies(&self.power, &device.power))
        {
            return false;
        }
        let observed: BTreeMap<_, _> = device
            .endpoints
            .iter()
            .filter(|ep| !zha || ep.id != 0)
            .map(|ep| (ep.id, ep))
            .collect();
        self.endpoints.len() == observed.len()
            && self.endpoints.iter().all(|rule| {
                observed.get(&rule.id).is_some_and(|ep| {
                    satisfies(&rule.profile, &ep.profile)
                        && satisfies(&rule.device, &ep.device)
                        && satisfies(&rule.servers, &ep.servers)
                        && satisfies(&rule.clients, &ep.clients)
                })
            })
    }
}

#[derive(Deserialize)]
pub(super) struct Rules {
    #[serde(default)]
    fingerprints: Vec<Rule>,
}

impl Rules {
    pub(super) fn matches_endpoint(&self, device: &Descriptor, zha: bool) -> bool {
        self.fingerprints
            .iter()
            .any(|rule| matches!(rule, Rule::Endpoint(ep) if ep.matches(device, zha)))
    }

    fn matches(&self, interview: &Interview, zha: bool) -> bool {
        self.fingerprints
            .iter()
            .any(|rule| match (rule, interview) {
                (Rule::Endpoint(rule), Interview::Endpoint(device)) => rule.matches(device, zha),
                (Rule::GreenPower(rule), Interview::GreenPower(device)) => {
                    rule.device == device.device
                        && (rule.source[0]..=rule.source[1]).contains(&device.source)
                }
                _ => false,
            })
    }
}

pub(super) fn indexable(row: &Row, raw: &str) -> Result<bool> {
    if !(row.source == "zha" || (row.source == "z2m" && row.models.is_empty())) {
        return Ok(false);
    }
    let rules: Rules = serde_json::from_str(raw)?;
    for rule in &rules.fingerprints {
        match rule {
            Rule::Endpoint(ep) => ep.validate()?,
            Rule::GreenPower(r) => ensure!(r.source[0] <= r.source[1], "reversed source ID range"),
        }
    }
    Ok(!rules.fingerprints.is_empty())
}

pub fn query(path: &Path, input: &Path) -> Result<()> {
    let interview: Interview = serde_json::from_reader(File::open(input)?)
        .context("expected endpoint interview or Green Power commissioning evidence")?;
    if let Interview::Endpoint(device) = &interview {
        device.validate()?;
    }
    let db = ReadOnlyDatabase::open(path)?;
    let tx = db.begin_read()?;
    let index = tx
        .open_table(INDEX)
        .context("rebuild catalogue to create the fingerprint index")?;
    let rows = tx.open_table(ROWS)?;
    let mut out = io::BufWriter::new(io::stdout().lock());
    for entry in index.iter()? {
        let (id, _) = entry?;
        let raw = rows
            .get(id.value())?
            .context("missing indexed fingerprint")?;
        let row: Row = serde_json::from_str(raw.value())?;
        let rules: Rules = serde_json::from_str(raw.value())?;
        if rules.matches(&interview, row.source == "zha") {
            serde_json::to_writer(
                &mut out,
                &serde_json::json!({
                    "record": id.value(), "source": row.source, "revision":row.revision,
                    "path":row.path, "symbol":row.symbol
                }),
            )?;
            writeln!(out)?;
        }
    }
    out.flush()?;
    Ok(())
}
