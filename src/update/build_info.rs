use serde::{Deserialize, Serialize};

use super::contract::{DURABLE_CONTRACT_EPOCH, HELPER_PROTOCOL, INSTALL_CONTRACT, UPDATE_PROTOCOL};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BuildInfo {
    pub version: String,
    pub target: String,
    pub source_commit: String,
    pub official_release: bool,
    pub update_protocol: u32,
    pub helper_protocol: u32,
    pub install_contract: String,
    pub durable_contract_epoch: u32,
    pub runtime_database_format: u32,
    pub cache_snapshot_format: u32,
    pub cache_semantics: u32,
}

impl BuildInfo {
    pub fn current() -> Self {
        Self {
            version: env!("CARGO_PKG_VERSION").into(),
            target: env!("PARINS_BUILD_TARGET").into(),
            source_commit: env!("PARINS_SOURCE_COMMIT").into(),
            official_release: env!("PARINS_OFFICIAL") == "true",
            update_protocol: UPDATE_PROTOCOL,
            helper_protocol: HELPER_PROTOCOL,
            install_contract: INSTALL_CONTRACT.into(),
            durable_contract_epoch: DURABLE_CONTRACT_EPOCH,
            runtime_database_format: crate::storage::DATABASE_FORMAT,
            cache_snapshot_format: crate::cache::persistence::VERSION,
            cache_semantics: crate::cache::persistence::CACHE_SEMANTICS,
        }
    }
}
