//! Immutable filtering policy and bounded local rule-file loading.
//! No cache ownership or upstream state.

use std::{
    collections::{HashMap, HashSet},
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, ensure};
use canonical::{Builder, Group, Limits};
use hickory_proto::{
    op::{Message, ResponseCode},
    rr::{Name, RData},
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize)]
#[serde(try_from = "Rules")]
pub struct Policy {
    data: Arc<Data>,
    #[cfg(test)]
    wire: Option<Arc<wire::Injection>>,
}

#[derive(Debug)]
struct Data {
    index: radix::Index,
    publication_digest: [u8; 32],
    source_ids: Vec<String>,
    local: Option<LocalRules>,
}

#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
struct Rules {
    enabled: bool,
    block_exact: Vec<String>,
    block_suffix: Vec<String>,
    allow_exact: Vec<String>,
    allow_suffix: Vec<String>,
}

/// Configuration owns validated source rules, never a prematurely compiled
/// index. The subscription worker owns candidate compilation and its budget.
#[derive(Clone, Debug, Deserialize)]
#[serde(try_from = "Rules")]
pub struct LocalRules {
    rules: Arc<Rules>,
    #[cfg(test)]
    injected: Option<Policy>,
}
impl Default for LocalRules {
    fn default() -> Self {
        Self::try_from(Rules::default()).expect("empty local rules")
    }
}
impl TryFrom<Rules> for LocalRules {
    type Error = anyhow::Error;
    fn try_from(rules: Rules) -> Result<Self> {
        ensure!(
            rules.groups().iter().map(|(g, _)| g.len()).sum::<usize>() <= 100_000,
            "filter exceeds 100000 rules"
        );
        ensure!(
            rules.bytes() <= 8 * 1024 * 1024,
            "filter exceeds 8 MiB of rule text"
        );
        for (group, _) in rules.groups() {
            for rule in group {
                canonical::Key::rule(rule).with_context(|| {
                    format!(
                        "invalid filter domain {rule:?}: expected ASCII DNS labels, not rule syntax"
                    )
                })?;
            }
        }
        Ok(Self {
            rules: Arc::new(rules),
            #[cfg(test)]
            injected: None,
        })
    }
}
impl LocalRules {
    fn owned_bytes(&self) -> usize {
        self.rules.owned_bytes() + std::mem::size_of::<Rules>() + 2 * std::mem::size_of::<usize>()
    }
    fn compile(&self, mut limits: Limits, mut check: impl FnMut() -> Result<()>) -> Result<Policy> {
        check()?;
        #[cfg(test)]
        if let Some(policy) = &self.injected {
            return Ok(policy.clone());
        }
        limits.retained_bytes = limits
            .retained_bytes
            .checked_add(self.owned_bytes())
            .context("subscription_memory_limit")?;
        let mut builder = Builder::new(limits)?;
        if self.rules.enabled {
            for (group, kind) in self.rules.groups() {
                for (position, rule) in group.iter().enumerate() {
                    if position.is_multiple_of(4096) {
                        check()?;
                    }
                    builder.add(rule, kind, 0)?;
                }
            }
        }
        let mut cancelled = || {
            check().map_err(|_| canonical::Error {
                line: 0,
                kind: canonical::ErrorKind::Cancelled,
            })
        };
        let index = builder
            .prepare_with_check(&mut cancelled)?
            .finish_radix_with_check(cancelled)?;
        let source_ids = vec![String::new()];
        let publication_digest = index.publication_digest(&source_ids);
        Ok(Policy {
            data: Arc::new(Data {
                index,
                publication_digest,
                source_ids,
                local: Some(self.clone()),
            }),
            #[cfg(test)]
            wire: None,
        })
    }
}
#[cfg(test)]
impl From<Policy> for LocalRules {
    fn from(policy: Policy) -> Self {
        Self {
            injected: Some(policy),
            ..Self::default()
        }
    }
}

