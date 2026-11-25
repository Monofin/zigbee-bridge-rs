//! Optional offline comparison of the normal binding/command entry points.
//! The projection provided by the operator carries product data only.
use super::*;
use crate::interview_log::{AttributeResult, Entry, IdentitySource, InterviewReport, Observation};
use crate::mqtt::{Command, Operation};
use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
};

#[derive(Deserialize)]
struct Group {
    model: String,
    manufacturer: String,
    endpoints: Vec<Ep>,
    expected: Vec<Exposed>,
}
#[derive(Deserialize)]
struct Ep {
    id: u8,
    profile: Option<u16>,
    device: Option<u16>,
    #[serde(default)]
    servers: Vec<u16>,
    #[serde(default)]
    clients: Vec<u16>,
    color_caps: Option<u16>,
}
#[derive(Deserialize)]
struct Exposed {
    property: String,
    values: Vec<Value>,
}

#[test]
#[ignore = "requires the private, sanitized household projection"]
fn household_access() -> Result<()> {
    let groups: Vec<Group> =
        serde_json::from_reader(fs::File::open(env::var("ZIGBEE_COVERAGE_INPUT")?)?)?;
    let mut output = Vec::new();
    for (index, group) in groups.into_iter().enumerate() {
        let mut bridge = Bindings::default();
        let mut ev = vec![Observation::Target {
            ieee: "0000000000000001".into(),
            short: 1,
            identity_source: IdentitySource::Z2mDatabase,
        }];
        ev.push(Observation::ActiveEndpoints {
            sequence: 1,
            endpoints: group
                .endpoints
                .iter()
                .filter(|e| e.profile.is_some() && e.device.is_some())
                .map(|e| e.id)
                .collect(),
        });
        for e in group.endpoints {
            let (Some(profile), Some(dev)) = (e.profile, e.device) else {
                continue;
            };
            if e.servers.contains(&0) {
                for (a, s) in [(4, &group.manufacturer), (5, &group.model)] {
                    let mut bytes = vec![s.len().try_into()?];
                    bytes.extend(s.as_bytes());
                    ev.push(Observation::Attribute {
                        sequence: 0,
                        endpoint: e.id,
                        cluster: 0,
                        attribute: a,
                        result: AttributeResult::Value {
                            data_type: 0x42,
                            bytes,
                        },
                    });
                }
            }
            if let Some(caps) = e.color_caps {
                ev.push(Observation::Attribute {
                    sequence: 0,
                    endpoint: e.id,
                    cluster: 0x300,
                    attribute: 0x400a,
                    result: AttributeResult::Value {
                        data_type: 0x19,
                        bytes: caps.to_le_bytes().to_vec(),
                    },
                });
            }
            ev.push(Observation::SimpleDescriptor {
                sequence: 0,
                endpoint: e.id,
                descriptor: Descriptor {
                    profile,
                    device: dev,
                    version: 0xff,
                    servers: e.servers,
                    clients: e.clients,
                },
            });
        }
        ev.push(Observation::Finished);
        bridge.add(
            &InterviewReport {
                finished: true,
                entries: ev
                    .into_iter()
                    .map(|observation| Entry {
                        unix_ms: 0,
                        observation,
                    })
                    .collect(),
            },
            &BTreeMap::new(),
        )?;
        let mut endpoints = Vec::new();
        for (&id, b) in &bridge.devices {
            let ep = endpoint(id.1, &b.descriptor);
            let mut props = BTreeMap::<String, Vec<Value>>::new();
            for p in &group.expected {
                props
                    .entry(p.property.clone())
                    .or_default()
                    .extend(p.values.iter().cloned());
            }
            let mut reports = BTreeSet::from([String::from("linkquality")]);
            for (key, samples) in [
                ("state", vec![json!("OFF"), json!("ON"), json!("OPEN")]),
                ("brightness", vec![json!(42)]),
                ("color_temp", vec![json!(275)]),
            ] {
                props.entry(key.into()).or_default().extend(samples);
            }
            if b.is_light {
                let light = b.light(&ep)?;
                for p in [
                    Property::State,
                    Property::Brightness,
                    Property::ColorTemperature,
                    Property::Color,
                ] {
                    if light.supports(p) && b.permits(p, |a| a.report) {
                        reports.insert(p.name().into());
                    }
                }
            }
            for &(key, attr) in &SENSORS {
                if attr.available(&ep) {
                    reports.insert(key.into());
                }
            }
            if b.is_light && b.controls.level_config { reports.insert("level_config".into()); }
            if b.controls.startup_mireds.is_some() {
                reports.insert(zigbee_devices::standard_controls::STARTUP.to_owned());
                props.entry(zigbee_devices::standard_controls::STARTUP.to_owned()).or_default().push(json!("previous"));
            }
            if b.controls.hue_effects {
                // The matched Philips2 decoder publishes these properties on a
                // manufacturer-specific state report, separately from light attributes.
                reports.extend(["effect", "effect_speed"].map(String::from));
            }
            b.household.coverage(&mut reports);
            if b.button.is_some() {
                reports.insert("action".into());
            }
            if let Some(d) = b.datapoints {
                if d.family() == zigbee_devices::tuya::household::Family::Cover {
                    reports.extend(["state", "position"].map(String::from));
                    props.entry("position".into()).or_default().push(json!(42));
                }
                for field in d.fields() {
                    reports.insert(field.key.clone());
                    let examples = props.entry(field.key.clone()).or_default();
                    examples.extend(field.choices.iter().map(|v| json!(v)));
                    if let Some(labels) = &field.switch {
                        examples.extend(labels.iter().map(|label| json!(label)));
                    }
                    examples.extend([json!(field.min), json!(true), json!(false)]);
                }
            }
            for k in &reports {
                props.entry(k.clone()).or_default();
            }
            let mut read = BTreeSet::new();
            let mut write = BTreeSet::new();
            let mut state_after_write = BTreeSet::new();
            for (property, samples) in props {
                let plan = |op, v| {
                    bridge
                        .plan(
                            &Command {
                                device: b.name.clone(),
                                operation: op,
                                properties: [(property.clone(), v)].into_iter().collect(),
                            },
                            1,
                        )
                };
                if plan(Operation::Get, json!("")).is_ok() {
                    read.insert(property.clone());
                }
                if let Some((_, steps)) = samples.into_iter().find_map(|v| plan(Operation::Set, v).ok()) {
                    for (_, response) in steps {
                        if let Some(Read::TuyaLamp(setting)) = response {
                            // Ember publishes this setting only after the addressed
                            // device returns a matching successful default response.
                            state_after_write.insert(setting.state().0);
                        }
                    }
                    write.insert(property);
                }
            }
            endpoints.push(json!({"endpoint":id.1,"read":read,"write":write,"report":reports,"state_after_write":state_after_write}));
        }
        output.push(json!({"index":index,"endpoints":endpoints}));
    }
    fs::write(
        env::var("ZIGBEE_COVERAGE_OUTPUT").context("coverage output path")?,
        serde_json::to_vec(&output)?,
    )?;
    Ok(())
}
