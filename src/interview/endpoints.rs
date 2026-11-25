//! Fixed local endpoints keep existing bindings stable and each Simple Descriptor
//! within the 64-byte limit (Zigbee specification, apscMaxDescriptorSize).
use zigbee_zcl::Direction;

pub(super) struct Endpoint {
    pub id: u8,
    pub servers: &'static [u16],
    pub clients: &'static [u16],
}

pub(super) const ENDPOINTS: &[Endpoint] = &[
    Endpoint {
        id: 1,
        servers: &[3, 5, 6, 8, 0xa, 0x19, 0x102, 0x300],
        clients: &[
            0, 1, 4, 5, 6, 8, 0x20, 0x102, 0x201, 0x202, 0x300, 0x400, 0x402, 0x403, 0x404, 0x405,
            0x406, 0x500, 0x702, 0xb04,
        ],
    },
    Endpoint {
        id: 2,
        servers: &[0x0501],
        clients: &[
            2, 7, 9, 0xc, 0xd, 0xe, 0xf, 0x10, 0x11, 0x12, 0x13, 0x14, 0x100, 0x101, 0x103, 0x200,
            0x203, 0x204, 0x301, 0x501, 0x502,
        ],
    },
    Endpoint {
        id: 3,
        servers: &[],
        clients: &[
            0x401, 0x407, 0x408, 0x409, 0x40a, 0x40b, 0x40c, 0x40d, 0x40e, 0x40f, 0x410, 0x411,
            0x412, 0x413, 0x414, 0x415, 0x416, 0x417, 0x418, 0x419, 0x41a, 0x41b, 0x41c, 0x41d,
            0x41e,
        ],
    },
    Endpoint {
        id: 4,
        servers: &[],
        clients: &[
            0x41f, 0x420, 0x421, 0x422, 0x423, 0x424, 0x425, 0x426, 0x427, 0x428, 0x429, 0x42a,
            0x42b,
        ],
    },
];

pub(crate) fn client_endpoint(cluster: u16) -> u8 {
    ENDPOINTS
        .iter()
        .find(|e| e.clients.contains(&cluster))
        .map_or(1, |e| e.id)
}

pub(crate) fn supports_cluster(cluster: u16, incoming: Direction) -> bool {
    ENDPOINTS.iter().any(|e| match incoming {
        Direction::ClientToServer => e.servers.contains(&cluster),
        Direction::ServerToClient => e.clients.contains(&cluster),
    })
}

pub(crate) fn valid_endpoint(id: u8) -> bool {
    ENDPOINTS.iter().any(|e| e.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn descriptors_fit_and_client_routes_are_unambiguous() {
        let mut ids = BTreeSet::new();
        let mut clients = BTreeSet::new();
        for e in ENDPOINTS {
            assert!(ids.insert(e.id));
            assert!(8 + 2 * (e.servers.len() + e.clients.len()) <= 64);
            for &cluster in e.clients {
                assert!(clients.insert(cluster));
                assert_eq!(client_endpoint(cluster), e.id);
            }
        }
    }
}
