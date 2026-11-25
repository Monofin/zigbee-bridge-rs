//! Fold one interview attempt into wire evidence. Never combine attempts or
//! treat a fallback endpoint probe as the device's complete endpoint list.
use anyhow::{Result, ensure};
use std::{collections::BTreeMap, path::Path};
use zigbee_bridge_rs::interview_log::{AttributeResult, InterviewReport, Observation};
use zigbee_catalogue::{Descriptor, Endpoint, identify};

fn basic_string(bytes: &[u8]) -> Option<&str> {
    let (&len, text) = bytes.split_first()?;
    if len == 0xff || len as usize != text.len() {
        return None;
    }
    std::str::from_utf8(text).ok()
}

fn evidence(report: &InterviewReport) -> Result<Descriptor> {
    let mut evidence = Descriptor::default();
    let mut listed = None;
    let mut descriptors = BTreeMap::new();
    let mut target = false;
    for entry in &report.entries {
        match &entry.observation {
            Observation::Target { .. } => {
                ensure!(!target, "multiple targets in one interview");
                target = true;
            }
            Observation::ActiveEndpoints { endpoints, .. } => {
                ensure!(
                    target && listed.replace(endpoints).is_none(),
                    "unexpected endpoint list"
                );
            }
            Observation::SimpleDescriptor {
                endpoint,
                descriptor,
                ..
            } => {
                ensure!(
                    target && descriptors.insert(*endpoint, descriptor).is_none(),
                    "unexpected descriptor"
                );
            }
            Observation::Attribute {
                endpoint,
                cluster: 0,
                attribute,
                result:
                    AttributeResult::Value {
                        data_type: 0x42,
                        bytes,
                    },
                ..
            } if matches!(attribute, 4 | 5)
                && descriptors
                    .get(endpoint)
                    .is_some_and(|d| d.servers.contains(&0)) =>
            {
                if let Some(text) = basic_string(bytes) {
                    let slot = if *attribute == 4 {
                        &mut evidence.manufacturer
                    } else {
                        &mut evidence.model
                    };
                    ensure!(
                        slot.as_deref().is_none_or(|old| old == text),
                        "conflicting Basic identity"
                    );
                    *slot = Some(text.to_owned());
                }
            }
            _ => {}
        }
    }
    if let Some(list) = listed {
        for &id in list {
            let mut ep = Endpoint {
                id,
                ..Default::default()
            };
            if let Some(d) = descriptors.get(&id) {
                ep.profile = Some(d.profile);
                ep.device = Some(d.device);
                ep.servers = Some(d.servers.iter().copied().collect());
                ep.clients = Some(d.clients.iter().copied().collect());
            }
            evidence.endpoints.push(ep);
        }
    }
    Ok(evidence)
}

pub fn print(report: &InterviewReport, database: &Path) -> Result<()> {
    let evidence = evidence(report)?;
    let candidates = identify(database, &evidence)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "finished":report.finished, "evidence":evidence, "definitions":candidates
        }))?
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use zigbee_bridge_rs::interview_log::{Descriptor as WireDescriptor, Entry, IdentitySource};

    #[test]
    fn fallback_probes_keep_identity_but_cannot_claim_a_complete_descriptor_set() -> Result<()> {
        let observations = [
            Observation::Target {
                ieee: String::new(),
                short: 123,
                identity_source: IdentitySource::Announcement,
            },
            Observation::SimpleDescriptor {
                sequence: 1,
                endpoint: 1,
                descriptor: WireDescriptor {
                    profile: 260,
                    device: 257,
                    version: 1,
                    servers: vec![0, 6],
                    clients: vec![],
                },
            },
            Observation::Attribute {
                sequence: 2,
                endpoint: 1,
                cluster: 0,
                attribute: 5,
                result: AttributeResult::Value {
                    data_type: 0x42,
                    bytes: vec![2, b'A', b' '],
                },
            },
        ];
        let mut report = InterviewReport {
            finished: false,
            entries: observations
                .into_iter()
                .map(|observation| Entry {
                    unix_ms: 0,
                    observation,
                })
                .collect(),
        };
        let fallback = evidence(&report)?;
        assert_eq!(fallback.model.as_deref(), Some("A "));
        assert!(fallback.endpoints.is_empty());
        report.entries.insert(
            1,
            Entry {
                unix_ms: 0,
                observation: Observation::ActiveEndpoints {
                    sequence: 0,
                    endpoints: vec![1, 242],
                },
            },
        );
        let partial = evidence(&report)?;
        assert_eq!(partial.endpoints.len(), 2);
        assert_eq!(partial.endpoints[0].device, Some(257));
        assert_eq!(partial.endpoints[1].servers, None);
        assert_eq!(basic_string(&[2, b'A']), None);
        assert_eq!(basic_string(&[0xff]), None);
        Ok(())
    }
}
