//! A schedule job owns its remaining days inside its outstanding transaction.
//! A leave, delivery failure, timeout or shutdown releases all unsent work.
use super::{
    Bindings, Identity,
    ember::{Pending, free_application_sequence, submitted},
    zdo,
};
use crate::{
    ash::Ezsp,
    mqtt::{ReceivedCommand, Operation, Publisher},
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value as Json, json};
use std::collections::{BTreeMap, VecDeque};
use tokio::time::{Duration, Instant};
use zigbee_devices::equipment::schedule::{self, Day};
use zigbee_zcl::{
    Direction, Frame,
    cluster::{ScheduleTransition, WeeklySchedule},
    foundation::{GeneralCommand, Payload},
};

#[derive(PartialEq)]
enum Phase {
    Write,
    Read,
}
pub(super) struct Job {
    route_epoch: Option<u64>,
    name: String,
    profile: u16,
    days: VecDeque<Day>,
    phase: Phase,
    completed: Vec<Json>,
    seen: Vec<ScheduleTransition>,
    daily: u8,
    weekly: u8,
    confirmed_count: usize,
    write: bool,
}

pub(super) fn prepare(db: &Bindings, cmd: &ReceivedCommand) -> Result<Option<(Identity, Job)>> {
    if !cmd.properties.contains_key("weekly_schedule") {
        return Ok(None);
    }
    ensure!(
        cmd.properties.len() == 1,
        "send a weekly schedule operation on its own"
    );
    let id = *db
        .topics
        .get(&cmd.device)
        .context("unknown schedule endpoint")?;
    let binding = &db.devices[&id];
    let write = cmd.operation == Operation::Set;
    let caps = binding.equipment_commands.weekly;
    // R8 provides no final-page flag for a Get Weekly Schedule response. An
    // edit must be verifiable even if the device kept extra transitions.
    ensure!(
        !write || usize::from(caps.daily.min(caps.weekly)) <= WeeklySchedule::MAX_TRANSITIONS,
        "editing a potentially multipart day cannot be verified"
    );
    let days = schedule::pages(&cmd.properties["weekly_schedule"], write, caps)?.into();
    Ok(Some((
        id,
        Job {
            route_epoch: cmd.epoch(),
            name: binding.name.clone(),
            profile: binding.descriptor.profile,
            days,
            phase: if write { Phase::Write } else { Phase::Read },
            completed: vec![],
            seen: vec![],
            daily: caps.daily.min(caps.weekly),
            weekly: caps.weekly,
            confirmed_count: 0,
            write,
        },
    )))
}

