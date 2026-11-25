//! Correlate a remote Groups, Scenes or Alarms operation with the device's ZCL response.
use super::{Bindings, Identity, door_lock, standard, zdo, admin::{self, sequences::{Reservation, Sequences}}};
use crate::{
    ash::{Destination, Ezsp},
    interview::Incoming,
    mqtt::admin::{EndpointCommand as Op, EndpointRequest, Reply},
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::time::Instant;
use zigbee_zcl::{
    Direction, Frame, FrameType, Header,
    cluster::{Alarms, CharacterString, Command, Counted, Groups, Scenes},
    foundation::GeneralCommand,
};

// Keep endpoint admin transactions disjoint from MQTT device commands below this
// range and reporting reconciliation at FIRST_SEQUENCE and above.
pub(super) const FIRST_SEQUENCE: u8 = 0xe0;

fn wire_command(op: &Op) -> Result<Command> {
    let group_id = |id: u16| {
        ensure!((1..=0xfff7).contains(&id), "group outside 1..0xfff7");
        Ok(id)
    };
    let scene_group = |id: u16| {
        ensure!(id <= 0xfff7, "invalid scene group");
        Ok(id)
    };
    Ok(match op {
        Op::GroupList {} => Command::Groups(Groups::GetGroupMembership {
            group_list: Counted(vec![]),
        }),
        Op::GroupView { group } => Command::Groups(Groups::ViewGroup {
            group_id: group_id(*group)?,
        }),
        Op::GroupAdd { group, name } => {
            ensure!(
                name.chars().count() <= 16,
                "group name exceeds 16 characters"
            );
            Command::Groups(Groups::AddGroup {
                group_id: group_id(*group)?,
                group_name: CharacterString::Bytes(name.as_bytes().into()),
            })
        }
        Op::GroupRemove { group } => Command::Groups(Groups::RemoveGroup {
            group_id: group_id(*group)?,
        }),
        Op::SceneList { group } => Command::Scenes(Scenes::GetSceneMembership {
            group_id: scene_group(*group)?,
        }),
        Op::SceneView { group, scene } => Command::Scenes(Scenes::ViewScene {
            group_id: scene_group(*group)?,
            scene_id: *scene,
        }),
        Op::SceneStore { group, scene } => Command::Scenes(Scenes::StoreScene {
            group_id: scene_group(*group)?,
            scene_id: *scene,
        }),
        Op::SceneRecall { group, scene } => Command::Scenes(Scenes::RecallScene {
            group_id: scene_group(*group)?,
            scene_id: *scene,
            transition_time: None,
        }),
        Op::SceneRemove { group, scene } => Command::Scenes(Scenes::RemoveScene {
            group_id: scene_group(*group)?,
            scene_id: *scene,
        }),
        Op::AlarmPop {} => Command::Alarms(Alarms::GetAlarm {}),
        Op::AlarmReset { cluster, code } => Command::Alarms(Alarms::ResetAlarm {
            alarm_code: *code,
            cluster_id: *cluster,
        }),
        Op::AlarmResetAll {} => Command::Alarms(Alarms::ResetAllAlarms {}),
        Op::AlarmClearLog {} => Command::Alarms(Alarms::ResetAlarmLog {}),
    })
}

fn string(value: CharacterString) -> Result<Value> {
    Ok(match value {
        CharacterString::Bytes(bytes) => json!(String::from_utf8(bytes)?),
        CharacterString::NonValue => Value::Null,
    })
}

fn check_status(status: u8) -> Result<()> {
    ensure!(status == 0, "device returned ZCL status {status:#04x}");
    Ok(())
}

// A response for a different command or different group/scene does not finish the request.
fn response(sent: &Command, frame: Frame<'_>) -> Option<Result<Value>> {
    let key = sent.key();
    if frame.header.direction != Direction::ServerToClient || frame.header.manufacturer.is_some() {
        return None;
    }
    if frame.header.general_command() == Some(GeneralCommand::DefaultResponse) {
        let [command, status] = *frame.payload else {
            return None;
        };
        if command != key.command {
            return None;
        }
        let accepts_default = matches!(
            sent,
            Command::Scenes(Scenes::RecallScene { .. })
                | Command::Alarms(
                    Alarms::ResetAlarm { .. }
                        | Alarms::ResetAllAlarms {}
                        | Alarms::ResetAlarmLog {}
                )
        );
        if status == 0 && !accepts_default {
            return None;
        }
        return Some(check_status(status).map(|()| json!({"accepted":true})));
    }
    let reply = Command::decode(key.cluster, frame).ok()?;
    use Command::{Alarms as A, Groups as G, Scenes as S};
    let (status, value) = match (sent,reply) {
        (G(Groups::GetGroupMembership {..}), G(Groups::GetGroupMembershipResponse {capacity,group_list})) =>
            (0,json!({"capacity":capacity,"groups":group_list.0})),
        (G(Groups::ViewGroup {group_id:g}), G(Groups::ViewGroupResponse {status,group_id,group_name})) if *g==group_id =>
            return Some(check_status(status).and_then(|()| Ok(json!({"group":group_id,"name":string(group_name)?})))),
        (G(Groups::AddGroup {group_id:g,..}), G(Groups::AddGroupResponse {status,group_id})) |
        (G(Groups::RemoveGroup {group_id:g}), G(Groups::RemoveGroupResponse {status,group_id})) if *g==group_id =>
            (status,json!({"group":group_id})),
        (S(Scenes::GetSceneMembership {group_id:g}), S(Scenes::GetSceneMembershipResponse {status,capacity,group_id,scene_list})) if *g==group_id =>
            (status,json!({"group":group_id,"capacity":capacity,"scenes":scene_list.map(|v|v.0)})),
        (S(Scenes::ViewScene {group_id:g,scene_id:s}), S(Scenes::ViewSceneResponse {
            status,group_id,scene_id,scene_name,transition_time,extension_field_sets,
        })) if (*g,*s)==(group_id,scene_id) => return Some(check_status(status).and_then(|()| {
            let extensions:Vec<_>=extension_field_sets.unwrap_or_default().into_iter()
                .map(|e| json!({"cluster":e.cluster_id,"bytes":e.value.0})).collect();
            Ok(json!({"group":group_id,"scene":scene_id,"name":scene_name.map(string).transpose()?,
                "transition_seconds":transition_time,"extensions":extensions}))
        })),
        (S(Scenes::StoreScene {group_id:g,scene_id:s}), S(Scenes::StoreSceneResponse {status,group_id,scene_id})) |
        (S(Scenes::RemoveScene {group_id:g,scene_id:s}), S(Scenes::RemoveSceneResponse {status,group_id,scene_id})) if (*g,*s)==(group_id,scene_id) =>
            (status,json!({"group":group_id,"scene":scene_id})),
        (A(Alarms::GetAlarm {}), A(Alarms::GetAlarmResponse {status,alarm_code,cluster_id,time_stamp})) =>
            (status,json!({"code":alarm_code,"cluster":cluster_id,"zigbee_time":time_stamp})),
        _ => return None,
    };
    Some(check_status(status).map(|()| value))
}

enum Sent {
    Standard(Command),
    Lock(door_lock::Operation),
}
impl Sent {
    fn cluster(&self) -> u16 {
        match self {
            Self::Standard(cmd) => cmd.key().cluster,
            Self::Lock(_) => zigbee_devices::equipment::DOOR_LOCK,
        }
    }
}
pub(super) fn advance_sequence(seq: u8, offset: u8) -> u8 {
    FIRST_SEQUENCE
        + (seq - FIRST_SEQUENCE + offset)
            % (crate::interview::reporting::FIRST_SEQUENCE - FIRST_SEQUENCE)
}

struct Pending {
    peer: Identity,
    destination: Destination,
    sent: Sent,
    seq: u8,
    transaction: Reservation,
    until: Instant,
    reply: Reply,
}

impl Pending {
    fn interrupt(self, reason: &str) -> Result<()> {
        if matches!(&self.sent, Sent::Lock(lock) if !lock.command_accepted()) {
            self.reply.error(&format!("{reason} before lock command submission"))?;
        } else {
            self.reply.indeterminate(&format!("{reason}; operation outcome unknown"))?;
        }
        Ok(())
    }
}

#[derive(Default)]
pub(super) struct Operations {
    pending: Option<Pending>,
    ids: Sequences,
    pub(super) lock_credentials: Option<door_lock::Credentials>,
}

impl Operations {
    pub(super) fn deadline(&self) -> Option<Instant> {
        self.pending.as_ref().map(|p| p.until)
    }
    pub(super) fn outstanding(&self, ieee: u64) -> bool {
        self.pending.as_ref().is_some_and(|p| p.peer.0 == ieee)
    }
    pub(super) fn expire(&mut self) -> Result<()> {
        if self.deadline().is_some_and(|end| end <= Instant::now()) {
            self.pending
                .take()
                .unwrap()
                .interrupt("Device response timed out")?;
        }
        Ok(())
    }
    pub(super) fn retire_if(&mut self, affected: impl Fn(&Destination) -> bool) -> Result<()> {
        if self
            .pending
            .as_ref()
            .is_some_and(|p| affected(&p.destination))
        {
            self.pending
                .take()
                .unwrap()
                .interrupt("Device address invalidated")?;
        }
        Ok(())
    }
    pub(super) async fn start(
        &mut self,
        devices: &Bindings,
        link: &mut Ezsp<'_>,
        req: EndpointRequest,
        reply: Reply,
    ) -> Result<()> {
        let result = async {
            if self.pending.is_some() {
                bail!("another endpoint administration request is pending");
            }
            let peer = (devices.peer(&req.id)?, req.endpoint);
            let d = devices.peers[&peer.0]
                .1
                .get(&peer.1)
                .context("unknown endpoint")?;
            let sent = wire_command(&req.command)?;
            let key = sent.key();
            ensure!(
                standard::application(peer.1, d) && d.servers.contains(&key.cluster),
                "endpoint did not advertise this application server"
            );
            if matches!(key.cluster, 4 | 5) {
                ensure!(devices.allows_network_security(peer.0),
                    "lock member requires verified network-security support for group and scene operations");
            }
            let short = zdo::node(link, peer.0).await?;
            let destination = link.destination(peer.0, short)?;
            let payload = sent.encode()?;
            let transaction = self.ids.reserve((short,peer.1,key.cluster), FIRST_SEQUENCE..crate::interview::reporting::FIRST_SEQUENCE, 1)?;
            let seq = transaction.first;
            let mut bytes = Vec::new();
            Frame {
                header: Header {
                    frame_type: FrameType::ClusterSpecific,
                    direction: key.direction,
                    disable_default_response: false,
                    manufacturer: None,
                    sequence: seq,
                    command_id: key.command,
                },
                payload: &payload,
            }
            .encode_into(&mut bytes);
            let send = zdo::unicast(
                link,
                &destination,
                d.profile,
                key.cluster,
                (crate::interview::client_endpoint(key.cluster), peer.1),
                0xff,
                &bytes,
            )
            .await;
            if let Err(e) = send {
                if !admin::submission_uncertain(&e) {self.ids.unsent(transaction);}
                return Err(e);
            }
            Ok::<_, anyhow::Error>((peer, destination, sent, transaction))
        }
        .await;
        match result {
            Ok((peer, destination, sent, transaction)) => {
                self.pending = Some(Pending {
                    peer,
                    destination,
                    sent: Sent::Standard(sent),
                    seq: transaction.first,
                    transaction,
                    reply,
                    until: Instant::now() + Duration::from_secs(12),
                })
            }
            Err(err) => admin::finish_send_error(reply, &err)?,
        }
        Ok(())
    }
    pub(super) async fn start_lock(
        &mut self,
        devices: &Bindings,
        link: &mut Ezsp<'_>,
        req: crate::mqtt::admin::DoorLockRequest,
        reply: Reply,
    ) -> Result<()> {
        let attempt = async {
            ensure!(
                self.pending.is_none(),
                "another endpoint administration request is pending"
            );
            let credentials = self
                .lock_credentials
                .as_ref()
                .context("lock controls are disabled")?;
            let peer = (devices.peer(&req.id)?, req.endpoint);
            let d = devices.peers[&peer.0]
                .1
                .get(&peer.1)
                .context("unknown endpoint")?;
            ensure!(
                standard::application(peer.1, d)
                    && d.servers.contains(&zigbee_devices::equipment::DOOR_LOCK)
                    && devices.allows_network_security(peer.0),
                "lock endpoint lacks a verified network-security interview"
            );
            credentials.check_endpoint(peer)?;
            let short = zdo::node(link, peer.0).await?;
            let destination = link.destination(peer.0, short)?;
            let transaction=self.ids.reserve((short,peer.1,zigbee_devices::equipment::DOOR_LOCK),FIRST_SEQUENCE..crate::interview::reporting::FIRST_SEQUENCE,3)?;
            let op=match door_lock::Operation::start(link, peer, &destination, d.profile, transaction.first, req.action).await {
                Ok(op)=>op,
                Err(e)=>{
                    if !admin::submission_uncertain(&e) {self.ids.unsent(transaction);}
                    return Err(e);
                }
            };
            Ok::<_, anyhow::Error>((peer, destination, transaction, op))
        }
        .await;
        match attempt {
            Ok((peer, destination, transaction, op)) => {
                self.pending = Some(Pending {
                    peer,
                    destination,
                    seq: transaction.first,
                    transaction,
                    sent: Sent::Lock(op),
                    reply,
                    until: Instant::now() + Duration::from_secs(12),
                })
            }
            // Only the security-policy read could have been submitted at this point.
            Err(e) => reply.error(&format!("Lock command was not submitted: {e:#}"))?,
        }
        Ok(())
    }

    pub(super) async fn received(
        &mut self,
        link: &mut Ezsp<'_>,
        devices: &Bindings,
        out: &crate::mqtt::Publisher,
        peer: Identity,
        msg: &Incoming<'_>,
        frame: Frame<'_>,
    ) -> Result<bool> {
        let Some(p) = &mut self.pending else {
            return Ok(false);
        };
        if !link.current(&p.destination) {
            self.pending.take().unwrap().interrupt("Device address invalidated")?;
            return Ok(false);
        }
        if (p.peer, p.destination.short(), p.sent.cluster(), p.seq)
            != (peer, msg.sender, msg.aps.cluster, frame.header.sequence)
            || msg.aps.dst != crate::interview::client_endpoint(msg.aps.cluster)
        {
            return Ok(false);
        }
        let result = match &mut p.sent {
            Sent::Standard(cmd) => {
                let Some(result) = response(cmd, frame) else {
                    return Ok(false);
                };
                result
            }
            Sent::Lock(lock) => match lock
                .advance(
                    link,
                    (
                        self.lock_credentials
                            .as_ref()
                            .expect("configured lock credential directory"),
                        devices,
                        out,
                    ),
                    &p.destination,
                    peer,
                    msg,
                    frame,
                )
                .await
            {
                Ok(door_lock::Step::Ignore) => return Ok(false),
                Ok(door_lock::Step::Declined(status)) => {
                    let pending = self.pending.take().unwrap();
                    self.ids.confirmed(pending.transaction);
                    pending.reply.error(&format!("Lock declined command ({status:#04x})"))?;
                    return Ok(true);
                }
                Ok(door_lock::Step::Sent) => {
                    p.seq = advance_sequence(p.seq, 1);
                    p.until = Instant::now() + Duration::from_secs(12);
                    return Ok(true);
                }
                Ok(door_lock::Step::Complete(result)) => Ok(result),
                Err(e) => Err(e),
            },
        };
        let pending = self.pending.take().unwrap();
        match result {
            Ok(value) => {
                self.ids.confirmed(pending.transaction);
                pending.reply.ok(value)?;
            }
            Err(err) if admin::submission_uncertain(&err) ||
                matches!(&pending.sent, Sent::Lock(lock) if lock.command_accepted()) => {
                pending.reply.indeterminate(&err.to_string())?;
            }
            Err(err) => {
                self.ids.confirmed(pending.transaction);
                pending.reply.error(&err.to_string())?;
            }
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scene_store_requires_its_own_group_scene_and_status() -> Result<()> {
        let store = wire_command(&Op::SceneStore { group: 3, scene: 8 })?;
        assert_eq!(store.encode()?, [3, 0, 8]);
        for wrong in [
            &[0x19, 2, 4, 0, 4, 0, 8][..],
            &[0x19, 2, 4, 0, 3, 0, 9],
            &[0x19, 2, 2, 0, 3, 0, 8],
            &[0x11, 2, 4, 0, 3, 0, 8],
            &[0x18, 2, 0x0b, 4, 0],
        ] {
            assert!(response(&store, Frame::decode(wrong)?).is_none());
        }
        assert_eq!(
            response(&store, Frame::decode(&[0x19, 2, 4, 0, 3, 0, 8])?).unwrap()?,
            json!({"group":3,"scene":8})
        );
        assert!(
            response(&store, Frame::decode(&[0x18, 2, 0x0b, 4, 0x81])?)
                .unwrap()
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn default_confirmation_and_membership_payloads() -> Result<()> {
        let recall = wire_command(&Op::SceneRecall { group: 0, scene: 1 })?;
        assert_eq!(
            response(&recall, Frame::decode(&[0x18, 9, 0x0b, 5, 0])?).unwrap()?,
            json!({"accepted":true})
        );
        let list = wire_command(&Op::GroupList {})?;
        assert_eq!(
            response(
                &list,
                Frame::decode(&[0x19, 9, 2, 10, 2, 5, 0, 0x34, 0x12])?
            )
            .unwrap()?,
            json!({"capacity":10,"groups":[5,0x1234]})
        );
        let pop = wire_command(&Op::AlarmPop {})?;
        assert_eq!(
            response(&pop, Frame::decode(&[0x19, 9, 1, 0, 7, 1, 0, 2, 0, 0, 0])?).unwrap()?,
            json!({"code":7,"cluster":1,"zigbee_time":2})
        );
        assert!(wire_command(&Op::GroupRemove { group: 0 }).is_err());
        Ok(())
    }
}
