//! One shared, adjustable interview for the window-covering bridge and transport tests.
use super::{
    AttributeResult, BTreeMap, Bindings, Descriptor, Entry, IdentitySource, InterviewReport,
    Observation, Result,
};
use zigbee_devices::cover::CLUSTER;

pub(super) fn bind(
    bindings: &mut Bindings,
    (ieee, ep): (u64, u8),
    profile: u16,
    server: bool,
    kind: u8,
    config: u8,
    commands: &[u8],
) -> Result<()> {
    let mut facts = vec![
        Observation::Target {
            ieee: format!("{ieee:016x}"),
            short: 0x743,
            identity_source: IdentitySource::Announcement,
        },
        Observation::ActiveEndpoints {
            sequence: 1,
            endpoints: vec![ep],
        },
        Observation::SimpleDescriptor {
            sequence: 2,
            endpoint: ep,
            descriptor: Descriptor {
                profile,
                device: if server { 0x0202 } else { 0x0203 },
                version: 1,
                servers: if server { vec![0, CLUSTER] } else { vec![0] },
                clients: if server { vec![] } else { vec![CLUSTER] },
            },
        },
    ];
    for (cluster, attribute, data_type, bytes) in [
        (0, 5, 0x42, b"\x0aNew Roller".to_vec()),
        (CLUSTER, 0, 0x30, vec![kind]),
        (CLUSTER, 7, 0x18, vec![config]),
    ] {
        facts.push(Observation::Attribute {
            sequence: 3,
            endpoint: ep,
            cluster,
            attribute,
            result: AttributeResult::Value { data_type, bytes },
        });
    }
    facts.push(Observation::CommandsReceived {
        endpoint: ep,
        cluster: CLUSTER,
        commands: commands.to_vec(),
    });
    facts.push(Observation::Finished);
    bindings.add(
        &InterviewReport {
            finished: true,
            entries: facts
                .into_iter()
                .map(|observation| Entry {
                    unix_ms: 0,
                    observation,
                })
                .collect(),
        },
        &BTreeMap::from([(ieee, "Shade".into())]),
    )?;
    Ok(())
}
