use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use serde_json::{Map, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{BufRead, BufReader, Read},
    path::{Component, Path},
};
use zigbee_bridge_rs::{
    interview_log::{
        AttributeResult, Descriptor, Entry, IdentitySource, InterviewReport, Observation,
    },
    registry::Device,
};

pub struct Member {
    pub device: Device,
    pub report: InterviewReport,
}
pub struct Plan {
    pub availability:zigbee_bridge_rs::availability::Catalogue,
    pub last_seen: zigbee_bridge_rs::bridge::LastSeen,
    pub members: Vec<Member>,
    pub expectations: super::expectations::Expectations,
    pub counts: BTreeMap<&'static str, usize>,
}
type Counts = BTreeMap<&'static str, usize>;

fn count(counts: &mut Counts, id: &'static str, n: usize) {
    *counts.entry(id).or_default() += n;
}
/// Import only the supported typed leaf. Unknown siblings remain visible in
/// the migration report; arbitrary user JSON never becomes HA configuration.
fn temperature_option(ha: Option<&Value>) -> (Option<bool>, bool) {
    let Some(ha) = ha else { return (None, false) };
    let Some(section) = ha.as_object() else { return (None, !ha.is_null() && ha != false) };
    let option = ha.pointer("/temperature/force_update").and_then(Value::as_bool);
    let unknown = section.iter().any(|(key, value)| {
        key != "temperature" || value.as_object().is_none_or(|fields| {
            fields.iter().any(|(k, v)| k != "force_update" || !v.is_boolean())
        })
    });
    (option, unknown)
}

fn open(path: &Path) -> Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    let f = File::options()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_NOCTTY)
        .open(path)?;
    ensure!(f.metadata()?.is_file(), "expected a regular snapshot file");
    Ok(f)
}
fn yaml(path: &Path) -> Result<Value> {
    let file = open(path)?;
    let mut text = String::new();
    file.take(16 * 1024 * 1024 + 1).read_to_string(&mut text)?;
    ensure!(
        text.len() <= 16 * 1024 * 1024,
        "oversized configuration input"
    );
    // Parser diagnostics can contain the source line, including credentials.
    serde_saphyr::from_str(&text).map_err(|_| anyhow::anyhow!("invalid Zigbee2MQTT YAML input"))
}
fn merge(target: &mut Value, source: Value) {
    if let (Some(to), Some(from)) = (target.as_object_mut(), source.as_object()) {
        for (k, v) in from {
            merge(to.entry(k).or_insert(Value::Null), v.clone());
        }
    } else {
        *target = source;
    }
}

