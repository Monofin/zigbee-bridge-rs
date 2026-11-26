//! Coordinator startup against the negotiated Ember wire codec.
use crate::ash::{Callback, Ezsp};
use anyhow::{Context, Result, bail, ensure};
use std::{fs::File, io::Read, time::Duration};

pub(crate) const DEADLINE: Duration = Duration::from_secs(8);
const SET_CONCENTRATOR: u16 = 0x10;
const SET_SOURCE_ROUTE_DISCOVERY_MODE: u16 = 0x5a;
const NETWORK_INIT: u16 = 0x17;
const NETWORK_STATE: u16 = 0x18;
const FORM_NETWORK: u16 = 0x1e;
const PERMIT_JOINING: u16 = 0x22;
const GET_NODE_ID: u16 = 0x27;
const GET_NETWORK_PARAMETERS: u16 = 0x28;
const GET_CONFIGURATION: u16 = 0x52;
const TC_ADDRESS_CACHE_SIZE: u8 = 0x19;
const SET_CONFIGURATION: u16 = 0x53;
const SET_POLICY: u16 = 0x55;
const GET_POLICY: u16 = 0x56;
const SET_INITIAL_SECURITY_STATE: u16 = 0x68;
const GET_TOKEN_DATA: u16 = 0x0102;
const GET_NETWORK_KEY_INFO: u16 = 0x0116;
const STACK_STATUS: u16 = 0x19;
const NETWORK_UP: u8 = 0x90;
const NOT_JOINED: u8 = 0x93;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Network {
    pub channel: u8,
    pub pan: u16,
    /// Same little-endian octet order as EmberNetworkParameters.
    pub extended_pan: [u8; 8],
}

impl Network {
    fn validate(self) -> Result<()> {
        ensure!(
            (11..=26).contains(&self.channel) && self.pan < 0xffff,
            "invalid channel or PAN"
        );
        ensure!(
            self.extended_pan != [0; 8] && self.extended_pan != [255; 8],
            "reserved extended PAN"
        );
        Ok(())
    }
    fn parameters(self) -> Vec<u8> {
        let mut p = self.extended_pan.to_vec();
        p.extend(self.pan.to_le_bytes());
        p.extend([0, self.channel, 0, 0, 0, 0]); // 0 dBm, MAC association, manager and update ID zero
        p.extend((1u32 << self.channel).to_le_bytes());
        p
    }
}

#[derive(Debug)]
pub struct SavedNode {
    pub network: Network,
    pub role: u8,
}

pub(crate) async fn request(link: &mut Ezsp<'_>, id: u16, args: &[u8]) -> Result<Vec<u8>> {
    link.runtime_command(id, args, DEADLINE)
        .await
        .with_context(|| format!("EZSP command {id:#06x}"))
}
async fn status(link: &mut Ezsp<'_>, id: u16, args: &[u8]) -> Result<u8> {
    let p = request(link, id, args).await?;
    let [s] = p[..] else {
        bail!("unexpected status layout for EZSP {id:#06x}")
    };
    Ok(s)
}
pub(crate) async fn checked(link: &mut Ezsp<'_>, id: u16, args: &[u8]) -> Result<()> {
    let s = status(link, id, args).await?;
    ensure!(s == 0, "EZSP {id:#06x} returned {s:#04x}");
    Ok(())
}

const TRUST_CENTER_POLICY: u8 = 0x00;
const TC_REJOINS_USING_WELL_KNOWN_KEY_POLICY: u8 = 0x09;
const ALLOW_JOINS: u8 = 0x01;
const ALLOW_UNSECURED_REJOINS: u8 = 0x02;

async fn verified_policy(link: &mut Ezsp<'_>, policy: u8, decision: u8) -> Result<()> {
    checked(link, SET_POLICY, &[policy, decision]).await?;
    ensure!(request(link, GET_POLICY, &[policy]).await? == [0, decision],
            "EZSP policy {policy:#04x} did not read back {decision:#04x}");
    Ok(())
}