#[derive(Clone, Debug)]
pub enum LocalPolicySource {
    Inline(LocalRules),
    File(PathBuf),
}
impl LocalPolicySource {
    pub(crate) fn reuse_policy(&self, policy: &Policy) -> Option<Policy> {
        match (self, &policy.data.local) {
            (Self::Inline(source), Some(local)) if source.rules == local.rules => {
                Some(policy.clone())
            }
            _ => None,
        }
    }
    pub(crate) fn compile(
        &self,
        limits: Limits,
        mut check: impl FnMut() -> Result<()>,
    ) -> Result<Policy> {
        match self {
            Self::Inline(rules) => rules.compile(limits, check),
            Self::File(path) => {
                check()?;
                const MAX: usize = 8 * 1024 * 1024;
                let mut file = File::open(path).context("open filter file")?;
                let metadata = file.metadata()?;
                ensure!(metadata.is_file(), "filter file must be a regular file");
                let length = usize::try_from(metadata.len())?;
                ensure!(length <= MAX, "filter file exceeds 8 MiB");
                // Reserve the bounded input buffer before reading. Keep it
                // charged alongside parsed rules and the candidate index.
                let mut budget = Builder::new(limits)?.budget;
                let mut bytes = Vec::new();
                canonical::grow(&mut bytes, length, &mut budget)?;
                bytes.resize(length, 0);
                file.read_exact(&mut bytes)?;
                ensure!(
                    file.read(&mut [0])? == 0,
                    "filter file changed while reading"
                );
                check()?;
                let text = std::str::from_utf8(&bytes).context("filter file must be UTF-8")?;
                let rules: LocalRules = toml::from_str(text).context("invalid filter file")?;
                rules.compile(
                    Limits {
                        retained_bytes: budget.live,
                        ..limits
                    },
                    check,
                )
            }
        }
    }
}

impl Rules {
    fn groups(&self) -> [(&Vec<String>, Group); 4] {
        [
            (&self.block_exact, Group::BlockExact),
            (&self.block_suffix, Group::BlockSuffix),
            (&self.allow_exact, Group::AllowExact),
            (&self.allow_suffix, Group::AllowSuffix),
        ]
    }
    fn bytes(&self) -> usize {
        self.groups()
            .iter()
            .flat_map(|(g, _)| g.iter())
            .map(String::len)
            .sum()
    }
    fn owned_bytes(&self) -> usize {
        self.groups()
            .iter()
            .map(|(g, _)| {
                g.capacity() * std::mem::size_of::<String>()
                    + g.iter().map(String::capacity).sum::<usize>()
            })
            .sum()
    }
}

impl Default for Policy {
    fn default() -> Self {
        Self::try_from(Rules::default()).expect("empty local policy")
    }
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Allowed,
    Blocked,
    Unmatched,
}
#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Exact,
    Suffix,
}
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct Witness {
    /// null identifies local rules; subscription IDs are always nonempty.
    pub source_id: Option<String>,
    pub rule: String,
    pub scope: Scope,
}
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct Explanation {
    pub decision: Decision,
    pub witness: Option<Witness>,
}

impl TryFrom<Rules> for Policy {
    type Error = anyhow::Error;

    fn try_from(rules: Rules) -> Result<Self> {
        LocalRules::try_from(rules)?.compile(Limits::default(), || Ok(()))
    }
}

