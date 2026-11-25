//! Reconcile reporting on standard clusters advertised by a verified member.
use super::*;
use zigbee_zcl::{
    attributes::WriteStatus,
    reporting::{
        Configuration, ReportingDirection, ReportingKey, ReportingRecord, ReportingResult,
    },
    value::Value,
};

/// Keep coordinator reporting transactions separate from concurrent application and manual bind requests.
pub(crate) const FIRST_SEQUENCE: u8 = 0xf0;

#[derive(Clone, Copy)]
pub(crate) struct Attribute {
    pub id: u16,
    pub ty: u8,
}
impl Attribute {
    fn desired(self, cluster:u16) -> Result<ReportingRecord<'static>> {
        let ty = self.ty.try_into()?;
        const ONE_PPM: [u8; 4] = 1e-6_f32.to_le_bytes();
        let change = match self.ty {
            0x20 => Some(Value::from_bytes(ty, &[1])?),
            0x29 => Some(Value::from_bytes(ty, &[10, 0])?),
            0x21 => Some(Value::from_bytes(ty, &[1, 0])?),
            // Basic I/O floats carry application-defined units. Zero means any change.
            0x39 if zigbee_devices::basic_io::supported(cluster) => Some(Value::from_bytes(ty, &[0;4])?),
            // One part per million of the on-wire concentration fraction.
            0x39 => Some(Value::from_bytes(ty, &ONE_PPM)?),
            0x10 | 0x18 | 0x19 | 0x30 => None,
            _ => bail!("unsupported reporting type"),
        };
        Ok(ReportingRecord {
            id: self.id,
            configuration: Configuration::Reported {
                data_type: ty,
                minimum: if matches!(self.ty,0x10|0x18) { 0 } else { 5 },
                maximum: 3600,
                change,
            },
        })
    }
}
pub(crate) struct Endpoint {
    pub endpoint: u8,
    pub profile: u16,
    pub cluster: u16,
    pub attributes: Vec<Attribute>,
    pub discover_cover: bool,
}

