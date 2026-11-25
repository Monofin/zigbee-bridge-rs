//! R8 §7.3.2. Lock/Unlock responses acknowledge receipt; LockState is separate.
use super::{Bindings, Identity, ember::submitted, zdo};
use crate::{
    ash::{Destination, Ezsp},
    interview::{Incoming, client_endpoint},
    mqtt::{Publisher, admin::LockAction},
};
use anyhow::{Context, Result, bail, ensure};
use rustix::fs::{Mode, OFlags, openat};
use serde_json::{Value, json};
use std::{
    fs::File,
    io::Read,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::Path,
};
use zigbee_devices::equipment::{DOOR_LOCK, SECURITY_LEVEL};
use zigbee_zcl::{
    Direction, Frame, FrameType,
    attributes::Status,
    foundation::{GeneralCommand, Payload},
};

pub(super) struct Credentials(File);

impl Credentials {
    pub(super) fn open(path: &Path, members: &Path) -> Result<Self> {
        let state = members
            .parent()
            .context("missing state directory")?
            .canonicalize()?;
        ensure!(
            !path.canonicalize()?.starts_with(state),
            "keep lock credentials outside coordinator backups"
        );
        let dir = File::options()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
            .open(path)?;
        ensure!(
            dir.metadata()?.permissions().mode() & 0o077 == 0,
            "lock credential directory must be owner-only"
        );
        Ok(Self(dir))
    }

    // Resolve only an exact IEEE/endpoint beneath the opened directory. A new open for
    // each operation permits atomic credential rotation or revocation without restart.
    fn open_pin(&self, peer: Identity) -> Result<File> {
        let filename = format!("{:016x}-{}.pin", peer.0, peer.1);
        let fd = openat(
            &self.0,
            filename,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::NOCTTY,
            Mode::empty(),
        )
        .context("no usable credential for this lock endpoint")?;
        let file = File::from(fd);
        let meta = file.metadata()?;
        ensure!(
            meta.is_file() && meta.permissions().mode() & 0o077 == 0,
            "expected an owner-only regular PIN file"
        );
        Ok(file)
    }

    pub(super) fn check_endpoint(&self, peer: Identity) -> Result<()> {
        self.open_pin(peer).map(drop)
    }

    pub(super) fn discover(&self, devices: &mut Bindings, ieee: u64) {
        devices.lock_controls.retain(|id| id.0 != ieee);
        if devices.allows_network_security(ieee) {
            for (&id, b) in devices.devices.range((ieee, 0)..=(ieee, u8::MAX)) {
                if b.descriptor.servers.contains(&DOOR_LOCK) && self.open_pin(id).is_ok() {
                    devices.lock_controls.insert(id);
                }
            }
        }
    }

    fn command(&self, peer: Identity, action: LockAction, seq: u8) -> Result<Vec<u8>> {
        let file = self.open_pin(peer)?;
        // One octet for length. Keep well within the adapter's single-frame limit.
        const MAX_PIN: usize = 32;
        let mut frame = Vec::with_capacity(4 + MAX_PIN + 3);
        frame.extend([0x11, seq, command_id(action), 0]);
        file.take((MAX_PIN + 3) as u64).read_to_end(&mut frame)?;
        if frame.last() == Some(&b'\n') {
            frame.pop();
            if frame.last() == Some(&b'\r') {
                frame.pop();
            }
        }
        ensure!(
            (1..=MAX_PIN).contains(&(frame.len() - 4)) && frame[4..].iter().all(u8::is_ascii_digit),
            "invalid lock PIN file (expected 1–32 ASCII digits)"
        );
        frame[3] = (frame.len() - 4) as u8;
        Ok(frame)
    }
}