impl Policy {
    /// The effective-rule digest is calculated once during compilation.
    pub fn semantic_digest(&self) -> [u8; 32] {
        #[cfg(test)]
        if let Some(injection) = &self.wire {
            return injection.digest;
        }
        self.data.index.semantic_digest
    }
    pub(crate) fn from_index(index: radix::Index, source_ids: Vec<String>) -> Self {
        let publication_digest = index.publication_digest(&source_ids);
        Self {
            data: Arc::new(Data {
                index,
                publication_digest,
                source_ids,
                local: None,
            }),
            #[cfg(test)]
            wire: None,
        }
    }
    /// Stable effective decisions and their actual bounded witnesses, excluding
    /// unused sources and input-only statistics such as repeated source lines.
    pub(crate) fn publication_digest(&self) -> [u8; 32] {
        self.data.publication_digest
    }
    pub(crate) fn write_index(
        &self,
        writer: impl std::io::Write,
        input_digest: [u8; 32],
    ) -> Result<()> {
        self.data
            .index
            .write_to(writer, input_digest, &self.data.source_ids)
    }
    pub(crate) fn append_to(&self, builder: &mut Builder) -> Result<()> {
        let local = self
            .data
            .local
            .as_ref()
            .context("aggregate policy cannot be used as local input")?;
        let rules = &local.rules;
        if rules.enabled {
            builder.account_local_text(rules.bytes())?;
            for (group, kind) in rules.groups() {
                for domain in group {
                    builder.add(domain, kind, 0)?;
                }
            }
        }
        Ok(())
    }
    pub(crate) fn index_bytes(&self) -> usize {
        self.data.index.index_bytes()
    }
    pub(crate) fn local_bytes(&self) -> usize {
        self.data.local.as_ref().map_or(0, LocalRules::owned_bytes)
    }
    pub(crate) fn owned_bytes(&self) -> usize {
        self.index_bytes()
            + self.local_bytes()
            + std::mem::size_of::<Data>()
            + 2 * std::mem::size_of::<usize>()
            + self.data.source_ids.capacity() * std::mem::size_of::<String>()
            + self
                .data
                .source_ids
                .iter()
                .map(String::capacity)
                .sum::<usize>()
    }
    pub(crate) fn same_allocation(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.data, &other.data)
    }
    pub(crate) fn input_bytes(&self) -> usize {
        self.data
            .local
            .as_ref()
            .filter(|r| r.rules.enabled)
            .map_or(0, |r| r.rules.bytes())
    }
    pub(crate) fn input_rules(&self) -> usize {
        self.data.index.input_rules
    }
    pub(crate) fn index_rules(&self) -> usize {
        self.data.index.index_rules
    }
    pub fn explain(&self, name: &Name) -> Explanation {
        match self.data.index.lookup(name) {
            None => Explanation {
                decision: Decision::Unmatched,
                witness: None,
            },
            Some(found) => Explanation {
                decision: if (found.group as u8) < 2 {
                    Decision::Allowed
                } else {
                    Decision::Blocked
                },
                witness: Some(Witness {
                    source_id: {
                        let id = &self.data.source_ids[usize::from(found.source_slot)];
                        (!id.is_empty()).then(|| id.clone())
                    },
                    rule: self.data.index.rule(name, found),
                    scope: if (found.group as u8).is_multiple_of(2) {
                        Scope::Suffix
                    } else {
                        Scope::Exact
                    },
                }),
            },
        }
    }
    /// Parse and validate a complete replacement before the caller publishes it.
    pub fn load(path: &Path) -> Result<Self> {
        LocalPolicySource::File(path.to_owned()).compile(Limits::default(), || Ok(()))
    }

    pub fn blocks(&self, name: &Name) -> bool {
        #[cfg(test)]
        if let Some(injection) = &self.wire {
            return injection.blocks(name);
        }
        self.data
            .index
            .lookup(name)
            .is_some_and(|matched| (matched.group as u8) >= 2)
    }

    pub fn blocks_query(&self, query: &Message) -> bool {
        query.queries.first().is_some_and(|q| self.blocks(q.name()))
    }

    /// Only follow answer CNAMEs reachable from the original question and class.
    /// A name's allow exception never skips checks on a different chain target.
    pub fn apply_response(&self, query: &Message, response: &mut Message) -> bool {
        if self.index_rules() == 0 && !self.has_test_injection() {
            return false;
        }
        let Some(question) = query.queries.first() else {
            return false;
        };
        let mut links: HashMap<&Name, Vec<&Name>> = HashMap::new();
        for rr in &response.answers {
            if rr.dns_class == question.query_class()
                && let RData::CNAME(target) = &rr.data
            {
                links.entry(&rr.name).or_default().push(&target.0);
            }
        }
        let mut pending = vec![question.name()];
        let mut visited = HashSet::new();
        while let Some(name) = pending.pop() {
            if !visited.insert(name) {
                continue;
            }
            if self.blocks(name) {
                *response = crate::protocol::error_response(query, ResponseCode::NoError);
                return true;
            }
            if let Some(targets) = links.get(name) {
                pending.extend(targets.iter().copied());
            }
        }
        false
    }
    fn has_test_injection(&self) -> bool {
        #[cfg(test)]
        {
            self.wire.is_some()
        }
        #[cfg(not(test))]
        {
            false
        }
    }
}

#[cfg(test)]
mod tests;

#[allow(dead_code)]
pub(crate) mod canonical;

pub(crate) mod radix;

#[cfg(test)]
mod compact;

#[cfg(test)]
mod wire;