pub(crate) async fn join_policy(link: &mut Ezsp<'_>, open: bool) -> Result<()> {
    let mask = if open { ALLOW_JOINS | ALLOW_UNSECURED_REJOINS } else { ALLOW_UNSECURED_REJOINS };
    verified_policy(link, TRUST_CENTER_POLICY, mask).await
}

const TC_KEY_REQUEST_POLICY: u8 = 0x05;
pub const DENY_TC_KEY_REQUESTS: u8 = 0x50;
pub const SEND_CURRENT_TC_KEY: u8 = 0x51;

pub async fn tc_key_request_policy(link: &mut Ezsp<'_>) -> Result<u8> {
    let raw = request(link, GET_POLICY, &[TC_KEY_REQUEST_POLICY]).await?;
    let [0, decision] = raw[..] else { bail!("failed to read TC_KEY_REQUEST_POLICY: {raw:02x?}") };
    println!("TC_KEY_REQUEST_POLICY readback {decision:#04x}");
    Ok(decision)
}

pub async fn set_tc_key_request_policy(link: &mut Ezsp<'_>, decision: u8) -> Result<()> {
    checked(link, SET_POLICY, &[TC_KEY_REQUEST_POLICY, decision]).await?;
    ensure!(tc_key_request_policy(link).await? == decision, "TC key policy readback mismatch");
    Ok(())
}

pub async fn inspect(link: &mut Ezsp<'_>) -> Result<SavedNode> {
    // SDK token-stack.h: primary STACK_NODE_DATA contains no security keys.
    let mut arg = 0x1ee64_u32.to_le_bytes().to_vec();
    arg.extend(0u32.to_le_bytes());
    let p = request(link, GET_TOKEN_DATA, &arg).await?;
    let [
        0,
        16,
        0,
        0,
        0,
        pan0,
        pan1,
        _,
        channel,
        _,
        role,
        _,
        _,
        ext @ ..,
    ] = &p[..]
    else {
        bail!("unrecognised STACK_NODE_DATA token")
    };
    Ok(SavedNode {
        role: *role,
        network: Network {
            pan: u16::from_le_bytes([*pan0, *pan1]),
            channel: *channel,
            extended_pan: ext.try_into()?,
        },
    })
}

#[derive(Clone, Copy)]
pub enum StartupMode {
    Form,
    Resume,
}

impl StartupMode {
    fn check(self, saved: &SavedNode, expected: Network) -> Result<()> {
        expected.validate()?;
        match self {
            Self::Form => ensure!(
                saved.role == 0 && saved.network.pan == 0xffff,
                "formation requires an empty coordinator; inspect the saved network"
            ),
            Self::Resume => ensure!(
                saved.role == 1 && saved.network == expected,
                "saved coordinator identity differs from the requested network"
            ),
        }
        Ok(())
    }
}

pub async fn start(link: &mut Ezsp<'_>, expected: Network, mode: StartupMode) -> Result<()> {
    start_configured(link, expected, mode, None, None).await
}