fn command_id(action: LockAction) -> u8 {
    match action {
        LockAction::Lock => 0,
        LockAction::Unlock => 1,
    }
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum Stage {
    Policy,
    Command,
    State,
}

pub(super) enum Step {
    Ignore,
    Sent,
    Declined(u8),
    Complete(Value),
}

pub(super) struct Operation {
    action: LockAction,
    stage: Stage,
}

async fn send(
    link: &mut Ezsp<'_>,
    peer: Identity,
    destination: &Destination,
    profile: u16,
    wire: &[u8],
) -> Result<()> {
    let node = zdo::node(link, peer.0).await?;
    ensure!(node == destination.short() && link.current(destination), "lock address changed during operation");
    let status = zdo::submit_protected_unicast(
        link,
        destination,
        profile,
        DOOR_LOCK,
        (client_endpoint(DOOR_LOCK), peer.1),
        0xff,
        wire,
    )
    .await?;
    ensure!(
        status == 0,
        "adapter declined protected lock frame ({status:#04x})"
    );
    Ok(())
}

impl Operation {
    pub(super) fn command_accepted(&self) -> bool {
        self.stage != Stage::Policy
    }

    pub(super) async fn start(
        link: &mut Ezsp<'_>,
        peer: Identity,
        destination: &Destination,
        profile: u16,
        seq: u8,
        action: LockAction,
    ) -> Result<Self> {
        let mut lookup = peer.0.to_le_bytes().to_vec();
        lookup.push(1);
        let key = crate::coordinator::request(link, 0x75, &lookup).await?;
        ensure!(
            peer.0 != 0 && matches!(key.as_slice(), [index] if *index!=0xff),
            "no peer-specific link key on this coordinator"
        );
        send(
            link,
            peer,
            destination,
            profile,
            &[0x10, seq, 0, SECURITY_LEVEL as u8, 0],
        )
        .await?;
        Ok(Self {
            action,
            stage: Stage::Policy,
        })
    }

    pub(super) async fn advance(
        &mut self,
        link: &mut Ezsp<'_>,
        exchange: (&Credentials, &Bindings, &Publisher),
        destination: &Destination,
        peer: Identity,
        msg: &Incoming<'_>,
        f: Frame<'_>,
    ) -> Result<Step> {
        let (credentials, devices, out) = exchange;
        if !msg.aps_encrypted
            || f.header.direction != Direction::ServerToClient
            || f.header.manufacturer.is_some()
        {
            return Ok(Step::Ignore);
        }
        let seq = super::endpoint_admin::advance_sequence(f.header.sequence, 1);
        if f.header.general_command() == Some(GeneralCommand::DefaultResponse) {
            let expected = if self.stage == Stage::Command {
                command_id(self.action)
            } else {
                0
            };
            if let [cmd, status] = *f.payload
                && cmd == expected
                && status != 0
            {
                if self.stage == Stage::Command {
                    return Ok(Step::Declined(status));
                }
                bail!("device rejected lock exchange ({status:#04x})");
            }
            return Ok(Step::Ignore);
        }
        match self.stage {
            Stage::Policy | Stage::State => {
                let Ok(Payload::ReadAttributesResponse(rows)) = Payload::decode(f) else {
                    return Ok(Step::Ignore);
                };
                let [row] = rows.as_slice() else {
                    return Ok(Step::Ignore);
                };
                if self.stage == Stage::Policy {
                    if row.id != SECURITY_LEVEL {
                        return Ok(Step::Ignore);
                    }
                    ensure!(
                        matches!(&row.result, Status::Success(v) if v.data_type().id()==0x30 && v.as_bytes()==[0])
                            || matches!(row.result, Status::Error(s) if s.get()==0x86),
                        "lock requires a different commissioning security policy"
                    );
                    let wire = credentials.command(peer, self.action, seq)?;
                    send(link, peer, destination, msg.aps.profile, &wire).await?;
                    self.stage = Stage::Command;
                } else {
                    if row.id != 0 {
                        return Ok(Step::Ignore);
                    }
                    let Status::Success(v) = &row.result else {
                        bail!("command acknowledged, lock-state read failed")
                    };
                    let field = zigbee_devices::equipment::FIELDS
                        .iter()
                        .find(|f| f.cluster == DOOR_LOCK && f.id == 0)
                        .expect("standard lock state");
                    let state = field
                        .decode(*v)
                        .context("command acknowledged, lock state unknown")?;
                    let requested = match self.action {
                        LockAction::Lock => "locked",
                        LockAction::Unlock => "unlocked",
                    };
                    let binding = devices
                        .devices
                        .get(&peer)
                        .context("lock membership changed")?;
                    submitted(out.state(&binding.name, "lock_state", state.clone()))?;
                    return Ok(Step::Complete(
                        json!({"lock_state":state,"matches_request":state==requested}),
                    ));
                }
            }
            Stage::Command => {
                if f.header.frame_type != FrameType::ClusterSpecific
                    || f.header.command_id != command_id(self.action)
                {
                    return Ok(Step::Ignore);
                }
                let [status] = *f.payload else {
                    return Ok(Step::Ignore);
                };
                if status != 0 {
                    return Ok(Step::Declined(status));
                }
                send(
                    link,
                    peer,
                    destination,
                    msg.aps.profile,
                    &[0x10, seq, 0, 0, 0],
                )
                .await?;
                self.stage = Stage::State;
            }
        }
        Ok(Step::Sent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::symlink};
    use tempfile::tempdir;

    #[test]
    fn credentials_remain_outside_state_and_follow_rotation_without_following_links() -> Result<()>
    {
        let state = tempdir()?;
        let members = state.path().join("members");
        fs::create_dir(&members)?;
        assert!(Credentials::open(&members, &members).is_err());
        let root = tempdir()?;
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700))?;
        let store = Credentials::open(root.path(), &members)?;
        let peer = (1, 2);
        let name = root.path().join("0000000000000001-2.pin");
        assert!(store.command(peer, LockAction::Lock, 0).is_err());
        let placeholder = root.path().join("fixture");
        // Generate a dummy PIN, so no operator credential is needed in source or test output.
        let bytes = std::process::id().to_string();
        fs::write(&placeholder, format!("{bytes}\r\n"))?;
        fs::set_permissions(&placeholder, fs::Permissions::from_mode(0o600))?;
        symlink(&placeholder, &name)?;
        assert!(store.command(peer, LockAction::Lock, 0).is_err());
        fs::rename(placeholder, &name)?;
        let wire = store.command(peer, LockAction::Unlock, 37)?;
        assert_eq!(wire[..4], [0x11, 37, 1, bytes.len() as u8]);
        assert!(wire[4..] == *bytes.as_bytes());
        fs::set_permissions(name, fs::Permissions::from_mode(0o644))?;
        assert!(store.command(peer, LockAction::Unlock, 37).is_err());
        Ok(())
    }
}
