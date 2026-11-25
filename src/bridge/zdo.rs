//! Remote endpoint selection and confirmed ZDO binding transactions.
use super::{Bindings, Identity};
use crate::{
    ash::{Destination,Ezsp},
    coordinator::request,
    interview::Incoming,
    mqtt::admin::{Bind, Selector},
};
use anyhow::{Context, Result, ensure};
use std::{collections::BTreeSet, fmt};

/// No valid NCP acceptance or refusal was received for this unicast.
#[derive(Debug)]
pub(super) struct UncertainSubmission;

impl fmt::Display for UncertainSubmission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("unicast submission outcome unknown")
    }
}
impl std::error::Error for UncertainSubmission {}

fn unicast_status(result: Result<Vec<u8>>) -> Result<u8> {
    match result {
        Ok(raw) => match *raw.as_slice() {
            [status, _] => Ok(status),
            _ => Err(anyhow::anyhow!("invalid sendUnicast response").context(UncertainSubmission)),
        },
        Err(e) if crate::ash::locally_refused(&e)
            || e.downcast_ref::<crate::ash::ProbeError>().is_some_and(|p| p.delivery == crate::ash::Delivery::Unsent) => Err(e),
        Err(e) => Err(e.context(UncertainSubmission)),
    }
}

pub(super) async fn node(link: &mut Ezsp<'_>, ieee: u64) -> Result<u16> {
    lookup_node(link, ieee)
        .await?
        .context("device has no current network address")
}

pub(super) async fn lookup_node(link: &mut Ezsp<'_>, ieee: u64) -> Result<Option<u16>> {
    let raw = request(link, 0x60, &ieee.to_le_bytes()).await?;
    let node = u16::from_le_bytes(raw.as_slice().try_into()?);
    Ok(link.adapter_mapping(ieee,node).then_some(node))
}

/// A follow-up belongs to the original transaction's assignment. A lookup can
/// expose a conflict or buffer a leave while we await it. A plain miss conveys
/// no new ownership information. The final unicast checks the lease again.
pub(super) async fn still_assigned(link: &mut Ezsp<'_>, target: &Destination) -> Result<bool> {
    if !link.current(target) { return Ok(false); }
    let node = lookup_node(link, target.ieee()).await?;
    Ok(node.is_none_or(|short| short == target.short()) && link.current(target))
}

pub(super) async fn lookup_ieee(link: &mut Ezsp<'_>, node: u16) -> Result<Option<u64>> {
    let raw = request(link, 0x61, &node.to_le_bytes()).await?;
    let (&status, bytes) = raw.split_first().context("missing sender lookup status")?;
    if status != 0 {
        return Ok(None);
    }
    let ieee=u64::from_le_bytes(bytes.try_into()?);
    Ok(link.adapter_mapping(ieee,node).then_some(ieee))
}

pub(super) async fn unicast(
    link: &mut Ezsp<'_>,
    target: &Destination,
    profile: u16,
    cluster: u16,
    endpoints: (u8, u8),
    tag: u8,
    data: &[u8],
) -> Result<()> {
    let status = submit_unicast(link, target, profile, cluster, endpoints, tag, data).await?;
    ensure!(status == 0, "adapter declined unicast ({status:#04x})");
    Ok(())
}

pub(super) async fn submit_unicast(
    link: &mut Ezsp<'_>,
    target: &Destination,
    profile: u16,
    cluster: u16,
    endpoints: (u8, u8),
    tag: u8,
    data: &[u8],
) -> Result<u8> {
    send_aps(link, target, profile, cluster, endpoints, (tag, false), data).await
}

pub(super) async fn submit_protected_unicast(
    link: &mut Ezsp<'_>,
    target: &Destination,
    profile: u16,
    cluster: u16,
    endpoints: (u8, u8),
    tag: u8,
    data: &[u8],
) -> Result<u8> {
    send_aps(link, target, profile, cluster, endpoints, (tag, true), data).await
}