pub async fn start_configured(link: &mut Ezsp<'_>, expected: Network, mode: StartupMode,
    children: Option<u8>, tx_dbm: Option<i8>) -> Result<()> {
    let saved = inspect(link).await?;
    mode.check(&saved, expected)?;
    if matches!(mode, StartupMode::Form) {
        // Security Manager's key-info response contains flags and counters only.
        // A detached network may still have a current or alternate key.
        let keys = request(link, GET_NETWORK_KEY_INFO, &[]).await?;
        ensure!(
            matches!(keys.as_slice(), [0, 0, 0, 0, 0, 0, _, _, _, _, _, _]),
            "formation requires both current and alternate network keys to be unset"
        );
    }
    // Ember needs its own IEEE-to-short mapping to decrypt APS key requests
    // forwarded by routers. A host-side address cache cannot serve that path.
    let cache_size = request(link, GET_CONFIGURATION, &[TC_ADDRESS_CACHE_SIZE]).await?;
    let [0, lo, hi] = cache_size[..] else { bail!("cannot read trust-centre address cache size") };
    let initial = u16::from_le_bytes([lo, hi]);
    if initial < 2 {
        checked(link, SET_CONFIGURATION, &[TC_ADDRESS_CACHE_SIZE, 2, 0]).await?;
        ensure!(request(link, GET_CONFIGURATION, &[TC_ADDRESS_CACHE_SIZE]).await? == [0, 2, 0],
                "trust-centre address cache readback differs");
    }
    println!("Trust-centre security address cache: {initial} entries at startup, {} verified", initial.max(2));
    if let Some(children) = children {
        let expected = [0, children, 0];
        if request(link, GET_CONFIGURATION, &[0x11]).await? != expected {
            checked(link, SET_CONFIGURATION, &[0x11, children, 0]).await?;
            ensure!(request(link, GET_CONFIGURATION, &[0x11]).await? == expected, "child limit readback differs");
        }
    }
    checked(link, SET_CONFIGURATION, &[12, 2, 0]).await?; // Zigbee PRO stack profile
    checked(link, SET_CONFIGURATION, &[13, 5, 0]).await?; // Zigbee security level
    // Existing members can recover with their own link key, even with joining closed.
    verified_policy(link, TC_REJOINS_USING_WELL_KNOWN_KEY_POLICY, 0).await?;
    join_policy(link, false).await?;
    ensure!(
        status(link, NETWORK_STATE, &[]).await? == 0,
        "unexpected active network before initialization"
    );
    let init = status(link, NETWORK_INIT, &[0, 0]).await?;
    if matches!(mode, StartupMode::Form) {
        ensure!(
            init == NOT_JOINED,
            "refusing formation: networkInit returned {init:#04x}"
        );
        // Separate CSPRNG material for the network key and trust-centre hashed
        // link key. Keys stay on the coordinator and out of logs and CLI args.
        let mut security = [0u8; 43];
        // Have both keys, hashed TC link keys, encrypted transport, retain frame counters.
        security[..2].copy_from_slice(&0x1b84_u16.to_le_bytes());
        File::open("/dev/urandom")?.read_exact(&mut security[2..34])?;
        checked(link, SET_INITIAL_SECURITY_STATE, &security).await?;
        checked(link, FORM_NETWORK, &expected.parameters()).await?;
    } else {
        ensure!(init == 0, "networkInit returned {init:#04x}");
    }
    tokio::time::timeout(DEADLINE, async {
        loop {
            let cb = link
                .next_callback()
                .await
                .map_err(|e| anyhow::anyhow!("waiting for network up: {e:?}"))?;
            if cb.id == STACK_STATUS {
                ensure!(
                    cb.payload == [NETWORK_UP],
                    "unexpected stack status during startup: {:?}",
                    cb.payload
                );
                return Ok::<(), anyhow::Error>(());
            }
        }
    })
    .await
    .context("no NETWORK_UP callback")??;
    verify(link, expected).await?;
    if let Some(dbm) = tx_dbm {
        let desired = dbm.to_le_bytes();
        let current = request(link, GET_NETWORK_PARAMETERS, &[]).await?;
        if current.get(12) != desired.first() {
            checked(link, 0x99, &desired).await?;
            let actual = request(link, GET_NETWORK_PARAMETERS, &[]).await?;
            ensure!(actual.len() == 22 && actual[..2] == [0, 1] && actual[12] == desired[0],
                "radio power readback differs");
        }
    }
    checked(link, PERMIT_JOINING, &[0]).await?; // close local joining explicitly
    let policy = request(link, GET_POLICY, &[TRUST_CENTER_POLICY]).await?;
    ensure!(
        policy == [0, ALLOW_UNSECURED_REJOINS],
        "trust centre join policy differs from requested policy"
    );
    start_route_discovery(link).await
}

