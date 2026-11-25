//! Native MQTT controls for standard equipment on interviewed application endpoints.
use super::{
    Bindings, Identity, Planned, Read,
    ember::{Pending, submitted},
};
use crate::mqtt::{Operation as MqttOperation, Publisher, discovery::Feature};
use anyhow::{Result, ensure};
use serde_json::Value as Json;
use std::{collections::BTreeMap, sync::LazyLock};
use tokio::time::Instant;
use zigbee_devices::equipment::{
    FIELDS, Field, Format,
    commands::{self, ScheduleQuery},
    commissioning::{Capabilities, Setting},
};
use zigbee_zcl::{
    Direction, Frame,
    attributes::{Status, WriteStatus},
    foundation::Payload,
};

#[derive(Clone, Copy)]
pub(super) enum Operation {
    Read(&'static Field),
    Write(&'static Field),
    OutputCommand(&'static Field, u8),
    Constraints(Setting),
    Panel,
    Schedule(ScheduleQuery),
}

#[derive(Clone, Copy, Default)]
pub(super) struct Commands {
    commissioning: Capabilities,
    pub(super) output: Option<zigbee_devices::equipment::level_output::Output>,
    panel: bool,
    barrier: bool,
    partial: bool,
    siren: bool,
    max_duration: Option<u16>,
    schedule: bool,
    pub(super) weekly: zigbee_devices::equipment::schedule::Capabilities,
}

impl Commands {
    pub(super) fn resolve<'a>(
        servers: &[u16],
        get: impl Fn(u16, u16) -> Option<(u8, &'a [u8])>,
    ) -> Self {
        let partial = matches!(get(0x103,3),Some((0x18,[v])) if v&1!=0);
        let max_duration = match get(0x502, 0) {
            Some((0x21, [a, b])) => Some(u16::from_le_bytes([*a, *b])).filter(|v| *v != u16::MAX),
            _ => None,
        };
        let weekly = if servers.contains(&0x201) {
            zigbee_devices::equipment::schedule::Capabilities::resolve(|id| get(0x201, id))
        } else {
            Default::default()
        };
        let schedule = servers.contains(&0x201)
            && matches!(get(0x201, 0x21), Some((0x20, [1..=255])))
            && matches!(get(0x201, 0x22), Some((0x20, [1..=10])));
        Self {
            output: None,
            commissioning: Capabilities::resolve(&get),
            panel: servers.contains(&commands::PANEL),
            barrier: servers.contains(&0x103),
            partial,
            siren: servers.contains(&0x502),
            max_duration,
            schedule,
            weekly,
        }
    }

    pub(super) fn active(self) -> bool {
        self.panel || self.barrier || self.siren || self.weekly.supported()
    }
    pub(super) fn handles(self, cluster: u16) -> bool {
        self.panel && cluster == commands::PANEL || self.schedule && cluster == 0x201
    }

    pub(super) fn features(self) -> impl Iterator<Item = Feature> {
        [
            self.panel.then_some(Feature::TextSensor("panel_status")),
            self.panel
                .then_some(Feature::TextSensor("panel_alarm_status")),
            self.panel.then_some(Feature::Measurement {
                property: "panel_seconds_remaining",
                unit: Some("s"),
            }),
            self.barrier.then_some(Feature::Barrier {
                partial: self.partial,
            }),
            self.schedule
                .then_some(Feature::CompositeSensor("weekly_schedule_page")),
            self.weekly
                .supported()
                .then_some(Feature::CompositeSensor("weekly_schedule")),
        ]
        .into_iter()
        .flatten()
    }
    fn plan(
        self,
        ep: u8,
        op: MqttOperation,
        name: &str,
        value: &Json,
        seq: u8,
    ) -> Result<Option<Planned>> {
        use MqttOperation::{Get, Set};
        let step = match (name, op) {
            (
                "panel_status"
                | "panel_alarm_status"
                | "panel_seconds_remaining"
                | "panel_audible_notification",
                Get,
            ) if self.panel => (
                commands::panel_query(ep, seq),
                Some(Read::Equipment(Operation::Panel)),
            ),
            ("weekly_schedule_page", Get) if self.schedule => {
                let query = ScheduleQuery::parse(value)?;
                (
                    query.request(ep, seq)?,
                    Some(Read::Equipment(Operation::Schedule(query))),
                )
            }
            ("barrier_command", Set) if self.barrier => {
                (commands::move_barrier(ep, seq, self.partial, value)?, None)
            }
            ("siren_warning", Set) if self.siren => {
                (commands::warning(ep, seq, self.max_duration, value)?, None)
            }
            ("siren_squawk", Set) if self.siren => (commands::squawk(ep, seq, value)?, None),
            _ => return Ok(None),
        };
        Ok(Some(step))
    }
}

/// Precedence applies only to endpoint properties that this module owns.
pub(super) fn plan(
    b: &super::Binding,
    ep: u8,
    cmd: &crate::mqtt::Command,
    seq: u8,
) -> Result<Option<Vec<Planned>>> {
    let mut steps = Vec::new();
    for (key, value) in &cmd.properties {
        if let Some(field) = b.equipment.iter().find(|f| f.name == key) {
            if let Some(output) = b.equipment_commands.output
                && field.id == 0
                && matches!(field.cluster, 6 | 8)
                && cmd.operation == MqttOperation::Set
            {
                let req = output.set(field, ep, seq.wrapping_add(steps.len() as u8), value)?;
                let command_id = Frame::decode(&req.frame)?.header.command_id;
                steps.push((
                    req,
                    Some(Read::Equipment(Operation::OutputCommand(field, command_id))),
                ));
                continue;
            }
            if cmd.operation == MqttOperation::Set && Setting::required(field) {
                ensure!(
                    cmd.properties.len() == 1,
                    "send one commissioning setting at a time"
                );
            }
            steps.push(field_plan(
                field,
                ep,
                cmd.operation,
                value,
                seq.wrapping_add(steps.len() as u8),
            )?);
        } else if let Some(step) = b.equipment_commands.plan(
            ep,
            cmd.operation,
            key,
            value,
            seq.wrapping_add(steps.len() as u8),
        )? {
            steps.push(step);
        }
    }
    if steps.is_empty() {
        return Ok(None);
    }
    ensure!(
        steps.len() == cmd.properties.len(),
        "send equipment operations in their own request"
    );
    Ok(Some(steps))
}

pub(super) fn field_plan(
    field: &'static Field,
    ep: u8,
    op: MqttOperation,
    value: &Json,
    seq: u8,
) -> Result<Planned> {
    let write = op == MqttOperation::Set;
    if write && let Some(setting) = Setting::prepare(field, value)? {
        return Ok((
            setting.read_constraints(ep, seq)?,
            Some(Read::Equipment(Operation::Constraints(setting))),
        ));
    }
    Ok((
        field.request(ep, seq, write.then_some(value))?,
        Some(Read::Equipment(if write {
            Operation::Write(field)
        } else {
            Operation::Read(field)
        })),
    ))
}

pub(super) fn feature(field: &'static Field, commands: Commands) -> Feature {
    let property = field.name;
    if let Some(output) = commands.output
        && field.id == 0
    {
        if field.cluster == 6 {
            return Feature::Switch;
        }
        if field.cluster == 8 {
            return match output.bounds {
                Some((lo, hi)) if lo < hi => Feature::Number {
                    property,
                    min: f64::from(lo),
                    max: f64::from(hi),
                    step: 1.,
                    unit: None,
                },
                _ => Feature::TextSensor(property),
            };
        }
    }
    if (field.cluster, field.id) == (0x0200, 0x0015) {
        return Feature::Sensor(crate::mqtt::discovery::Sensor::PumpLifetimeRunningHours);
    }
    if (field.cluster, field.id) == (0x200, 0x21) {
        let options = commands.commissioning.pump_choices();
        return if options.len() > 1 {
            Feature::Select { property, options }
        } else {
            Feature::TextSensor(property)
        };
    }
    if matches!(
        (field.cluster, field.id),
        (0x301, 0x10 | 0x11) | (0x100, 0x10)
    ) {
        let limits = if field.cluster == 0x301 {
            commands
                .commissioning
                .ballast_levels
                .map(|(a, b)| (u16::from(a), u16::from(b)))
        } else {
            commands.commissioning.shade_travel.map(|n| (1, n))
        };
        return match limits {
            Some((lo, hi)) if lo < hi => Feature::Number {
                property,
                min: f64::from(lo),
                max: f64::from(hi),
                step: 1.,
                unit: None,
            },
            _ => Feature::TextSensor(property),
        };
    }
    match field.format {
        Format::Bit { names, .. } => Feature::StringBinary {
            property,
            off: names[0],
            on: names[1],
            writable: field.writable,
        },
        Format::Enum(_) if field.writable => {
            static OPTIONS: LazyLock<BTreeMap<&str, Vec<String>>> = LazyLock::new(|| {
                FIELDS
                    .iter()
                    .filter_map(|f| match f.format {
                        Format::Enum(names) if f.writable => Some((
                            f.name,
                            names
                                .iter()
                                .filter(|s| !s.is_empty())
                                .map(|s| String::from(*s))
                                .collect(),
                        )),
                        _ => None,
                    })
                    .collect()
            });
            Feature::Select {
                property,
                options: &OPTIONS[property],
            }
        }
        Format::Number {
            min,
            max,
            divisor,
            unit,
        } if field.writable => Feature::Number {
            property,
            min: f64::from(min) / f64::from(divisor),
            max: f64::from(max) / f64::from(divisor),
            step: 1. / f64::from(divisor),
            unit,
        },
        Format::Number {
            unit: Some(unit), ..
        } => Feature::Measurement {
            property,
            unit: Some(unit),
        },
        _ => Feature::TextSensor(property),
    }
}

/// A second change to the same equipment cluster waits through device readback.
pub(super) fn busy(
    pending: &BTreeMap<u8, Pending>,
    peer: Identity,
    cmd: &crate::mqtt::Command,
) -> bool {
    if cmd.operation != MqttOperation::Set {
        return false;
    }
    FIELDS
        .iter()
        .filter(|f| {
            matches!(f.cluster, 0x100 | 0x200 | 0x301) && cmd.properties.contains_key(f.name)
        })
        .any(|f| {
            pending.values().any(|p| {
                p.peer == peer
                    && p.cluster == f.cluster
                    && matches!(p.read, Some(Read::Equipment(_) | Read::Refresh(_)))
            })
        })
}

/// Reports are independent observations. Solicited replies must match the endpoint,
/// current short address, cluster, sequence and operation before affecting a request.
pub(super) async fn receive(
    link: &mut crate::ash::Ezsp<'_>,
    devices: &Bindings,
    out: &Publisher,
    pending: &mut BTreeMap<u8, Pending>,
    peer: Identity,
    source: (u16, u16),
    frame: Frame<'_>,
) -> Result<bool> {
    let (short, cluster) = source;
    if frame.header.direction != Direction::ServerToClient || frame.header.manufacturer.is_some() {
        return Ok(false);
    }
    let b = &devices.devices[&peer];
    let seq = frame.header.sequence;
    let op = pending.get(&seq).and_then(|p| match p.read {
        Some(Read::Equipment(op)) if (p.peer, p.short, p.cluster) == (peer, short, cluster) => {
            Some(op)
        }
        _ => None,
    });
    if cluster == commands::PANEL {
        if let Some(status) = zigbee_devices::ias::ace::PanelStatus::decode(
            frame,
            matches!(op, Some(Operation::Panel)),
        ) {
            submitted(
                out.state_fields(
                    &b.name,
                    "panel_status",
                    [
                        ("panel_status".into(), Json::from(status.state)),
                        ("panel_alarm_status".into(), Json::from(status.alarm)),
                        (
                            "panel_seconds_remaining".into(),
                            Json::from(status.seconds_remaining),
                        ),
                        (
                            "panel_audible_notification".into(),
                            Json::from(status.audible_notification),
                        ),
                    ]
                    .into_iter()
                    .collect(),
                ),
            )?;
            if frame.header.command_id == 5 {
                pending.remove(&seq);
            }
            return Ok(true);
        }
        return Ok(false);
    }
    if cluster == 0x0201
        && let Some(Operation::Schedule(query)) = op
        && let Some(page) = query.response(frame)
    {
        submitted(out.state(&b.name, "weekly_schedule_page", page))?;
        pending.remove(&seq);
        return Ok(true);
    }
    let payload = Payload::decode(frame);
    if let (Ok(Payload::DefaultResponse(reply)), Some(Operation::OutputCommand(_, id))) =
        (&payload, op)
    {
        if reply.command_id != id {
            return Ok(false);
        }
        if reply.status != 0 {
            pending.remove(&seq);
            return Ok(true);
        }
    }
    let next = match (&payload, op) {
        (Ok(Payload::DefaultResponse(_)), Some(Operation::OutputCommand(field, _))) => {
            Some((field, None))
        }
        (Ok(Payload::ReadAttributesResponse(rows)), Some(Operation::Constraints(setting))) => {
            Some((setting.field, Some((setting, rows))))
        }
        (
            Ok(Payload::WriteAttributesResponse(WriteStatus::Success)),
            Some(Operation::Write(field)),
        ) => Some((field, None)),
        _ => None,
    };
    if let Some((field, constraints)) = next {
        let Some((destination, route_epoch)) = pending.remove(&seq).map(|p|(p.destination,p.route_epoch)) else {return Ok(true);};
        let Some(tag) = super::ember::available_tag(link,pending,&destination,b.descriptor.profile,cluster,(crate::interview::client_endpoint(cluster),peer.1),seq.wrapping_add(1)) else {
            return Ok(true);
        };
        let (req, next) = if let Some((setting, rows)) = constraints {
            match setting.write(peer.1, tag, rows) {
                Ok(req) => (req, Operation::Write(field)),
                Err(e) => {
                    eprintln!("Equipment setting {} declined: {e}", field.name);
                    return Ok(true);
                }
            }
        } else {
            (field.request(peer.1, tag, None)?, Operation::Read(field))
        };
        // A delayed delivery callback from the previous step cannot complete the new one.
        if super::zdo::still_assigned(link, &destination).await?
            && super::zdo::submit_unicast(
                link,
                &destination,
                b.descriptor.profile,
                cluster,
                (crate::interview::client_endpoint(cluster), peer.1),
                tag,
                &req.frame,
            )
            .await?
                == 0
        {
            pending.insert(
                tag,
                Pending {
                    route_epoch,
                    destination,
                    schedule: None,
                    peer,
                    short,
                    cluster,
                    read: Some(Read::Equipment(next)),
                    until: Instant::now() + std::time::Duration::from_secs(8),
                },
            );
        }
        return Ok(true);
    }
    match (payload, op) {
        (Ok(Payload::ReportAttributes(records)), _) => {
            let mut observed = false;
            for r in records {
                for field in b
                    .equipment
                    .iter()
                    .filter(|f| (f.cluster, f.id) == (cluster, r.id))
                {
                    if let Some(value) = field.decode(r.value) {
                        submitted(out.state(&b.name, field.name, value))?;
                        observed = true;
                    }
                }
            }
            return Ok(observed);
        }
        (Ok(Payload::ReadAttributesResponse(records)), Some(Operation::Read(field))) => {
            if records.len() != 1 || records[0].id != field.id {
                return Ok(false);
            }
            if let Status::Success(v) = records[0].result {
                let Some(value) = field.decode(v) else {
                    return Ok(false);
                };
                submitted(out.state(&b.name, field.name, value))?;
            }
            pending.remove(&seq);
        }
        (
            Ok(Payload::WriteAttributesResponse(WriteStatus::Failures(failed))),
            Some(Operation::Write(field)),
        ) if failed.len() == 1 && failed[0].key == field.id => {
            pending.remove(&seq);
        }
        _ => return Ok(false),
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interview_log::{
        AttributeResult as A, Descriptor, Entry, IdentitySource, InterviewReport, Observation,
    };
    #[test]
    fn equipment_lock_reinterview_revokes_network_security_for_the_whole_member() -> Result<()> {
        const PEER: u64 = 0x1234;
        let mut bindings = Bindings::default();
        let level = |n| A::Value {
            data_type: 0x30,
            bytes: vec![n],
        };
        for (levels, allowed) in [
            (vec![], false),
            (vec![level(0)], true),
            (vec![level(1)], false),
            (vec![A::Error { status: 0x86 }], true),
            (vec![A::Omitted], false),
            (vec![level(0), A::Omitted], false),
            (
                vec![A::Value {
                    data_type: 0x20,
                    bytes: vec![0],
                }],
                false,
            ),
        ] {
            let mut facts = vec![
                Observation::Target {
                    ieee: format!("{PEER:016x}"),
                    short: 123,
                    identity_source: IdentitySource::Announcement,
                },
                Observation::ActiveEndpoints {
                    sequence: 0,
                    endpoints: vec![1, 2],
                },
            ];
            for (ep, servers) in [(1, vec![0x101]), (2, vec![0x0004, 0x20, 0x101])] {
                facts.push(Observation::SimpleDescriptor {
                    sequence: 1,
                    endpoint: ep,
                    descriptor: Descriptor {
                        profile: 0x104,
                        device: 0xfffe,
                        version: 1,
                        servers,
                        clients: vec![],
                    },
                });
            }
            for (endpoint, result) in
                std::iter::once((1, level(0))).chain(levels.into_iter().map(|l| (2, l)))
            {
                facts.push(Observation::Attribute {
                    sequence: 2,
                    endpoint,
                    cluster: 0x101,
                    attribute: 0x34,
                    result,
                });
            }
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
                &Default::default(),
            )?;
            assert_eq!(bindings.allows_network_security(PEER), allowed);
            assert_eq!(
                super::super::reporting::plans(&bindings, PEER)
                    .iter()
                    .any(|p| p.cluster == 0x101),
                allowed
            );
            assert!(!bindings.allows_network_security(PEER + 1));
        }
        Ok(())
    }
}
