//! Diagnostic expectations from the old host. The running bridge never opens this file.
use super::source::address;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Number, Value};
use std::io::Write;

#[derive(Default, Serialize)]
pub struct Expectations {
    bindings: Vec<Entry<Binding>>,
    reporting: Vec<Entry<Reporting>>,
    unrecognized_bindings: usize,
    unrecognized_reporting: usize,
}

#[derive(Serialize)]
struct Entry<T> {
    source_role: String,
    source_ieee: String,
    source_endpoint: u8,
    #[serde(flatten)]
    value: T,
}
#[derive(Serialize)]
struct Binding {
    cluster: u16,
    target: Destination,
}
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Destination {
    Endpoint { ieee: String, endpoint: u8 },
    Group { group: u16 },
}
#[derive(Deserialize, Serialize)]
struct Reporting {
    cluster: u16,
    #[serde(rename(deserialize = "attrId"))]
    attribute: u16,
    // Herdsman's persisted ConfiguredReportingInternal contains reported attributes
    // only. Direction zero is the ZCL reporting record direction for this case.
    #[serde(default)]
    direction: u8,
    #[serde(rename(deserialize = "minRepIntval"))]
    min_interval: u16,
    #[serde(rename(deserialize = "maxRepIntval"))]
    max_interval: u16,
    #[serde(rename(deserialize = "repChange"))]
    change: Option<Number>,
    #[serde(rename(deserialize = "manufacturerCode"))]
    manufacturer_code: Option<u16>,
}
pub(super) fn reported_attribute(value: &Value) -> Option<(u16, u16)> {
    let report: Reporting = serde_json::from_value(value.clone()).ok()?;
    (report.direction == 0 && report.manufacturer_code.is_none())
        .then_some((report.cluster, report.attribute))
}
fn binding(v: &Value) -> Option<Binding> {
    let cluster = u16::try_from(v["cluster"].as_u64()?).ok()?;
    let target = match v["type"].as_str()? {
        "endpoint" => Destination::Endpoint {
            ieee: format!("0x{:016x}", address(v["deviceIeeeAddress"].as_str()?).ok()?),
            endpoint: u8::try_from(v["endpointID"].as_u64()?).ok()?,
        },
        "group" => Destination::Group {
            group: u16::try_from(v["groupID"].as_u64()?).ok()?,
        },
        _ => return None,
    };
    Some(Binding { cluster, target })
}
fn records<'a>(v: &'a Value, rejected: &mut usize) -> &'a [Value] {
    match v {
        Value::Null => &[],
        Value::Array(records) => records,
        Value::Object(m) => {
            *rejected += m.len().max(1);
            &[]
        }
        _ => {
            *rejected += 1;
            &[]
        }
    }
}
fn entry<T>(role: &str, ieee: u64, endpoint: u8, value: T) -> Entry<T> {
    Entry {
        source_role: role.to_string(),
        source_ieee: format!("0x{ieee:016x}"),
        source_endpoint: endpoint,
        value,
    }
}
impl Expectations {
    pub fn collect(&mut self, row: &Value, role: &str) -> Result<()> {
        let Some(eps) = row["endpoints"].as_object() else {
            return Ok(());
        };
        let ieee = address(
            row["ieeeAddr"]
                .as_str()
                .context("missing expectation source IEEE")?,
        )?;
        for (key, ep) in eps {
            let endpoint = u8::try_from(
                ep["epId"]
                    .as_u64()
                    .context("invalid expectation source endpoint")?,
            )?;
            anyhow::ensure!(
                key.parse::<u8>()? == endpoint,
                "expectation source endpoint mismatch"
            );
            for value in records(&ep["binds"], &mut self.unrecognized_bindings) {
                if let Some(b) = binding(value) {
                    self.bindings.push(entry(role, ieee, endpoint, b));
                } else {
                    self.unrecognized_bindings += 1;
                }
            }
            for value in records(
                &ep["configuredReportings"],
                &mut self.unrecognized_reporting,
            ) {
                match serde_json::from_value::<Reporting>(value.clone()) {
                    Ok(r) if r.direction == 0 => {
                        self.reporting.push(entry(role, ieee, endpoint, r))
                    }
                    _ => self.unrecognized_reporting += 1,
                }
            }
        }
        Ok(())
    }
    pub fn counts(&self) -> [(&'static str, usize); 4] {
        [
            ("binding_expectations_saved", self.bindings.len()),
            ("reporting_expectations_saved", self.reporting.len()),
            (
                "unrecognized_binding_expectations",
                self.unrecognized_bindings,
            ),
            (
                "unrecognized_reporting_expectations",
                self.unrecognized_reporting,
            ),
        ]
    }
    pub fn write(&self, dir: &std::path::Path) -> Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::create_dir(dir)?;
        let mut file = std::fs::File::options()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(dir.join("expectations.json"))?;
        serde_json::to_writer_pretty(&mut file, self)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        std::fs::File::open(dir)?.sync_all()?;
        Ok(())
    }
}