fn config_table(root: &Path, value: Option<&Value>) -> Result<Map<String, Value>> {
    let Some(value) = value else {
        return Ok(Map::new());
    };
    if let Some(m) = value.as_object() {
        return Ok(m.clone());
    }
    let files: Vec<_> = match value {
        Value::String(s) => vec![s.as_str()],
        Value::Array(items) => items
            .iter()
            .map(|s| s.as_str().context("invalid split configuration filename"))
            .collect::<Result<_>>()?,
        Value::Null => return Ok(Map::new()),
        _ => bail!("expected device/group configuration table or filename"),
    };
    let mut table = Value::Object(Map::new());
    for file in files {
        let p = Path::new(file);
        ensure!(
            p.components()
                .all(|c| matches!(c, Component::Normal(_) | Component::CurDir)),
            "split configuration must be within the supplied snapshot"
        );
        let path = root.join(p);
        ensure!(
            fs::canonicalize(&path)?.starts_with(root),
            "split configuration escaped snapshot"
        );
        let next = yaml(&path)?;
        ensure!(
            next.is_object() || next.is_null(),
            "invalid split configuration table"
        );
        if !next.is_null() {
            merge(&mut table, next);
        }
    }
    Ok(table.as_object().unwrap().clone())
}
pub(super) fn address(raw: &str) -> Result<u64> {
    let hex = raw
        .strip_prefix("0x")
        .or_else(|| raw.strip_prefix("0X"))
        .context("missing IEEE address prefix")?;
    ensure!(hex.len() == 16, "invalid IEEE address length");
    Ok(u64::from_str_radix(hex, 16)?)
}
fn cardinality(v: &Value) -> usize {
    v.as_array()
        .map_or_else(|| v.as_object().map_or(0, Map::len), Vec::len)
}
fn text_attr(ep: u8, id: u16, s: &str) -> Result<Observation> {
    ensure!(s.len() < 255, "oversized Basic identity string");
    let mut bytes = vec![s.len() as u8];
    bytes.extend(s.bytes());
    Ok(attr(ep, 0, id, 0x42, bytes))
}
fn attr(ep: u8, cluster: u16, id: u16, ty: u8, bytes: Vec<u8>) -> Observation {
    Observation::Attribute {
        sequence: 0,
        endpoint: ep,
        cluster,
        attribute: id,
        result: AttributeResult::Value {
            data_type: ty,
            bytes,
        },
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Ep {
    ep_id: u8,
    prof_id: Option<u16>,
    dev_id: Option<u16>,
    in_cluster_list: Option<Vec<u16>>,
    out_cluster_list: Option<Vec<u16>>,
    dev_ver: Option<u8>,
    #[serde(default)]
    clusters: Value,
    #[serde(default)]
    binds: Value,
    #[serde(default)]
    configured_reportings: Value,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Node {
    #[serde(rename="type")] role:String,
    power_source:Option<String>,
    last_seen:Option<u64>,
    ieee_addr: String,
    nwk_addr: Option<u16>,
    #[serde(default)]
    ep_list: Vec<u8>,
    #[serde(default)]
    endpoints: BTreeMap<u8, Ep>,
    interview_state: Option<String>,
    interview_completed: Option<bool>,
    #[serde(rename = "modelId")]
    model: Option<String>,
    manuf_name: Option<String>,
    #[serde(default)]
    sw_build_id: Value,
    #[serde(default)]
    scheduled_ota: Value,
}

fn cached(
    ep: u8,
    d: &Ep,
    fallback: (Option<&str>, Option<&str>, Option<&str>),
    records: &mut Vec<Observation>,
) -> Result<()> {
    if d.in_cluster_list.as_ref().is_some_and(|l| l.contains(&0)) {
        let basic = &d.clusters["genBasic"]["attributes"];
        for (id, key, old) in [
            (4, "manufacturerName", fallback.0),
            (5, "modelId", fallback.1),
            (0x4000, "swBuildId", fallback.2),
        ] {
            if let Some(s) = basic[key].as_str().or(old) {
                if id == 0x4000 && (s.is_empty() || s.len() >= 255) { continue; }
                records.push(text_attr(ep, id, s)?);
            }
        }
    }
    if let Some(configured) = d.configured_reportings.as_array() {
        for entry in configured {
            if let Some((cluster @ 1, attribute @ (0x20 | 0x21))) = super::expectations::reported_attribute(entry) {
                records.push(Observation::ReportingConfigured {endpoint:ep, cluster, attribute});
            }
        }
    }
    let cover = &d.clusters["closuresWindowCovering"]["attributes"];
    for (id, key, ty) in [(0, "windowCoveringType", 0x30), (7, "configStatus", 0x18)] {
        if let Some(n) = cover[key].as_u64().and_then(|n| u8::try_from(n).ok()) {
            records.push(attr(ep, zigbee_devices::cover::CLUSTER, id, ty, vec![n]));
        }
    }
    for (cluster, family, fields) in [
        (0x0201, "hvacThermostat", &[
            (0, "localTemp", 0x29), (0x11,"occupiedCoolingSetpoint",0x29), (0x12,"occupiedHeatingSetpoint",0x29),
            (0x15,"minHeatSetpointLimit",0x29), (0x16,"maxHeatSetpointLimit",0x29),
            (0x17,"minCoolSetpointLimit",0x29), (0x18,"maxCoolSetpointLimit",0x29),
            (0x1b,"ctrlSeqeOfOper",0x30), (0x1c,"systemMode",0x30),
        ][..]),
        (0x0202, "hvacFanCtrl", &[(0,"fanMode",0x30), (1,"fanModeSequence",0x30)][..]),
    ] {
        if !d.in_cluster_list.as_ref().is_some_and(|list|list.contains(&cluster)) {continue;}
        let cached = &d.clusters[family]["attributes"];
        for &(id,key,ty) in fields {
            let Some(n) = cached[key].as_i64() else {continue};
            let bytes = if ty==0x29 {
                i16::try_from(n).ok().map(|v|v.to_le_bytes().to_vec())
            } else {
                u8::try_from(n).ok().map(|v|vec![v])
            };
            if let Some(bytes)=bytes {records.push(attr(ep,cluster,id,ty,bytes));}
        }
    }
    for (cluster, name, fields) in [
        (0x0b04, "haElectricalMeasurement", &[
            (0x0505, "rmsVoltage", 0x21), (0x0508, "rmsCurrent", 0x21), (0x050b, "activePower", 0x29),
            (0x0600, "acVoltageMultiplier", 0x21), (0x0601, "acVoltageDivisor", 0x21),
            (0x0602, "acCurrentMultiplier", 0x21), (0x0603, "acCurrentDivisor", 0x21),
            (0x0604, "acPowerMultiplier", 0x21), (0x0605, "acPowerDivisor", 0x21),
        ][..]),
        (0x0702, "seMetering", &[
            (0, "currentSummDelivered", 0x25), (0x0300, "unitOfMeasure", 0x30),
            (0x0301, "multiplier", 0x22), (0x0302, "divisor", 0x22),
            (0x0400, "instantaneousDemand", 0x2a),
        ][..]),
    ] {
        if !d.in_cluster_list.as_ref().is_some_and(|list| list.contains(&cluster)) { continue; }
        for &(id, key, ty) in fields {
            let Some(n) = d.clusters[name]["attributes"][key].as_i64() else { continue };
            let bytes = match ty {
                0x30 if u8::try_from(n).is_ok() => 1,
                0x21 if u16::try_from(n).is_ok() => 2,
                0x29 if i16::try_from(n).is_ok() => 2,
                0x22 if (0..=0x00ff_ffff).contains(&n) => 3,
                0x2a if (-8_388_608..=8_388_607).contains(&n) => 3,
                0x25 if (0..=0xffff_ffff_ffff).contains(&n) => 6,
                _ => continue,
            };
            records.push(attr(ep, cluster, id, ty, n.to_le_bytes()[..bytes].to_vec()));
        }
    }
    let color = &d.clusters["lightingColorCtrl"]["attributes"];
    for (id, key, ty) in [
        (0x400a, "colorCapabilities", 0x19),
        (0x400b, "colorTempPhysicalMin", 0x21),
        (0x400c, "colorTempPhysicalMax", 0x21),
    ] {
        if let Some(n) = color[key].as_u64() {
            records.push(attr(
                ep,
                0x300,
                id,
                ty,
                u16::try_from(n)?.to_le_bytes().to_vec(),
            ));
        }
    }
    Ok(())
}

impl Plan {
    pub fn load(root: &Path) -> Result<Self> {
        let root = fs::canonicalize(root)?;
        let config = yaml(&root.join("configuration.yaml"))?;
        let config = config
            .as_object()
            .context("configuration must be a mapping")?;
        let last_seen = config.get("advanced").and_then(|section|section.get("last_seen"))
            .map(|v| serde_json::from_value(v.clone())).transpose()?.unwrap_or_default();
        let mut availability=zigbee_bridge_rs::availability::Catalogue::import(config.get("availability"))?;
        let table = config_table(&root, config.get("devices"))?;
        let device_options = table
            .into_iter()
            .map(|(s, v)| Ok((address(&s)?, v)))
            .collect::<Result<BTreeMap<_, _>>>()?;
        let mut counts = Counts::new();
        count(
            &mut counts,
            "configured_groups_not_imported",
            config_table(&root, config.get("groups"))?.len(),
        );
        count(
            &mut counts,
            "configuration_sections_not_imported",
            config
                .keys()
                .filter(|k| !matches!(k.as_str(), "devices" | "groups" | "availability"))
                .count(),
        );
        for (file, key) in [
            (
                "coordinator_backup.json",
                "coordinator_backup_present_unopened",
            ),
            ("state.json", "retained_state_present_unopened"),
            ("secret.yaml", "secret_file_present_unopened"),
        ] {
            count(&mut counts, key, usize::from(root.join(file).try_exists()?));
        }
        if counts["coordinator_backup_present_unopened"] == 0 {
            count(&mut counts, "coordinator_backup_absent_from_snapshot", 1);
        }
        let db = open(&root.join("database.db")).context("open copied database.db")?;
        let mut members = Vec::new();
        let mut expectations = super::expectations::Expectations::default();
        let mut seen = BTreeSet::new();
        for (n, line) in BufReader::new(db).lines().enumerate() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let row: Value = serde_json::from_str(&line)
                .map_err(|_| anyhow::anyhow!("invalid database JSON on line {}", n + 1))?;
            let role = row.get("type").and_then(Value::as_str);
            if let Some(role @ ("Router" | "EndDevice" | "Coordinator")) = role {
                expectations.collect(&row, role)?;
            }
            match role {
                Some("Router" | "EndDevice") => {}
                Some("Group") => {
                    count(&mut counts, "database_groups_not_imported", 1);
                    count(
                        &mut counts,
                        "group_memberships_not_imported",
                        cardinality(&row["members"]),
                    );
                    continue;
                }
                Some("Coordinator") => {
                    count(&mut counts, "coordinator_records_left_on_radio", 1);
                    continue;
                }
                Some("GreenPower") => {
                    count(&mut counts, "green_power_records_not_imported", 1);
                    continue;
                }
                _ => {
                    count(&mut counts, "other_records_not_imported", 1);
                    continue;
                }
            }
            let node: Node = serde_json::from_value(row)
                .map_err(|_| anyhow::anyhow!("invalid device record on line {}", n + 1))?;
            let ieee = address(&node.ieee_addr)?;
            ensure!(
                seen.insert(ieee),
                "duplicate device IEEE in source database"
            );
            let options = device_options.get(&ieee).unwrap_or(&Value::Null);
            let vibration_timeout = options.get("vibration_timeout")
                .filter(|_| node.model.as_deref() == Some("TS0210"))
                .and_then(Value::as_u64).and_then(|v| u32::try_from(v).ok());
            let (temperature_force_update, unknown_ha) = temperature_option(options.get("homeassistant"));
            count(
                &mut counts,
                "device_option_keys_not_imported",
                options.as_object().map_or(0, |m| {
                    m.keys()
                        .filter(|k| match k.as_str() {
                            "friendly_name" | "disabled" | "availability" => false,
                            "homeassistant" => unknown_ha,
                            "vibration_timeout" => vibration_timeout.is_none(),
                            _ => true,
                        })
                        .count()
                }),
            );
            if unknown_ha {
                count(
                    &mut counts,
                    "device_homeassistant_overrides_not_imported",
                    1,
                );
            }
            count(
                &mut counts,
                "bindings_not_imported",
                node.endpoints.values().map(|e| cardinality(&e.binds)).sum(),
            );
            count(
                &mut counts,
                "reporting_records_not_imported",
                node.endpoints
                    .values()
                    .map(|e| cardinality(&e.configured_reportings))
                    .sum(),
            );
            count(
                &mut counts,
                "ota_schedules_not_imported",
                usize::from(!node.scheduled_ota.is_null()),
            );
            if options["disabled"] == true
                || options
                    .get("homeassistant")
                    .is_some_and(|v| v.is_null() || v == false)
            {
                count(&mut counts, "disabled_devices_held_back", 1);
                continue;
            }
            let complete = node
                .interview_state
                .as_deref()
                .map_or(node.interview_completed == Some(true), |s| {
                    s == "SUCCESSFUL"
                });
            if !complete {
                count(&mut counts, "incomplete_interviews_held_back", 1);
                continue;
            }
            let mut eps = BTreeSet::new();
            ensure!(
                node.ep_list.iter().all(|e| eps.insert(*e)),
                "duplicate endpoint in source list"
            );
            let mut records = vec![Observation::Target {
                ieee: format!("{ieee:016x}"),
                short: node.nwk_addr.unwrap_or(0xfffe),
                identity_source: IdentitySource::Z2mDatabase,
            }];
            let mut valid = Vec::new();
            let mut missing = node
                .ep_list
                .iter()
                .any(|e| (1..=240).contains(e) && !node.endpoints.contains_key(e));
            for (&ep, d) in &node.endpoints {
                ensure!(ep == d.ep_id, "endpoint key/identity mismatch");
                if !(1..=240).contains(&ep) {
                    count(&mut counts, "non_application_endpoints_excluded", 1);
                    continue;
                }
                let (Some(profile), Some(device), Some(inputs), Some(outputs)) =
                    (d.prof_id, d.dev_id, &d.in_cluster_list, &d.out_cluster_list)
                else {
                    missing = true;
                    continue;
                };
                valid.push(ep);
                records.push(Observation::SimpleDescriptor {
                    sequence: 0,
                    endpoint: ep,
                    descriptor: Descriptor {
                        profile,
                        device,
                        version: d.dev_ver.unwrap_or(0xff),
                        servers: inputs.clone(),
                        clients: outputs.clone(),
                    },
                });
                cached(ep, d, (node.manuf_name.as_deref(), node.model.as_deref(), node.sw_build_id.as_str()), &mut records)?;
                if d.dev_ver.is_none() {
                    count(&mut counts, "descriptor_versions_unknown", 1);
                }
            }
            if missing || valid.is_empty() {
                count(&mut counts, "incomplete_descriptors_held_back", 1);
                continue;
            }
            if valid.iter().all(|ep| eps.contains(ep)) && !node.ep_list.is_empty() {
                records.push(Observation::ActiveEndpoints {
                    sequence: 0,
                    endpoints: valid.clone(),
                });
            }
            records.push(Observation::Finished);
            count(&mut counts, "imported_application_endpoints", valid.len());
            let name = match options.get("friendly_name") {
                Some(v) => v.as_str().context("friendly_name must be text")?.to_owned(),
                None => format!("0x{ieee:016x}"),
            };
            count(&mut counts, "vibration_timeout_overrides_imported", usize::from(vibration_timeout.is_some()));
            count(&mut counts, "temperature_force_update_overrides_imported", usize::from(temperature_force_update.is_some()));
            availability.import_member(ieee,&node.role,node.power_source.as_deref(),node.last_seen,options.get("availability"))?;
            count(&mut counts,"availability_enabled_devices",usize::from(availability.enabled(ieee)));
            members.push(Member {
                device: Device {
                    ieee,
                    name,
                    endpoint: valid[0],
                    short: None,
                    temperature_force_update,
                    vibration_timeout,
                },
                report: InterviewReport {
                    finished: true,
                    entries: records
                        .into_iter()
                        .map(|observation| Entry {
                            unix_ms: 0,
                            observation,
                        })
                        .collect(),
                },
            });
        }
        count(
            &mut counts,
            "configured_devices_missing_from_database",
            device_options
                .keys()
                .filter(|id| !seen.contains(id))
                .count(),
        );
        if counts.get("coordinator_records_left_on_radio").copied() != Some(1) {
            count(
                &mut counts,
                "coordinator_identity_record_missing_or_ambiguous",
                1,
            );
        }
        count(&mut counts, "imported_devices", members.len());
        for (key, n) in expectations.counts() {
            count(&mut counts, key, n);
        }
        Ok(Self {
            availability,
            last_seen,
            members,
            counts,
            expectations,
        })
    }
}

#[cfg(test)]
mod cache_tests {
    use super::*;

    #[test]
    fn cached_metering_checks_roles_and_wire_ranges() -> Result<()> {
        let endpoint: Ep = serde_json::from_value(serde_json::json!({
            "epId": 2, "inClusterList": [0x0702], "outClusterList": [0x0b04],
            "clusters": {
                "seMetering": {"attributes": {"instantaneousDemand": -3, "currentSummDelivered": 0xffff_ffff_ffff_i64,
                    "multiplier": 0x0100_0000, "divisor": 100, "unitOfMeasure": "kWh"}},
                "haElectricalMeasurement": {"attributes": {"rmsVoltage": 230}}
            }
        }))?;
        let mut records = Vec::new();
        cached(2, &endpoint, (None, None, None), &mut records)?;
        let fields: Vec<_> = records.iter().filter_map(|obs| match obs {
            Observation::Attribute {endpoint: 2, cluster: 0x0702, attribute, result: AttributeResult::Value {data_type, bytes}, ..}
                => Some((*attribute, *data_type, bytes.as_slice())),
            _ => None,
        }).collect();
        assert_eq!(records.len(), 3);
        assert!(fields.contains(&(0, 0x25, &[255u8; 6][..])));
        assert!(fields.contains(&(0x0302, 0x22, &[100, 0, 0][..])));
        assert!(fields.contains(&(0x0400, 0x2a, &[253, 255, 255][..])));
        Ok(())
    }

    #[test]
    fn cached_thermostat_requires_input_cluster_and_checked_wire_values() -> Result<()> {
        let ep: Ep = serde_json::from_value(serde_json::json!({
            "epId": 9, "inClusterList":[0x201], "outClusterList":[0x202],
            "clusters":{
                "hvacThermostat":{"attributes":{"localTemp":-125,"ctrlSeqeOfOper":2,"occupiedHeatingSetpoint":75000}},
                "hvacFanCtrl":{"attributes":{"fanMode":2}}
            }
        }))?;
        let mut attrs=Vec::new();
        cached(9,&ep,(None,None,None),&mut attrs)?;
        assert_eq!(attrs.len(),2);
        assert!(attrs.iter().any(|r|matches!(r,Observation::Attribute {endpoint:9,cluster:0x201,attribute:0,
            result:AttributeResult::Value{data_type:0x29,bytes},..} if bytes==&(-125i16).to_le_bytes())));
        assert!(attrs.iter().any(|r|matches!(r,Observation::Attribute {endpoint:9,cluster:0x201,attribute:0x1b,
            result:AttributeResult::Value{data_type:0x30,bytes},..} if bytes==&[2])));
        Ok(())
    }
}