pub(crate) async fn reconcile(
    link: &mut impl InterviewIo,
    node: u16,
    ieee: [u8; 8],
    coordinator: u64,
    plans: &[Endpoint],
    record: &mut Recorder<'_>,
) -> Result<()> {
    let mut t = Target {
        record,
        node,
        ieee,
        seq: FIRST_SEQUENCE,
    };
    for p in plans {
        if p.discover_cover {
            t.read(link, p.profile, p.endpoint, p.cluster, &[0, 7])
                .await?;
            t.cover_commands(link, p.profile, p.endpoint).await?;
        }
        if p.attributes.is_empty() {
            continue;
        }
        let mut body = vec![0];
        body.extend(ieee);
        body.push(p.endpoint);
        body.extend(p.cluster.to_le_bytes());
        body.push(3);
        body.extend(coordinator.to_le_bytes());
        body.push(client_endpoint(p.cluster));
        let bind = t
            .exchange(
                link,
                Aps {
                    profile: 0,
                    cluster: 0x21,
                    src: 0,
                    dst: 0,
                },
                0x8021,
                body,
            )
            .await?;
        match bind.as_deref() {
            Some([_, 0]) => {}
            None => break, // Sleepy device. Retry on its next authenticated rejoin.
            _ => continue,
        }
        let keys: Vec<_> = p
            .attributes
            .iter()
            .map(|a| ReportingKey {
                direction: ReportingDirection::Reported,
                id: a.id,
            })
            .collect();
        let read = || Payload::ReadReportingConfiguration(keys.clone());
        let Some(bytes) = t
            .foundation(link, p.profile, p.endpoint, p.cluster, read())
            .await?
        else {
            break;
        };
        let Ok(Payload::ReadReportingConfigurationResponse(rows)) =
            Payload::decode(Frame::decode(&bytes)?)
        else {
            continue;
        };
        let mut missing = Vec::new();
        for &a in &p.attributes {
            let desired = a.desired(p.cluster)?;
            // Ambiguous, omitted and unsupported rows provide no basis for a change.
            let mut matching = rows.iter().filter(|row| match row {
                ReportingResult::Success(r) => {
                    r.id == a.id && matches!(r.configuration, Configuration::Reported { .. })
                }
                ReportingResult::Error(e) => {
                    e.key.id == a.id && e.key.direction == ReportingDirection::Reported
                }
            });
            let first = matching.next();
            if matching.next().is_some() {
                continue;
            }
            match first {
                Some(ReportingResult::Success(r)) => {
                    if let Configuration::Reported { data_type, minimum, maximum, .. } = &r.configuration
                        && data_type.id() == a.ty
                    {
                        if *maximum == u16::MAX {
                            missing.push(desired);
                        } else if *minimum != u16::MAX && (*maximum == 0 || minimum <= maximum) {
                            // A device may clamp intervals or already have a working policy.
                            confirmed(&mut t,p,r)?;
                        }
                    }
                }
                Some(ReportingResult::Error(e)) if e.status.get() == 0x8b => missing.push(desired),
                _ => {}
            }
        }
        if missing.is_empty() {
            continue;
        }
        let ids: Vec<_> = missing.iter().map(|r| r.id).collect();
        let Some(bytes) = t
            .foundation(
                link,
                p.profile,
                p.endpoint,
                p.cluster,
                Payload::ConfigureReporting(missing),
            )
            .await?
        else {
            break;
        };
        let Ok(Payload::ConfigureReportingResponse(accepted)) =
            Payload::decode(Frame::decode(&bytes)?)
        else {
            continue;
        };
        let accepted: Vec<_> = ids
            .into_iter()
            .filter(|id| match &accepted {
                WriteStatus::Success => true,
                WriteStatus::Failures(f) => !f
                    .iter()
                    .any(|e| e.key.direction == ReportingDirection::Reported && e.key.id == *id),
            })
            .collect();
        if accepted.is_empty() {
            continue;
        }
        let keys = accepted
            .iter()
            .map(|id| ReportingKey {
                direction: ReportingDirection::Reported,
                id: *id,
            })
            .collect();
        let Some(bytes) = t
            .foundation(
                link,
                p.profile,
                p.endpoint,
                p.cluster,
                Payload::ReadReportingConfiguration(keys),
            )
            .await?
        else {
            break;
        };
        if let Ok(Payload::ReadReportingConfigurationResponse(rows)) =
            Payload::decode(Frame::decode(&bytes)?)
        {
            for id in accepted {
                let wanted = p
                    .attributes
                    .iter()
                    .find(|a| a.id == id)
                    .expect("planned attribute");
                let matching = rows
                    .iter()
                    .filter(|row| match row {
                        ReportingResult::Success(r) => r.id == id,
                        ReportingResult::Error(e) => e.key.id == id,
                    })
                    .collect::<Vec<_>>();
                if let [ReportingResult::Success(r)] = matching.as_slice()
                    && matches!(&r.configuration,Configuration::Reported{data_type,..} if data_type.id()==wanted.ty)
                {
                    confirmed(&mut t, p, r)?;
                }
            }
        }
    }
    Ok(())
}

