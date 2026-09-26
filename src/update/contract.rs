//! Pure shared release contracts. Neither parsing nor validation performs I/O.
use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};

use super::build_info::BuildInfo;

pub const REPOSITORY: &str = "paricafe/PariNS";
pub const UPDATE_PROTOCOL: u32 = 1;
pub const HELPER_PROTOCOL: u32 = 1;
pub const INSTALL_CONTRACT: &str = "linux-managed-updater-v1";
// This is the complete bidirectional state/TOML/database/payload contract, not
// merely a database schema version. Pre-updater 0.1.4 has no epoch or helper.
pub const DURABLE_CONTRACT_EPOCH: u32 = 1;
pub const MAX_BINARY_BYTES: u64 = 128 * 1024 * 1024;
pub const SUPPORTED_TARGETS: [&str; 2] =
    ["x86_64-unknown-linux-musl", "aarch64-unknown-linux-musl"];

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(pub u64, pub u64, pub u64);

impl Version {
    pub fn parse(value: &str) -> Result<Self, ContractError> {
        value.parse()
    }
    pub fn parse_tag(value: &str) -> Result<Self, ContractError> {
        value
            .strip_prefix('v')
            .ok_or(ContractError::InvalidVersion)?
            .parse()
    }
}

impl FromStr for Version {
    type Err = ContractError;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let mut parts = value.split('.');
        let mut component = || {
            let text = parts.next().ok_or(ContractError::InvalidVersion)?;
            if text.is_empty()
                || text.len() > 20
                || (text.len() > 1 && text.starts_with('0'))
                || !text.bytes().all(|b| b.is_ascii_digit())
            {
                return Err(ContractError::InvalidVersion);
            }
            text.parse().map_err(|_| ContractError::InvalidVersion)
        };
        let version = Self(component()?, component()?, component()?);
        if parts.next().is_some() {
            return Err(ContractError::InvalidVersion);
        }
        Ok(version)
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContractError {
    InvalidVersion,
    InvalidManifest,
    ReleaseIncomplete,
    ReleaseChanged,
    ManualRequired,
    NotNewer,
}
impl fmt::Display for ContractError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Self::InvalidVersion => "invalid_version",
                Self::InvalidManifest => "invalid_manifest",
                Self::ReleaseIncomplete => "release_incomplete",
                Self::ReleaseChanged => "release_changed",
                Self::ManualRequired => "manual_upgrade_required",
                Self::NotNewer => "not_newer",
            }
        )
    }
}
impl std::error::Error for ContractError {}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UpgradeMode {
    InPlace,
    Manual,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub target: String,
    pub name: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub schema: u32,
    pub repository: String,
    pub version: String,
    pub tag: String,
    pub source_commit: String,
    pub update_protocol: u32,
    pub install_contract: String,
    pub min_helper_protocol: u32,
    pub durable_contract_epoch: u32,
    pub runtime_database_format: u32,
    pub cache_snapshot_format: u32,
    pub cache_semantics: u32,
    pub upgrade_mode: UpgradeMode,
    pub artifacts: Vec<Artifact>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReleaseAsset {
    pub id: u64,
    pub name: String,
    pub state: String,
    pub size: u64,
    pub digest: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct GithubRelease {
    pub id: u64,
    pub tag_name: String,
    pub draft: bool,
    pub prerelease: bool,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub published_at: Option<String>,
    pub assets: Vec<ReleaseAsset>,
}

pub fn valid_sha256(value: &str) -> bool {
    valid_hex(value, 64)
}
fn valid_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

impl Manifest {
    pub fn validate(&self) -> Result<(), ContractError> {
        let version = Version::parse(&self.version)?;
        if self.schema != 1
            || self.repository != REPOSITORY
            || Version::parse_tag(&self.tag)? != version
            || !valid_hex(&self.source_commit, 40)
            || self.update_protocol == 0
            || self.install_contract != INSTALL_CONTRACT
            || self.min_helper_protocol == 0
            || self.durable_contract_epoch == 0
            || self.runtime_database_format == 0
            || self.cache_snapshot_format == 0
            || self.cache_semantics == 0
            || self.artifacts.len() != 2
        {
            return Err(ContractError::InvalidManifest);
        }
        for target in SUPPORTED_TARGETS {
            let mut found = self.artifacts.iter().filter(|a| a.target == target);
            let artifact = found.next().ok_or(ContractError::ReleaseIncomplete)?;
            if found.next().is_some() {
                return Err(ContractError::ReleaseIncomplete);
            }
            let arch = target.split('-').next().expect("fixed target");
            if artifact.name != format!("parins-{}-linux-{arch}.bin", self.tag)
                || !(64..=MAX_BINARY_BYTES).contains(&artifact.size)
                || !valid_sha256(&artifact.sha256)
            {
                return Err(ContractError::InvalidManifest);
            }
        }
        Ok(())
    }

    pub fn artifact_for(&self, target: &str) -> Result<&Artifact, ContractError> {
        self.artifacts
            .iter()
            .find(|a| a.target == target)
            .ok_or(ContractError::ManualRequired)
    }

    pub fn validate_release(&self, release: &GithubRelease) -> Result<(), ContractError> {
        self.validate()?;
        if release.id == 0 || release.draft || release.prerelease || release.tag_name != self.tag {
            return Err(ContractError::ReleaseChanged);
        }
        let mut ids = std::collections::HashSet::new();
        let mut names = std::collections::HashSet::new();
        for asset in &release.assets {
            if asset.id == 0
                || !ids.insert(asset.id)
                || !names.insert(&asset.name)
                || asset.state != "uploaded"
            {
                return Err(ContractError::ReleaseIncomplete);
            }
        }
        let manifest = release
            .assets
            .iter()
            .find(|a| a.name == "parins-update.json")
            .ok_or(ContractError::ReleaseIncomplete)?;
        if manifest.size == 0 || manifest.size > 64 * 1024 {
            return Err(ContractError::ReleaseIncomplete);
        }
        for artifact in &self.artifacts {
            let asset = release
                .assets
                .iter()
                .find(|a| a.name == artifact.name)
                .ok_or(ContractError::ReleaseIncomplete)?;
            if asset.size != artifact.size
                || asset
                    .digest
                    .as_ref()
                    .is_some_and(|digest| digest != &format!("sha256:{}", artifact.sha256))
            {
                return Err(ContractError::ReleaseChanged);
            }
        }
        Ok(())
    }

    pub fn check_compatible(
        &self,
        current: &BuildInfo,
        helper_protocol: u32,
    ) -> Result<(), ContractError> {
        self.validate()?;
        if Version::parse(&self.version)? <= Version::parse(&current.version)? {
            return Err(ContractError::NotNewer);
        }
        if !current.official_release
            || self.upgrade_mode != UpgradeMode::InPlace
            || self.update_protocol != current.update_protocol
            || self.install_contract != current.install_contract
            || self.min_helper_protocol > helper_protocol
            || self.durable_contract_epoch != current.durable_contract_epoch
            || self.runtime_database_format != current.runtime_database_format
        {
            return Err(ContractError::ManualRequired);
        }
        self.artifact_for(&current.target)?;
        Ok(())
    }

    pub fn matches_build(&self, build: &BuildInfo) -> bool {
        build.official_release
            && self.version == build.version
            && self.source_commit == build.source_commit
            && self.update_protocol == build.update_protocol
            && self.install_contract == build.install_contract
            && self.min_helper_protocol <= build.helper_protocol
            && self.durable_contract_epoch == build.durable_contract_epoch
            && self.runtime_database_format == build.runtime_database_format
            && self.cache_snapshot_format == build.cache_snapshot_format
            && self.cache_semantics == build.cache_semantics
            && self.artifact_for(&build.target).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (Manifest, BuildInfo, GithubRelease) {
        let mut build = BuildInfo::current();
        build.version = "0.1.9".into();
        build.target = SUPPORTED_TARGETS[0].into();
        build.official_release = true;
        let manifest = Manifest {
            schema: 1,
            repository: REPOSITORY.into(),
            version: "0.1.10".into(),
            tag: "v0.1.10".into(),
            source_commit: "a".repeat(40),
            update_protocol: UPDATE_PROTOCOL,
            install_contract: INSTALL_CONTRACT.into(),
            min_helper_protocol: HELPER_PROTOCOL,
            durable_contract_epoch: DURABLE_CONTRACT_EPOCH,
            runtime_database_format: build.runtime_database_format,
            cache_snapshot_format: build.cache_snapshot_format,
            cache_semantics: build.cache_semantics,
            upgrade_mode: UpgradeMode::InPlace,
            artifacts: SUPPORTED_TARGETS
                .iter()
                .map(|target| Artifact {
                    target: (*target).into(),
                    name: format!(
                        "parins-v0.1.10-linux-{}.bin",
                        target.split('-').next().unwrap()
                    ),
                    size: 100,
                    sha256: "b".repeat(64),
                })
                .collect(),
        };
        let mut release = GithubRelease {
            id: 7,
            tag_name: manifest.tag.clone(),
            draft: false,
            prerelease: false,
            body: None,
            published_at: None,
            assets: vec![ReleaseAsset {
                id: 1,
                name: "parins-update.json".into(),
                state: "uploaded".into(),
                size: 1000,
                digest: None,
            }],
        };
        release.assets.extend(
            manifest
                .artifacts
                .iter()
                .enumerate()
                .map(|(i, a)| ReleaseAsset {
                    id: i as u64 + 2,
                    name: a.name.clone(),
                    state: "uploaded".into(),
                    size: a.size,
                    digest: Some(format!("sha256:{}", a.sha256)),
                }),
        );
        (manifest, build, release)
    }

    #[test]
    fn strict_numeric_versions() {
        assert!(Version::parse_tag("v0.1.10").unwrap() > Version::parse_tag("v0.1.9").unwrap());
        for invalid in [
            "1.2",
            "1.2.3.4",
            "01.2.3",
            "1.02.3",
            "1.2.3-rc1",
            "1.2.3+abc",
            "1.2.3\n",
            "-1.2.3",
            "18446744073709551616.1.0",
        ] {
            assert!(Version::parse(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn manifest_and_release_complete_exact_and_bound() {
        let (manifest, _, release) = fixture();
        assert!(manifest.validate_release(&release).is_ok());
        for mutate in [0, 1, 2, 3, 4, 5, 6] {
            let mut bad = release.clone();
            match mutate {
                0 => {
                    bad.assets.pop();
                }
                1 => bad.assets.push(bad.assets[0].clone()),
                2 => bad.assets[1].size += 1,
                3 => bad.assets[1].digest = Some(format!("sha256:{}", "c".repeat(64))),
                4 => bad.assets[1].state = "new".into(),
                5 => bad.prerelease = true,
                _ => bad.tag_name = "v0.1.11".into(),
            }
            assert!(manifest.validate_release(&bad).is_err());
        }
        for mutate in 0..7 {
            let mut bad = manifest.clone();
            match mutate {
                0 => bad.schema = 2,
                1 => bad.artifacts[1] = bad.artifacts[0].clone(),
                2 => bad.artifacts[0].sha256 = "z".repeat(64),
                3 => bad.artifacts[0].size = MAX_BINARY_BYTES + 1,
                4 => bad.source_commit = "main".into(),
                5 => bad.artifacts[0].name = "../candidate".into(),
                _ => bad.repository = "other/PariNS".into(),
            }
            assert!(bad.validate().is_err());
        }
        let mut value = serde_json::to_value(&manifest).unwrap();
        value["url"] = "https://example.com".into();
        assert!(serde_json::from_value::<Manifest>(value).is_err());
    }

    #[test]
    fn whole_durable_contract_not_cache_format_gates_rollback() {
        let (mut manifest, mut build, _) = fixture();
        assert!(manifest.check_compatible(&build, HELPER_PROTOCOL).is_ok());
        manifest.cache_snapshot_format += 1;
        assert!(manifest.check_compatible(&build, HELPER_PROTOCOL).is_ok());
        manifest.durable_contract_epoch += 1;
        assert_eq!(
            manifest.check_compatible(&build, HELPER_PROTOCOL),
            Err(ContractError::ManualRequired)
        );
        manifest.durable_contract_epoch -= 1;
        build.official_release = false;
        assert_eq!(
            manifest.check_compatible(&build, HELPER_PROTOCOL),
            Err(ContractError::ManualRequired)
        );
        build.official_release = true;
        build.version = manifest.version.clone();
        assert_eq!(
            manifest.check_compatible(&build, HELPER_PROTOCOL),
            Err(ContractError::NotNewer)
        );
        build.version = "0.1.11".into();
        assert_eq!(
            manifest.check_compatible(&build, HELPER_PROTOCOL),
            Err(ContractError::NotNewer)
        );
        build.version = "0.1.9".into();
        manifest.upgrade_mode = UpgradeMode::Manual;
        assert_eq!(
            manifest.check_compatible(&build, HELPER_PROTOCOL),
            Err(ContractError::ManualRequired)
        );
    }

    #[test]
    fn candidate_build_identity_must_match_manifest() {
        let (manifest, mut build, _) = fixture();
        build.version = manifest.version.clone();
        build.source_commit = manifest.source_commit.clone();
        assert!(manifest.matches_build(&build));
        for mutation in 0..5 {
            let mut bad = build.clone();
            match mutation {
                0 => bad.source_commit = "c".repeat(40),
                1 => bad.official_release = false,
                2 => bad.durable_contract_epoch += 1,
                3 => bad.helper_protocol = 0,
                _ => bad.cache_snapshot_format += 1,
            }
            assert!(!manifest.matches_build(&bad));
        }
    }
}