impl Job {
    fn multipart(&self) -> bool {
        usize::from(self.daily) > WeeklySchedule::MAX_TRANSITIONS
    }
    fn summary(&self) -> Json {
        let mut v = self.current().json(&[]);
        v.as_object_mut().unwrap().remove("transitions");
        v["count"] = json!(self.seen.len());
        v
    }
    fn partial(&self) -> bool {
        self.multipart()
            && !self.seen.is_empty()
            && self.seen.len() < usize::from(self.daily)
            && self.confirmed_count + self.seen.len() < usize::from(self.weekly)
    }
    fn append(&mut self, rows: &[ScheduleTransition]) -> Result<()> {
        let count = self.seen.len() + rows.len();
        ensure!(
            count <= usize::from(self.daily)
                && self.confirmed_count + count <= usize::from(self.weekly),
            "response exceeds thermostat schedule capacity"
        );
        let mut prev = self.seen.last().map(|v| v.minutes_since_midnight);
        for row in rows {
            ensure!(
                row.minutes_since_midnight < 1440
                    && prev.is_none_or(|n| n < row.minutes_since_midnight)
                    && row
                        .heat_setpoint
                        .into_iter()
                        .chain(row.cool_setpoint)
                        .all(|v| v >= -27315),
                "invalid or overlapping schedule response"
            );
            prev = Some(row.minutes_since_midnight);
        }
        self.seen.extend_from_slice(rows);
        Ok(())
    }
    fn current(&self) -> &Day {
        self.days.front().expect("active schedule day")
    }
    fn result(&self, status: &str, reason: &str, out: &Publisher) -> Result<()> {
        if !out.route_is_current(&self.name, self.route_epoch) {return Ok(());}
        let mut value = json!({"status":status,"completed":self.completed,"operation":if self.write{"set"}else{"get"}});
        if !reason.is_empty() {
            value["reason"] = reason.into();
        }
        if let Some(day) = self.days.front() {
            value["current"] = if self.multipart() {
                self.summary()
            } else {
                day.json(&self.seen)
            };
        }
        submitted(out.state(&self.name, "weekly_schedule", value))
    }
    async fn submit(
        self,
        link: &mut Ezsp<'_>,
        out: &Publisher,
        pending: &mut BTreeMap<u8, Pending>,
        peer: Identity,
        destination: crate::ash::Destination,
        tag: u8,
    ) -> Result<()> {
        if !link.current(&destination) {
            return self.result("interrupted", "membership changed", out);
        }
        let short = destination.short();
        let endpoints = (crate::interview::client_endpoint(0x201), peer.1);
        let Some(tag) = super::ember::available_tag(link,pending,&destination,self.profile,0x201,endpoints,tag) else {
            return self.result("interrupted", "no unambiguous transaction available", out);
        };
        let req = self
            .current()
            .request(peer.1, tag, self.phase == Phase::Write)?;
        let status = zdo::submit_unicast(
            link,
            &destination,
            self.profile,
            0x201,
            endpoints,
            tag,
            &req.frame,
        )
        .await;
        if let Err(e) = &status && crate::ash::locally_refused(e) {
            return self.result("interrupted", "destination or transaction retired", out);
        }
        if status? != 0 {
            return self.result("interrupted", "adapter declined transmission", out);
        }
        pending.insert(
            tag,
            Pending {
                route_epoch: self.route_epoch,
                destination,
                peer,
                short,
                cluster: 0x201,
                read: None,
                schedule: Some(Box::new(self)),
                until: Instant::now() + Duration::from_secs(8),
            },
        );
        Ok(())
    }
}

pub(super) async fn start(
    link: &mut Ezsp<'_>,
    out: &Publisher,
    pending: &mut BTreeMap<u8, Pending>,
    peer: Identity,
    job: Job,
    destination: crate::ash::Destination,
    cursor: &mut u8,
) -> Result<()> {
    if pending
        .values()
        .any(|p| p.peer == peer && p.schedule.is_some())
    {
        // Keep the current operation's result intact; the rejected command never ran.
        return submitted(out.event(
            &job.name,
            "weekly_schedule_rejected",
            "another schedule operation is in progress".into(),
        ));
    }
    if !super::ember::application_room(pending,1) {
        return job.result("rejected", "no transaction available", out);
    }
    let Some(tag) = free_application_sequence(*cursor, pending) else {
        return job.result("rejected", "no transaction available", out);
    };
    *cursor = tag.wrapping_add(1) % super::endpoint_admin::FIRST_SEQUENCE;
    job.submit(link, out, pending, peer, destination, tag).await
}

pub(super) fn abort(
    pending: &mut BTreeMap<u8, Pending>,
    out: &Publisher,
    mut matches: impl FnMut(u8, &Pending) -> bool,
    reason: &str,
) -> Result<()> {
    let tags: Vec<_> = pending
        .iter()
        .filter(|(tag, p)| p.schedule.is_some() && matches(**tag, p))
        .map(|(tag, _)| *tag)
        .collect();
    for tag in tags {
        let job = pending.remove(&tag).unwrap().schedule.unwrap();
        job.result(
            if job.partial() {
                "partial"
            } else {
                "interrupted"
            },
            reason,
            out,
        )?;
    }
    Ok(())
}