fn confirmed(t: &mut Target<'_>, ep: &Endpoint, r: &ReportingRecord<'_>) -> Result<()> {
    if let Configuration::Reported {
        data_type,
        minimum,
        maximum,
        change,
    } = &r.configuration
    {
        (t.record)(Observation::ReportingReadback {
            endpoint: ep.endpoint,
            cluster: ep.cluster,
            attribute: r.id,
            data_type: data_type.id(),
            minimum: *minimum,
            maximum: *maximum,
            change: change.map(|v| v.as_bytes().to_vec()),
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    #[derive(Default)]
    struct Thermostat {
        replies: VecDeque<Callback>,
        configured: bool,
        asleep: bool,
        writes: usize,
        binds: usize,
    }
    impl Thermostat {
        fn reply(&mut self, profile: u16, cluster: u16, endpoints: (u8, u8), body: &[u8]) {
            let mut payload = vec![0];
            payload.extend(profile.to_le_bytes());
            payload.extend(cluster.to_le_bytes());
            payload.extend([
                endpoints.0,
                endpoints.1,
                0,
                0,
                0,
                0,
                0,
                190,
                0,
                0x34,
                0x12,
                0,
                0,
                body.len() as u8,
            ]);
            payload.extend(body);
            self.replies.push_back(Callback { id: 0x45, payload });
        }
    }
    impl InterviewIo for Thermostat {
        async fn send(&mut self, args: &[u8]) -> Result<Vec<u8>> {
            let seq = args[14];
            match u16::from_le_bytes(args[5..7].try_into()?) {
                0x21 => {
                    assert_eq!(
                        &args[16..],
                        &[&[seq][..], &[17; 8], &[7, 1, 2, 3], &[34; 8], &[1]].concat()
                    );
                    self.binds += 1;
                    self.reply(0, 0x8021, (0, 0), &[seq, 0]);
                }
                0x0201 => {
                    let f = Frame::decode(&args[16..])?;
                    match f.header.general_command().unwrap() {
                        GeneralCommand::ReadReportingConfiguration => {
                            assert_eq!(f.payload, &[0, 0, 0]);
                            // Even a genuine attribute report with the matching TSN cannot confirm intervals.
                            self.reply(0x104, 0x201, (7, 1), &[8, seq, 10, 0, 0, 0x29, 0xd0, 7]);
                            if !self.asleep {
                                let mut reply = vec![8, seq, 9];
                                if self.configured {
                                    reply.extend([0, 0, 0, 0, 0x29, 8, 0, 0x10, 0x0e, 10, 0]);
                                } else {
                                    reply.extend([0x8b, 0, 0, 0]);
                                }
                                self.reply(0x104, 0x201, (7, 1), &reply);
                            }
                        }
                        GeneralCommand::ConfigureReporting => {
                            assert_eq!(f.payload, &[0, 0, 0, 0x29, 5, 0, 0x10, 0x0e, 10, 0]);
                            self.configured = true;
                            self.writes += 1;
                            self.reply(0x104, 0x201, (7, 1), &[8, seq, 7, 0]);
                        }
                        c => panic!("unexpected request {c:?}"),
                    }
                }
                c => panic!("unexpected cluster {c:?}"),
            }
            Ok(vec![0, 1])
        }
        async fn receive(&mut self, _: Instant) -> Result<Option<Callback>> {
            Ok(self.replies.pop_front())
        }
    }
    #[tokio::test]
    async fn sleeping_thermostat_reconciles_after_rejoin_and_existing_intervals_survive_restart()
    -> Result<()> {
        let mut device = Thermostat {
            asleep: true,
            ..Default::default()
        };
        let plan = [Endpoint {
            endpoint: 7,
            profile: 0x104,
            cluster: 0x201,
            attributes: vec![Attribute { id: 0, ty: 0x29 }],
            discover_cover: false,
        }];
        for (asleep, expected, expected_writes) in [(true, 0, 0), (false, 1, 1), (false, 1, 1)] {
            device.asleep = asleep;
            let mut confirmed = 0;
            reconcile(
                &mut device,
                0x1234,
                [17; 8],
                u64::from_le_bytes([34; 8]),
                &plan,
                &mut |e| {
                    if let Observation::ReportingReadback {
                        data_type,
                        minimum,
                        maximum,
                        change,
                        ..
                    } = e
                    {
                        assert_eq!(
                            (data_type, minimum, maximum, change),
                            (0x29, 8, 3600, Some(vec![10, 0]))
                        );
                        confirmed += 1;
                    }
                    Ok(())
                },
            )
            .await?;
            assert_eq!((confirmed, device.writes), (expected, expected_writes));
        }
        assert_eq!(device.binds, 3);
        Ok(())
    }
}