async fn send_aps(
    link: &mut Ezsp<'_>,
    target: &Destination,
    profile: u16,
    cluster: u16,
    endpoints: (u8, u8),
    (tag, protected): (u8, bool),
    data: &[u8],
) -> Result<u8> {
    let mut args = vec![0];
    args.extend(target.short().to_le_bytes());
    args.extend(profile.to_le_bytes());
    args.extend(cluster.to_le_bytes());
    args.extend([
        endpoints.0,
        endpoints.1,
        if protected { 0x40 | crate::interview::APS_ENCRYPTION } else { 0x40 },
        1,
        0,
        0,
        0,
        tag,
        data.len().try_into()?,
    ]);
    args.extend(data);
    unicast_status(link.submit_to(target, &args).await)
}

pub(super) async fn probe_ieee(link: &mut Ezsp<'_>, node:u16, seq:u8) -> Result<u8> {
    let [lo,hi]=node.to_le_bytes();
    let args=[0,lo,hi,0,0,1,0,0,0,0x40,1,0,0,0,0xff,5,seq,lo,hi,0,0];
    let raw=link.probe_ieee(node,&args).await?;
    let [status,_]=*raw.as_slice() else {anyhow::bail!("invalid address discovery submission result")};
    Ok(status)
}

// Management Permit Joining to all routers. The trust centre's own admission
// policy is still authoritative. Broadcast delivery has no per-router receipt.
pub(super) async fn broadcast_permit(link: &mut Ezsp<'_>, seconds: u8, seq: u8) -> Result<()> {
    let status = submit_broadcast(link, 0xfffc, 0x0036, &[seq, seconds, 1]).await?;
    ensure!(status == 0, "adapter declined permit-join broadcast");
    Ok(())
}

pub(super) async fn submit_broadcast(
    link: &mut Ezsp<'_>,
    destination: u16,
    cluster: u16,
    payload: &[u8],
) -> Result<u8> {
    let mut args = destination.to_le_bytes().to_vec();
    args.extend(0u16.to_le_bytes()); // ZDO profile
    args.extend(cluster.to_le_bytes());
    args.extend([0; 8]); // endpoints, APS options, group, APS sequence and default radius
    args.extend([0xff, payload.len().try_into()?]);
    args.extend(payload);
    let raw = request(link, 0x36, &args).await?;
    let [status, _sequence] = *raw.as_slice() else {
        anyhow::bail!("invalid sendBroadcast response")
    };
    Ok(status)
}

impl Bindings {
    pub(super) fn peer(&self, s: &str) -> Result<u64> {
        if let Some(hex) = s.strip_prefix("0x")
            && hex.len() == 16
            && let Ok(id) = u64::from_str_radix(hex, 16)
            && self.peers.contains_key(&id)
        {
            return Ok(id);
        }
        self.peers
            .iter()
            .find(|(_, (name, _))| name == s)
            .map(|(&id, _)| id)
            .context("unknown device")
    }

    fn endpoint_id(&self, name: &str, explicit: Option<&Selector>) -> Result<Identity> {
        let (ieee, suffix) = if let Ok(id) = self.peer(name) {
            (id, None)
        } else if let Some(&(id, ep)) = self.topics.get(name) {
            (id, Some(ep))
        } else {
            let (base, ep) = name.rsplit_once('/').context("unknown binding device")?;
            (
                self.peer(base)?,
                Some(ep.trim_start_matches("ep").parse::<u8>()?),
            )
        };
        let table = &self.peers[&ieee].1;
        let specified = match explicit {
            None => None,
            Some(Selector::Number(n)) => Some(u8::try_from(*n)?),
            Some(Selector::Name(s)) => Some(
                s.trim_start_matches("ep")
                    .parse::<u8>()
                    .context("unknown endpoint label")?,
            ),
        };
        ensure!(
            suffix.is_none() || specified.is_none_or(|s| Some(s) == suffix),
            "conflicting endpoint selectors"
        );
        let ep = specified
            .or(suffix)
            .or_else(|| table.keys().copied().find(|e| (1..=240).contains(e)))
            .context("device has no application endpoint")?;
        ensure!(
            table.contains_key(&ep) && (1..=240).contains(&ep),
            "unknown application endpoint"
        );
        Ok((ieee, ep))
    }
}

