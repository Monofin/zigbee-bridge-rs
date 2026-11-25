//! Reporting setup for admitted members shares the cancellable interview port.
use super::Bindings;
use crate::{
    interview::reporting::{Attribute, Endpoint},
    interview_log::{InterviewLog, InterviewReport, Observation},
};
use anyhow::Result;
use std::fs;
use zigbee_devices::{
    cover::{self, Axis},
    hvac::{self, Property},
};

pub(super) fn plans(devices: &Bindings, ieee: u64) -> Vec<Endpoint> {
    let mut endpoints = Vec::new();
    for (&(_, ep), b) in devices.devices.range((ieee, 0)..=(ieee, 255)) {
        for channel in &b.io {
            if let Some(ty)=channel.reporting_type {
                endpoints.push(Endpoint {
                    endpoint: ep, profile: b.descriptor.profile, cluster: channel.layout.cluster(),
                    attributes: vec![Attribute {id: zigbee_devices::basic_io::PRESENT, ty}],
                    discover_cover: false,
                });
            }
        }
        let mut equipment = std::collections::BTreeMap::new();
        for field in b.equipment.iter().filter(|f| f.reportable) {
            if let Some(ty) = zigbee_zcl::standard_attributes::wire_type(field.cluster, field.id) {
                equipment.entry(field.cluster).or_insert_with(Vec::new).push(Attribute { id:field.id, ty });
            }
        }
        for (cluster, attributes) in equipment {
            endpoints.push(Endpoint {endpoint:ep, profile:b.descriptor.profile, cluster, attributes, discover_cover:false});
        }
        for r in b.environment.iter().filter(|r| r.attribute == 0) {
            endpoints.push(Endpoint {
                endpoint: ep, profile: b.descriptor.profile, cluster: r.cluster,
                attributes: vec![Attribute { id: r.attribute, ty: r.wire_type() }],
                discover_cover: false,
            });
        }
        if let Some(cover) = b.cover {
            let attributes = [(Axis::Lift, 8), (Axis::Tilt, 9)]
                .into_iter()
                .filter(|(a, _)| cover.reads(*a))
                .map(|(_, id)| Attribute { id, ty: 0x20 })
                .collect();
            endpoints.push(Endpoint {
                endpoint: ep,
                profile: b.descriptor.profile,
                cluster: cover::CLUSTER,
                attributes,
                discover_cover: true,
            });
        }
        for cluster in [hvac::THERMOSTAT, hvac::FAN] {
            let attributes: Vec<_> = hvac::PROPERTIES
                .into_iter()
                .filter(|p| p.address().0 == cluster && b.hvac.observed(*p))
                .map(|p| Attribute {
                    id: p.address().1,
                    ty: if matches!(p, Property::SystemMode | Property::FanMode) {
                        0x30
                    } else {
                        0x29
                    },
                })
                .collect();
            if !attributes.is_empty() {
                endpoints.push(Endpoint {
                    endpoint: ep,
                    profile: b.descriptor.profile,
                    cluster,
                    attributes,
                    discover_cover: false,
                });
            }
        }
    }
    endpoints
}