async fn start_route_discovery(link: &mut Ezsp<'_>) -> Result<()> {
    // Ember high-RAM concentrator, 5–60 s between broadcasts, thresholds
    // of three route errors or one delivery failure, stack-default hop limit.
    let mut parameters = vec![1];
    parameters.extend(0xfff9u16.to_le_bytes());
    parameters.extend(5u16.to_le_bytes());
    parameters.extend(60u16.to_le_bytes());
    parameters.extend([3, 1, 0]);
    checked(link, SET_CONCENTRATOR, &parameters).await?;
    let delay = match route_discovery_delay(link, 2).await? {
        Some(delay) => delay,
        None => route_discovery_delay(link, 1).await?
            .context("Ember many-to-one route discovery remains disabled")?,
    };
    println!("Many-to-one route discovery scheduled by adapter: {delay} ms");
    Ok(())
}

// Ember returns UINT32_MAX when discovery is off. Rescheduling an existing
// schedule and enabling it are distinct operations.
async fn route_discovery_delay(link: &mut Ezsp<'_>, mode: u8) -> Result<Option<u32>> {
    let raw = request(link, SET_SOURCE_ROUTE_DISCOVERY_MODE, &[mode]).await?;
    let delay = u32::from_le_bytes(raw.try_into()
        .map_err(|_| anyhow::anyhow!("invalid Ember route-discovery delay"))?);
    Ok((delay != u32::MAX).then_some(delay))
}

pub async fn verify(link: &mut Ezsp<'_>, expected: Network) -> Result<()> {
    let p = request(link, GET_NETWORK_PARAMETERS, &[]).await?;
    ensure!(
        p.len() == 22 && p[..2] == [0, 1],
        "getNetworkParameters did not return a coordinator"
    );
    ensure!(
        p[2..10] == expected.extended_pan
            && p[10..12] == expected.pan.to_le_bytes()
            && p[13] == expected.channel,
        "coordinator returned an unexpected network identity"
    );
    ensure!(
        status(link, NETWORK_STATE, &[]).await? == 2
            && request(link, GET_NODE_ID, &[]).await? == [0, 0],
        "coordinator did not confirm joined state and address zero"
    );
    Ok(())
}

/// A loss of stack state invalidates readiness until a fresh startup verifies it.
pub fn observe(callback: Callback) -> Result<()> {
    if callback.id == STACK_STATUS {
        let [s] = callback.payload[..] else {
            bail!("malformed stack status")
        };
        // NETWORK_CLOSED is expected after disabling permit joining.
        ensure!(
            matches!(s, NETWORK_UP | 0x9d),
            "coordinator stack state changed: {s:#04x}"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_verifies_the_saved_network_before_formation_or_resume() {
        let expected = Network {
            channel: 20,
            pan: 0x1234,
            extended_pan: [1; 8],
        };
        let mut saved = SavedNode {
            network: expected,
            role: 1,
        };
        assert!(StartupMode::Resume.check(&saved, expected).is_ok());
        assert!(StartupMode::Form.check(&saved, expected).is_err());
        for other in [
            Network {
                channel: 26,
                ..expected
            },
            Network {
                pan: 42,
                ..expected
            },
            Network {
                extended_pan: [2; 8],
                ..expected
            },
        ] {
            assert!(StartupMode::Resume.check(&saved, other).is_err());
        }
        saved.role = 0;
        assert!(StartupMode::Resume.check(&saved, expected).is_err());
        assert!(StartupMode::Form.check(&saved, expected).is_err());
        saved.network.pan = 0xffff;
        assert!(StartupMode::Form.check(&saved, expected).is_ok());
    }

    #[test]
    fn network_closed_is_benign_and_network_opened_invalidates_readiness() {
        for (status, ok) in [(0x90, true), (0x9d, true), (0x91, false), (0x9c, false)] {
            assert_eq!(
                observe(Callback {
                    id: STACK_STATUS,
                    payload: vec![status]
                })
                .is_ok(),
                ok
            );
        }
    }
}
