//! A client cluster supplies a trigger for incoming standard commands. The
//! trigger's payload carries the exact command, including common dimmer arguments.
use super::{Bindings, Identity};
use crate::{
    interview_log::Descriptor,
    mqtt::discovery::{Endpoint, Feature},
};
pub(super) fn cluster_name(cluster: u16) -> Option<&'static str> {
    Some(match cluster {
        3 => "identify",
        5 => "scenes",
        6 => "on_off",
        8 => "level",
        0x102 => "cover",
        0x300 => "color",
        _ => return None,
    })
}
fn relevant(ep: u8, d: &Descriptor) -> bool {
    super::standard::application(ep, d)
        && d.clients
            .iter()
            .any(|&c| c != 3 && cluster_name(c).is_some())
}
impl Bindings {
    pub(super) fn has_remote_trigger(&self, peer: Identity, d: &Descriptor) -> bool {
        relevant(peer.1, d)
            && self.devices.get(&peer).is_none_or(|b| {
                b.button.is_none()
                    && !b.datapoints.is_some_and(|d| d.fields().iter().any(|f| f.key == "action"))
                    && !b.household.input_property("action")
            })
    }
    pub(super) fn remote_features(&self, ieee: u64) -> Vec<(Endpoint<'_>, Feature)> {
        let Some((name, eps)) = self.peers.get(&ieee) else {
            return Vec::new();
        };
        let mut results = Vec::new();
        for (&id, d) in eps {
            if !self.has_remote_trigger((ieee, id), d) {
                continue;
            }
            for &cluster in &d.clients {
                let Some(cluster_name) = cluster_name(cluster) else {
                    continue;
                };
                results.push((
                    Endpoint {
                        ieee,
                        endpoint: id,
                        device_name: name,
                        topic_name: name,
                        device_availability: self.availability.enabled(ieee),
                    },
                    Feature::RemoteTrigger {
                        cluster,
                        name: cluster_name,
                    },
                ));
            }
        }
        results
    }
}