pub(super) async fn receive(
    link: &mut Ezsp<'_>,
    out: &Publisher,
    pending: &mut BTreeMap<u8, Pending>,
    peer: Identity,
    short: u16,
    frame: Frame<'_>,
) -> Result<bool> {
    let tag = frame.header.sequence;
    let Some(p) = pending
        .get(&tag)
        .filter(|p| p.peer == peer && p.short == short && p.cluster == 0x201)
    else {
        return Ok(false);
    };
    let Some(job) = p.schedule.as_ref() else {
        return Ok(false);
    };
    if frame.header.direction != Direction::ServerToClient || frame.header.manufacturer.is_some() {
        return Ok(false);
    }
    let rows = if frame.header.general_command() == Some(GeneralCommand::DefaultResponse) {
        let Ok(Payload::DefaultResponse(r)) = Payload::decode(frame) else {
            return Ok(false);
        };
        let expected = if job.phase == Phase::Write { 1 } else { 2 };
        if r.command_id != expected {
            return Ok(false);
        }
        if r.status != 0 {
            let job = pending.remove(&tag).unwrap().schedule.unwrap();
            job.result("declined", &format!("ZCL status 0x{:02x}", r.status), out)?;
            return Ok(true);
        }
        if job.phase == Phase::Read {
            return Ok(false);
        }
        None
    } else {
        if job.phase != Phase::Read {
            return Ok(false);
        }
        let Some(rows) = job.current().observe(frame) else {
            return Ok(false);
        };
        Some(rows)
    };
    let mut slot = pending.remove(&tag).unwrap();
    let mut job = slot.schedule.take().unwrap();
    if let Some(rows) = rows {
        let offset = job.seen.len();
        if let Err(e) = job.append(&rows) {
            job.result("interrupted", &e.to_string(), out)?;
            return Ok(true);
        }
        if job.multipart() {
            let mut chunk = job.current().json(&rows);
            chunk["offset"] = json!(offset);
            submitted(out.event(&job.name, "weekly_schedule_chunk", chunk))?;
        }
        if job.partial() {
            // The server may send further frames with this transaction sequence.
            // On timeout expose the collected rows as partial without advancing.
            if !rows.is_empty() {
                slot.until = Instant::now() + Duration::from_secs(8);
            }
            slot.schedule = Some(job);
            pending.insert(tag, slot);
            return Ok(true);
        }
        if job
            .current()
            .edits
            .as_ref()
            .is_some_and(|expected| expected != &job.seen)
        {
            job.result(
                "mismatch",
                "thermostat readback differed from the requested day",
                out,
            )?;
            return Ok(true);
        }
        let report = if job.multipart() {
            job.summary()
        } else {
            job.current().json(&job.seen)
        };
        job.completed.push(report);
        job.confirmed_count += job.seen.len();
        job.seen.clear();
        job.days.pop_front();
        if job.days.is_empty() {
            job.result(if job.write { "verified" } else { "read" }, "", out)?;
            return Ok(true);
        }
        job.phase = if job.write { Phase::Write } else { Phase::Read };
    } else {
        job.phase = Phase::Read;
    }
    let Some(next) = free_application_sequence(tag, pending) else {
        job.result("interrupted", "no fresh transaction available", out)?;
        return Ok(true);
    };
    if !zdo::still_assigned(link, &slot.destination).await? {
        job.result("interrupted", "membership changed", out)?;
        return Ok(true);
    }
    job.submit(link, out, pending, peer, slot.destination, next)
        .await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zigbee_zcl::cluster::ScheduleMode;
    #[test]
    fn multipart_reads_keep_order_and_stay_partial_until_provably_complete() -> Result<()> {
        let mut job = Job {
            route_epoch: None,
            name: "thermostat".into(),
            profile: 0x104,
            days: [Day {
                days: 2,
                mode: ScheduleMode::HEAT,
                edits: None,
            }]
            .into(),
            phase: Phase::Read,
            completed: vec![],
            seen: vec![],
            daily: 12,
            weekly: 20,
            confirmed_count: 0,
            write: false,
        };
        let rows = (0..12)
            .map(|m| ScheduleTransition {
                minutes_since_midnight: m,
                heat_setpoint: Some(2000),
                cool_setpoint: None,
            })
            .collect::<Vec<_>>();
        job.append(&rows[..10])?;
        assert!(job.partial());
        assert!(job.append(&rows[9..11]).is_err());
        job.append(&rows[10..11])?;
        assert!(job.partial());
        assert_eq!(
            job.summary(),
            json!({"day":"monday","mode":"heat","count":11})
        );
        job.append(&rows[11..])?;
        assert!(!job.partial());
        assert!(job.append(&rows[0..1]).is_err());
        Ok(())
    }
}
