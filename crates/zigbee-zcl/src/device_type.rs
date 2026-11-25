//! Profile-scoped application device types and the cluster-role evidence from
//! each template. HA/ZLO and Smart Energy revisions can share an identifier;
//! callers can inspect every candidate. An interview supplies actual clusters.

/// These values describe the SDK template switch and its lock. A missing
/// cluster entry means the source supplied no role here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Required,
    /// The template permits this role and leaves it off initially.
    Optional,
    /// The template permits this role and preselects it.
    OptionalDefaultOn,
    /// The template locks this role off.
    Excluded,
    /// Repeated declarations disagree inside this single template.
    Conflicting,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cluster {
    pub id: u16,
    pub server: Role,
    pub client: Role,
}

#[derive(Debug)]
pub struct DeviceType {
    pub profile: u16,
    pub device: u16,
    pub name: &'static str,
    pub template: &'static str,
    pub source: &'static str,
    /// The literal profile in the source, including the SDK's ZLL convention.
    pub source_profile: u16,
    pub clusters: &'static [Cluster],
}

include!("device_type/generated.rs");

/// Returns all indexed variants. An empty slice preserves an unknown identity.
pub fn lookup(profile: u16, device: u16) -> &'static [DeviceType] {
    let key = (profile, device);
    let start = ALL.partition_point(|row| (row.profile, row.device) < key);
    let count = ALL[start..].partition_point(|row| (row.profile, row.device) == key);
    &ALL[start..start + count]
}

/// Whether the catalogue covers this application profile, independent of device type.
pub fn has_profile(profile: u16) -> bool {
    ALL.binary_search_by_key(&profile, |row| row.profile).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_scoping_preserves_roles_and_revision_specific_variants() {
        let ha = lookup(0x0104, 0x0000);
        assert_eq!(ha.len(), 2);
        let on_off = |d: &DeviceType| *d.clusters.iter().find(|c| c.id == 6).unwrap();
        assert_eq!(on_off(&ha[0]).client, Role::Required);
        assert_eq!(on_off(&ha[1]).server, Role::Required);
        assert_eq!(
            lookup(0x0104, 0x0220)[0].template,
            lookup(0xc05e, 0x0220)[0].template
        );
        let zll = &lookup(0xc05e, 0x0000)[0];
        assert_eq!(zll.source_profile, 0x0104);
        assert_eq!(on_off(zll).server, Role::Required);
        assert!(lookup(0x0109, 8).len() > 1);
        assert!(lookup(0x0104, 0x0307)[0].clusters.is_empty());
        assert!(lookup(0xffff, 0xffff).is_empty());
        for field in ALL.windows(2) {
            assert!((field[0].profile, field[0].device) <= (field[1].profile, field[1].device));
        }
        assert_eq!(SDK_EXCEPTIONS.len(), 4);
        assert_eq!(lookup(0x0105, 0x000a)[0].name, "Constructed BACnet Device");
        assert_eq!(lookup(0x0105, 0x000b)[0].name, "BACnet Tunneled Device");
        let lock = &lookup(0x0104, 0x000b)[0];
        assert_eq!(
            lock.clusters.iter().find(|c| c.id == 10).unwrap().server,
            Role::Conflicting
        );
    }
}
