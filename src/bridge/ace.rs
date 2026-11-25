//! Read-only IAS ACE CIE services (ZCL R8 §8.3). The physical panel owns arming.
use super::{Bindings, Identity, Management};
use crate::{ash::Ezsp, interview::Incoming};
use anyhow::{Result, ensure};
use serde::Deserialize;
use std::{collections::BTreeSet, fs::File, io::ErrorKind, path::Path};
use zigbee_devices::ias::ace::DirectoryRequest;
use zigbee_zcl::{Frame, foundation::GeneralCommand};

pub(super) use zigbee_devices::ias::ace::{CLUSTER, Zone};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Reader {
    ieee: String,
    endpoint: u8,
}

#[derive(Default)]
pub(super) struct Readers(BTreeSet<Identity>);

impl Readers {
    pub(super) fn has_member(&self, ieee: u64) -> bool {
        self.0.range((ieee, 0)..=(ieee, 255)).next().is_some()
    }

    pub(super) fn load(dir: &Path) -> Result<Self> {
        let path = dir.join("ias-ace-keypads.json");
        let file = match crate::store::open_regular(&path, File::options().read(true)) {
            Ok(f) => f,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e.into()),
        };
        let mut readers = Self::default();
        for r in serde_json::from_reader::<_, Vec<Reader>>(file)? {
            let hex = r.ieee.strip_prefix("0x").unwrap_or(&r.ieee);
            ensure!(
                hex.len() == 16 && hex.bytes().all(|b| b.is_ascii_hexdigit()),
                "invalid IAS ACE reader IEEE"
            );
            let ieee = u64::from_str_radix(hex, 16)?;
            ensure!(
                !matches!(ieee, 0 | u64::MAX)
                    && (1..=240).contains(&r.endpoint)
                    && readers.0.insert((ieee, r.endpoint)),
                "invalid or duplicate IAS ACE reader"
            );
        }
        Ok(readers)
    }
}

impl Management {
    pub(super) async fn keypad(
        &self,
        link: &mut Ezsp<'_>,
        devices: &Bindings,
        peer: Identity,
        msg: &Incoming<'_>,
        frame: Frame<'_>,
    ) -> Result<()> {
        if !matches!(msg.kind, 0 | 1) || !msg.aps_encrypted {
            return Ok(());
        }
        let Some(request) = DirectoryRequest::from_frame(frame) else {
            return Ok(());
        };
        let known = devices
            .peers
            .get(&peer.0)
            .and_then(|(_, eps)| eps.get(&peer.1));
        let authorized = self.ace_readers.0.contains(&peer)
            && known.is_some_and(|d| d.profile == msg.aps.profile && d.clients.contains(&CLUSTER))
            && !self.zones.departed(peer.0);
        let wire = if authorized {
            request.reply(&self.zones.directory(devices, |d| link.current(d)))
        } else {
            super::responses::reply(
                frame.header,
                GeneralCommand::DefaultResponse,
                &[frame.header.command_id, 0x7e],
            )
        };
        let target=link.destination(peer.0,msg.sender)?;
        super::responses::send(link, &target, msg, &wire).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn direct_panel_binding_requires_matching_roles_and_unicast() -> Result<()> {
        use crate::{
            bridge::zdo::BindingPlan,
            interview_log::Descriptor,
            mqtt::admin::{Bind, Selector},
        };
        let mut devices = Bindings::default();
        for (ieee, name, ep, client) in [
            (42, "keypad", 3, true),
            (77, "panel", 7, false),
            (99, "other_keypad", 7, true),
        ] {
            let d = Descriptor {
                profile: 0x104,
                device: 0xffff,
                version: 1,
                servers: if client { vec![] } else { vec![CLUSTER] },
                clients: if client { vec![CLUSTER] } else { vec![] },
            };
            devices
                .peers
                .insert(ieee, (name.to_owned(), [(ep, d)].into()));
        }
        let mut args = Bind {
            from: "keypad".into(),
            from_endpoint: Some(Selector::Number(3)),
            to: Selector::Name("panel".into()),
            to_endpoint: Some(Selector::Number(7)),
            clusters: Some(vec!["ssIasAce".into()]),
            skip_disable_reporting: false,
        };
        let plan = BindingPlan::new(&devices, &args, false, 1)?;
        assert_eq!(
            plan.payload(4, CLUSTER),
            [
                4, 42, 0, 0, 0, 0, 0, 0, 0, 3, 1, 5, 3, 77, 0, 0, 0, 0, 0, 0, 0, 7
            ]
        );
        args.to = Selector::Name("other_keypad".into());
        assert!(BindingPlan::new(&devices, &args, false, 1).is_err());
        args.to = Selector::Number(7);
        args.to_endpoint = None;
        assert!(BindingPlan::new(&devices, &args, false, 1).is_err());
        args.to = Selector::Name("Coordinator".into());
        args.to_endpoint = Some(Selector::Number(2));
        assert_eq!(
            BindingPlan::new(&devices, &args, false, 1)?
                .payload(4, CLUSTER)
                .last(),
            Some(&2)
        );
        Ok(())
    }

    #[test]
    fn readers_are_specific_and_disabled_by_default() -> Result<()> {
        let dir = tempfile::tempdir()?;
        assert!(!Readers::load(dir.path())?.has_member(42));
        let path = dir.path().join("ias-ace-keypads.json");
        std::fs::write(&path, r#"[{"ieee":"000000000000002a","endpoint":3}]"#)?;
        let readers = Readers::load(dir.path())?;
        assert!(readers.0.contains(&(42, 3)));
        assert!(!readers.0.contains(&(42, 1)));
        std::fs::write(&path, r#"[{"ieee":"0000000000000000","endpoint":3}]"#)?;
        assert!(Readers::load(dir.path()).is_err());
        Ok(())
    }
}