const CLUSTERS: &[(u16, &str)] = &[
    (0, "genBasic"),
    (1, "genPowerCfg"),
    (3, "genIdentify"),
    (4, "genGroups"),
    (5, "genScenes"),
    (6, "genOnOff"),
    (8, "genLevelCtrl"),
    (0x19, "genOta"),
    (0x102, "closuresWindowCovering"),
    (0x201, "hvacThermostat"),
    (0x300, "lightingColorCtrl"),
    (0x400, "msIlluminanceMeasurement"),
    (0x402, "msTemperatureMeasurement"),
    (0x403, "msPressureMeasurement"),
    (0x405, "msRelativeHumidity"),
    (0x406, "msOccupancySensing"),
    (0x408, "msSoilMoisture"),
    (0x40d, "msCO2"),
    (0x500, "ssIasZone"),
    (0x501, "ssIasAce"),
    (0x702, "seMetering"),
    (0xb04, "haElectricalMeasurement"),
];

pub(super) fn cluster_name(id: u16) -> String {
    CLUSTERS
        .iter()
        .find(|(n, _)| *n == id)
        .map_or_else(|| format!("0x{id:04x}"), |(_, s)| s.to_string())
}
fn cluster_id(s: &str) -> Result<u16> {
    if let Some((id, _)) = CLUSTERS.iter().find(|(_, n)| *n == s) {
        return Ok(*id);
    }
    if let Some(hex) = s.strip_prefix("0x") {
        return Ok(u16::from_str_radix(hex, 16)?);
    }
    s.parse().context("unknown binding cluster")
}

#[derive(Clone, Copy)]
enum BindDestination {
    Coordinator(u64),
    Endpoint(Identity),
    Group(u16),
}

