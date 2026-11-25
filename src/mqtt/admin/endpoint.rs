//! Named ZCL operations on one interviewed server endpoint.
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EndpointRequest {
    pub id: String,
    pub endpoint: u8,
    pub command: EndpointCommand,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum EndpointCommand {
    GroupList {},
    GroupView {
        group: u16,
    },
    GroupAdd {
        group: u16,
        #[serde(default)]
        name: String,
    },
    GroupRemove {
        group: u16,
    },
    SceneList {
        group: u16,
    },
    SceneView {
        group: u16,
        scene: u8,
    },
    SceneStore {
        group: u16,
        scene: u8,
    },
    SceneRecall {
        group: u16,
        scene: u8,
    },
    SceneRemove {
        group: u16,
        scene: u8,
    },
    AlarmPop {},
    AlarmReset {
        cluster: u16,
        code: u8,
    },
    AlarmResetAll {},
    AlarmClearLog {},
}

impl EndpointCommand {
    pub(super) fn read_only(&self) -> bool {
        matches!(
            self,
            Self::GroupList {}
                | Self::GroupView { .. }
                | Self::SceneList { .. }
                | Self::SceneView { .. }
        )
    }
}