pub(super) fn persist(
    dir: &std::path::Path,
    ieee: u64,
    result: InterviewReport,
    devices: &mut Bindings,
    names: &std::collections::BTreeMap<u64, String>,
) -> Result<bool> {
    let path = dir.join(format!("{ieee:016x}.member"));
    if !path.exists() {
        return Ok(false);
    }
    let mut report = InterviewReport::open(&path)?;
    let Some(end) = report.entries.pop() else {
        return Ok(false);
    };
    anyhow::ensure!(
        matches!(end.observation, Observation::Finished),
        "incomplete member"
    );
    let mut changed = false;
    let mut discovery_changed = false;
    for e in result.entries {
        use Observation::*;
        match &e.observation {
            ReportingReadback{endpoint,cluster,attribute,..}=>{
                report.entries.retain(|old|!matches!(&old.observation,ReportingReadback{endpoint:a,cluster:b,attribute:c,..} if (a,b,c)==(endpoint,cluster,attribute)));
            }
            CoverCommandsReviewed{endpoint,..}=>{
                report.entries.retain(|old|!matches!(&old.observation,CoverCommandsReviewed{endpoint:a,..} if a==endpoint));
            }
            CommandsReceived{endpoint,cluster,commands}=>{
                if commands.iter().all(|cmd|report.entries.iter().any(|old|matches!(&old.observation,
                    CommandsReceived{endpoint:a,cluster:b,commands:prior} if (a,b)==(endpoint,cluster) && prior.contains(cmd)))) {continue;}
            }
            Attribute{endpoint,cluster,attribute,result:crate::interview_log::AttributeResult::Value{..},..}=>{
                report.entries.retain(|old|!matches!(&old.observation,Attribute{endpoint:a,cluster:b,attribute:c,..} if (a,b,c)==(endpoint,cluster,attribute)));
            }
            _=>continue,
        }
        report.entries.push(e);
        changed = true;
    }
    if changed {
        report.entries.push(end);
        let mut candidate = devices.clone();
        candidate.add(&report, names)?;
        let mut log = InterviewLog::create(dir)?;
        for e in &report.entries {
            log.record_entry(e)?;
        }
        fs::rename(log.path(), &path)?;
        fs::File::open(dir)?.sync_all()?;
        // Keep live timers and cached observations. Only the cover evidence was probed again.
        for (&id, b) in devices.devices.range_mut((ieee, 0)..=(ieee, 255)) {
            if b.cover.is_some()
                && let Some(updated) = candidate.devices.get(&id)
            {
                discovery_changed |= b.cover != updated.cover;
                b.cover = updated.cover;
            }
        }
    }
    Ok(discovery_changed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interview_log::{AttributeResult, Descriptor, Entry, IdentitySource};
    use std::collections::BTreeMap;
    fn report(facts: Vec<Observation>) -> InterviewReport {
        InterviewReport {
            finished: true,
            entries: facts
                .into_iter()
                .map(|observation| Entry {
                    unix_ms: 0,
                    observation,
                })
                .collect(),
        }
    }
    #[test]
    fn legacy_cover_gains_only_confirmed_controls_and_restores_them_from_disk() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let id = 0x1234u64;
        let ep = 4;
        let names = BTreeMap::from([(id, "blind".into())]);
        let historical = report(vec![
            Observation::Target {
                ieee: format!("{id:016x}"),
                short: 42,
                identity_source: IdentitySource::Z2mDatabase,
            },
            Observation::ActiveEndpoints {
                sequence: 1,
                endpoints: vec![ep],
            },
            Observation::SimpleDescriptor {
                sequence: 2,
                endpoint: ep,
                descriptor: Descriptor {
                    profile: 0x104,
                    device: 0x202,
                    version: 1,
                    servers: vec![cover::CLUSTER],
                    clients: vec![],
                },
            },
            Observation::Finished,
        ]);
        let mut bindings = Bindings::default();
        bindings.add(&historical, &names)?;
        let mut log = InterviewLog::create(dir.path())?;
        for e in &historical.entries {
            log.record_entry(e)?;
        }
        let member = dir.path().join(format!("{id:016x}.member"));
        fs::rename(log.path(), &member)?;
        assert!(!bindings.devices[&(id, ep)].cover.unwrap().sets(Axis::Lift));
        let attrs = |(id, ty, v)| Observation::Attribute {
            sequence: 1,
            endpoint: ep,
            cluster: cover::CLUSTER,
            attribute: id,
            result: AttributeResult::Value {
                data_type: ty,
                bytes: vec![v],
            },
        };
        let snapshot = |commands| Observation::CoverCommandsReviewed {
            endpoint: ep,
            commands,
        };
        let mut facts = vec![
            attrs((0, 0x30, 8)),
            attrs((7, 0x18, 0x18)),
            snapshot(vec![5]),
        ];
        for expected in [true, false] {
            assert!(persist(
                dir.path(),
                id,
                report(facts),
                &mut bindings,
                &names
            )?);
            let mut reopened = Bindings::default();
            reopened.add(&InterviewReport::open(&member)?, &names)?;
            for b in [&bindings, &reopened] {
                let cover = b.devices[&(id, ep)].cover.unwrap();
                assert_eq!(cover.sets(Axis::Lift), expected);
                assert!(!cover.sets(Axis::Tilt));
                assert!(cover.reads(Axis::Lift) && cover.reads(Axis::Tilt));
            }
            facts = vec![snapshot(vec![])];
        }
        Ok(())
    }
}