pub(super) struct BindingPlan {
    pub source: Identity,
    dest: BindDestination,
    pub clusters: Vec<u16>,
    pub request: u16,
}
impl BindingPlan {
    pub fn new(devices: &Bindings, args: &Bind, remove: bool, coordinator: u64) -> Result<Self> {
        let source = devices.endpoint_id(&args.from, args.from_endpoint.as_ref())?;
        let desc = &devices.peers[&source.0].1[&source.1];
        let dest = match &args.to {
            Selector::Number(n) => {
                ensure!(
                    (1..=0xfff7).contains(n) && args.to_endpoint.is_none(),
                    "invalid group target"
                );
                BindDestination::Group(*n)
            }
            Selector::Name(s) if s.eq_ignore_ascii_case("coordinator") => {
                match &args.to_endpoint {
                    None => BindDestination::Coordinator(coordinator),
                    Some(selector) => {
                        let ep: u8 = match selector {
                            Selector::Number(n) => (*n).try_into()?,
                            Selector::Name(name) => name.parse()?,
                        };
                        ensure!(crate::interview::valid_endpoint(ep), "unknown coordinator endpoint");
                        BindDestination::Endpoint((coordinator, ep))
                    }
                }
            }
            Selector::Name(s) => {
                BindDestination::Endpoint(devices.endpoint_id(s, args.to_endpoint.as_ref())?)
            }
        };
        let mut available = BTreeSet::new();
        match dest {
            BindDestination::Endpoint((e, ep)) if e != coordinator => {
                let other = &devices.peers[&e].1[&ep];
                ensure!(
                    source != (e, ep) && desc.profile == other.profile,
                    "incompatible binding endpoints"
                );
                available.extend(
                    desc.clients
                        .iter()
                        .filter(|id| other.servers.contains(id))
                        .copied(),
                );
                available.extend(
                    desc.servers
                        .iter()
                        .filter(|id| other.clients.contains(id))
                        .copied(),
                );
            }
            BindDestination::Group(_) => available.extend(desc.clients.iter().copied()),
            BindDestination::Endpoint((_, ep)) if ep != 1 => available.extend(
                desc.servers.iter().filter(|&&cluster| crate::interview::client_endpoint(cluster) == ep).copied()),
            _ => available.extend(desc.servers.iter().chain(&desc.clients).copied()),
        }
        if let BindDestination::Endpoint((owner,ep))=dest && owner==coordinator
            && ep==crate::interview::client_endpoint(super::ace::CLUSTER) && desc.clients.contains(&super::ace::CLUSTER) {
            available.insert(super::ace::CLUSTER);
        }
        let clusters = if let Some(names) = &args.clusters {
            let ids = names
                .iter()
                .map(|s| cluster_id(s))
                .collect::<Result<BTreeSet<_>>>()?;
            ensure!(
                remove || ids.is_subset(&available),
                "cluster absent on selected endpoints"
            );
            ids
        } else {
            // Match Z2M's default command/report cluster subset.
            available.retain(|c| {
                matches!(
                    *c,
                    5 | 6 | 8 | 0x102 | 0x201 | 0x300 | 0x400 | 0x402 | 0x405 | 0x408 | 0x40d
                )
            });
            available
        };
        ensure!(
            !clusters.is_empty() && clusters.len() <= 32,
            "supply 1–32 compatible clusters"
        );
        if !remove && clusters.contains(&super::ace::CLUSTER) {
            ensure!(!matches!(dest,BindDestination::Group(_)), "IAS ACE requires a unicast endpoint");
            if let BindDestination::Endpoint((owner,ep))=dest && owner==coordinator {
                ensure!(ep==crate::interview::client_endpoint(super::ace::CLUSTER), "incorrect local IAS ACE endpoint");
            }
        }
        Ok(Self {
            source,
            dest,
            clusters: clusters.into_iter().collect(),
            request: if remove { 0x22 } else { 0x21 },
        })
    }
    pub fn payload(&self, seq: u8, cluster: u16) -> Vec<u8> {
        let mut b = vec![seq];
        b.extend(self.source.0.to_le_bytes());
        b.push(self.source.1);
        b.extend(cluster.to_le_bytes());
        match self.dest {
            BindDestination::Coordinator(e) => {
                b.push(3);
                b.extend(e.to_le_bytes());
                b.push(crate::interview::client_endpoint(cluster));
            }
            BindDestination::Group(n) => {
                b.push(1);
                b.extend(n.to_le_bytes());
            }
            BindDestination::Endpoint((e, ep)) => {
                b.push(3);
                b.extend(e.to_le_bytes());
                b.push(ep);
            }
        };
        b
    }
    pub fn confirmed(&self, msg: &Incoming<'_>, short: u16, seq: u8) -> Option<u8> {
        if msg.kind > 1
            || msg.sender != short
            || msg.aps.profile != 0
            || msg.aps.src != 0
            || msg.aps.dst != 0
            || msg.aps.cluster != self.request | 0x8000
        {
            return None;
        }
        match msg.body {
            [s, status] if *s == seq => Some(*status),
            _ => None,
        }
    }
}

#[cfg(test)]
mod submission_tests {
    use super::*;
    use crate::ash::{Delivery, Failure, ProbeError};

    #[test]
    fn only_a_possibly_submitted_unicast_is_indeterminate() {
        let timeout = |delivery| anyhow::Error::from(ProbeError { delivery, cause: Failure::Timeout });
        let unconfirmed = Delivery::OutcomeUnknown { link_acknowledged: true };
        // An ordinary lookup timeout does not imply that the device was commanded.
        assert!(!timeout(unconfirmed).is::<UncertainSubmission>());
        assert!(!unicast_status(Err(timeout(Delivery::Unsent))).unwrap_err().is::<UncertainSubmission>());
        assert!(unicast_status(Err(timeout(unconfirmed))).unwrap_err().is::<UncertainSubmission>());
        assert!(unicast_status(Ok(vec![0])).unwrap_err().is::<UncertainSubmission>());
        assert_eq!(unicast_status(Ok(vec![0x66, 0])).unwrap(), 0x66);
    }
}
