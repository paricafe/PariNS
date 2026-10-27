//! Immutable filtering policy and bounded local rule-file loading.

use std::{
    collections::{HashMap, HashSet},
    fs::File,
    io::Read,
    net::{Ipv4Addr, Ipv6Addr},
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, ensure};
use canonical::{Builder, Group, Limits};
use hickory_proto::{
    op::Message,
    rr::{Name, RData},
};
use response::Responses;
pub use response::{BlockMode, BlockResponse};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, Deserialize)]
#[serde(try_from = "Rules")]
pub struct Policy {
    data: Arc<Data>,
}

#[derive(Debug)]
struct Data {
    index: radix::Index,
    publication_digest: [u8; 32],
    source_ids: Vec<String>,
    responses: Responses,
    local: Option<LocalRules>,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
struct Rules {
    enabled: bool,
    block_response: BlockMode,
    block_ipv4: Option<Ipv4Addr>,
    block_ipv6: Option<Ipv6Addr>,
    blocked_ttl_secs: u32,
    block_exact: Vec<String>,
    block_suffix: Vec<String>,
    allow_exact: Vec<String>,
    allow_suffix: Vec<String>,
}
impl Default for Rules {
    fn default() -> Self {
        Self {
            enabled: false,
            block_response: BlockMode::default(),
            block_ipv4: None,
            block_ipv6: None,
            blocked_ttl_secs: response::DEFAULT_TTL_SECS,
            block_exact: Vec::new(),
            block_suffix: Vec::new(),
            allow_exact: Vec::new(),
            allow_suffix: Vec::new(),
        }
    }
}

/// Distinct per-rule block methods in local rules, each with its own index slot.
pub(crate) const MAX_LOCAL_RESPONSES: usize = 1024;

/// Configuration owns validated source rules, never a prematurely compiled
/// index. The subscription worker owns candidate compilation and its budget.
#[derive(Clone, Debug, Deserialize)]
#[serde(try_from = "Rules")]
pub struct LocalRules {
    rules: Arc<Rules>,
    global: BlockResponse,
    /// Per-rule methods in first-declaration order; slot `i + 1` is `variants[i]`.
    variants: Arc<[BlockResponse]>,
    /// Every rule line and setting, which decide each rule's slot and method.
    digest: [u8; 32],
}

/// A rule line is a domain, then for block rules an optional method.
fn split_rule(rule: &str, group: Group) -> Result<(&str, Option<BlockResponse>)> {
    ensure!(rule.trim() == rule, "leading or trailing whitespace");
    let mut tokens = rule.split_whitespace();
    let domain = tokens.next().context("empty rule")?;
    let method: Vec<_> = tokens.collect();
    if method.is_empty() {
        return Ok((domain, None));
    }
    ensure!(
        matches!(group, Group::BlockExact | Group::BlockSuffix),
        "allow rules take no block method"
    );
    Ok((domain, Some(BlockResponse::parse_rule(&method)?)))
}
impl Default for LocalRules {
    fn default() -> Self {
        Self::try_from(Rules::default()).expect("empty local rules")
    }
}
impl TryFrom<Rules> for LocalRules {
    type Error = anyhow::Error;
    fn try_from(rules: Rules) -> Result<Self> {
        Self::new(rules, true)
    }
}
impl LocalRules {
    /// `quote` names offending rule text in errors. A rule file can be any
    /// local file, so its errors name only the rule's position. Reasons are
    /// fixed program text and never repeat rule content.
    fn new(rules: Rules, quote: bool) -> Result<Self> {
        ensure!(
            rules
                .groups()
                .iter()
                .map(|(g, _, _)| g.len())
                .sum::<usize>()
                <= 100_000,
            "filter exceeds 100000 rules"
        );
        ensure!(
            rules.bytes() <= 8 * 1024 * 1024,
            "filter exceeds 8 MiB of rule text"
        );
        // Serde shows only the message, so each one carries its cause.
        let global =
            BlockResponse::from_settings(rules.block_response, rules.block_ipv4, rules.block_ipv6)
                .map_err(|error| anyhow::anyhow!("invalid filter block_response: {error}"))?;
        ensure!(
            (1..=response::MAX_TTL_SECS).contains(&rules.blocked_ttl_secs),
            "filter.blocked_ttl_secs must be in 1..=86400"
        );
        let mut variants = Vec::new();
        for (group, kind, list) in rules.groups() {
            for (position, rule) in group.iter().enumerate() {
                let invalid = |reason: &dyn std::fmt::Display| {
                    if quote {
                        anyhow::anyhow!("filter.{list}[{position}] {rule:?}: {reason}")
                    } else {
                        anyhow::anyhow!("filter file {list}[{position}]: {reason}")
                    }
                };
                let (domain, method) = split_rule(rule, kind).map_err(|error| invalid(&error))?;
                canonical::Key::rule(domain)
                    .map_err(|_| invalid(&"expected ASCII DNS labels, not rule syntax"))?;
                if let Some(method) = method
                    && !variants.contains(&method)
                {
                    ensure!(
                        variants.len() < MAX_LOCAL_RESPONSES,
                        "filter has more than {MAX_LOCAL_RESPONSES} distinct block methods"
                    );
                    variants.push(method);
                }
            }
        }
        let mut hash = Sha256::new();
        hash.update(b"parins-local-rules-v1\0");
        hash.update([u8::from(rules.enabled)]);
        global.digest(&mut hash);
        hash.update(rules.blocked_ttl_secs.to_be_bytes());
        for (group, _, list) in rules.groups() {
            hash.update(list.as_bytes());
            hash.update((group.len() as u64).to_be_bytes());
            for rule in group {
                hash.update((rule.len() as u64).to_be_bytes());
                hash.update(rule.as_bytes());
            }
        }
        Ok(Self {
            rules: Arc::new(rules),
            global,
            variants: variants.into(),
            digest: hash.finalize().into(),
        })
    }
    fn owned_bytes(&self) -> usize {
        self.rules.owned_bytes()
            + std::mem::size_of::<Rules>()
            + 2 * std::mem::size_of::<usize>()
            + std::mem::size_of_val(&*self.variants)
    }
    fn local_slots(&self) -> u16 {
        1 + self.variants.len() as u16
    }
    fn responses(&self) -> Responses {
        Responses {
            global: self.global,
            ttl: self.rules.blocked_ttl_secs,
            local_slots: self.local_slots(),
            slots: std::iter::once(None)
                .chain(self.variants.iter().copied().map(Some))
                .collect(),
        }
    }
    /// Adds every enabled rule under the slot of its method.
    fn add_to(&self, builder: &mut Builder, mut check: impl FnMut() -> Result<()>) -> Result<()> {
        builder.local_slots = self.local_slots();
        builder.classes = self.responses().classes();
        if !self.rules.enabled {
            return Ok(());
        }
        for (group, kind, _) in self.rules.groups() {
            for (position, rule) in group.iter().enumerate() {
                if position.is_multiple_of(4096) {
                    check()?;
                }
                let (domain, method) = split_rule(rule, kind).expect("validated filter rule");
                let slot = method.map_or(0, |method| {
                    1 + self
                        .variants
                        .iter()
                        .position(|variant| *variant == method)
                        .expect("collected block method") as u16
                });
                builder.add(domain, kind, slot)?;
            }
        }
        Ok(())
    }
    fn compile(&self, mut limits: Limits, mut check: impl FnMut() -> Result<()>) -> Result<Policy> {
        check()?;
        limits.retained_bytes = limits
            .retained_bytes
            .checked_add(self.owned_bytes())
            .context("subscription_memory_limit")?;
        let mut builder = Builder::new(limits)?;
        self.add_to(&mut builder, &mut check)?;
        let mut cancelled = || {
            check().map_err(|_| canonical::Error {
                line: 0,
                kind: canonical::ErrorKind::Cancelled,
            })
        };
        let index = builder
            .prepare_with_check(&mut cancelled)?
            .finish_radix_with_check(cancelled)?;
        let source_ids = vec![String::new(); usize::from(self.local_slots())];
        Ok(Policy::new(
            index,
            source_ids,
            self.responses(),
            Some(self.clone()),
        ))
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
                let rules = toml::from_str(text).map_err(|error| file_error(text, &error))?;
                LocalRules::new(rules, false)?.compile(
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

/// A rule file's TOML error, reduced to its position and a fixed kind: the
/// parser's own message can quote the file, which may be any local file.
fn file_error(text: &str, error: &toml::de::Error) -> anyhow::Error {
    let kind = match toml::from_str::<toml::Table>(text) {
        Err(_) => "TOML syntax error",
        Ok(_) => "unknown key or invalid value",
    };
    let Some(before) = error.span().and_then(|span| text.get(..span.start)) else {
        return anyhow::anyhow!("invalid filter file: {kind}");
    };
    let line = before.matches('\n').count() + 1;
    let column = before
        .rsplit('\n')
        .next()
        .unwrap_or_default()
        .chars()
        .count()
        + 1;
    anyhow::anyhow!("invalid filter file: {kind} at line {line}, column {column}")
}

impl Rules {
    fn groups(&self) -> [(&Vec<String>, Group, &'static str); 4] {
        [
            (&self.block_exact, Group::BlockExact, "block_exact"),
            (&self.block_suffix, Group::BlockSuffix, "block_suffix"),
            (&self.allow_exact, Group::AllowExact, "allow_exact"),
            (&self.allow_suffix, Group::AllowSuffix, "allow_suffix"),
        ]
    }
    fn bytes(&self) -> usize {
        self.groups()
            .iter()
            .flat_map(|(g, _, _)| g.iter())
            .map(String::len)
            .sum()
    }
    fn owned_bytes(&self) -> usize {
        self.groups()
            .iter()
            .map(|(g, _, _)| {
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
/// The rule that blocked a query and the method its answer used, for the query log.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct BlockTrace {
    pub rule: String,
    /// null identifies local rules.
    pub source_id: Option<String>,
    pub response: String,
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
        self.data.index.semantic_digest
    }
    fn new(
        index: radix::Index,
        source_ids: Vec<String>,
        responses: Responses,
        local: Option<LocalRules>,
    ) -> Self {
        // Each slot's witness is its source and the answer it actually gives.
        let witnesses: Vec<_> = source_ids
            .iter()
            .enumerate()
            .map(|(slot, id)| format!("{id}\0{}", responses.get(slot as u16).label()))
            .collect();
        let mut hash = Sha256::new();
        hash.update(index.publication_digest(&witnesses));
        hash.update(responses.ttl.to_be_bytes());
        Self {
            data: Arc::new(Data {
                index,
                publication_digest: hash.finalize().into(),
                source_ids,
                responses,
                local,
            }),
        }
    }
    /// Slot IDs of an aggregate on this local policy: its local slots, then
    /// one slot per subscription source in the given order.
    pub(crate) fn aggregate_ids(&self, sources: impl IntoIterator<Item = String>) -> Vec<String> {
        vec![String::new(); usize::from(self.data.responses.local_slots)]
            .into_iter()
            .chain(sources)
            .collect()
    }
    /// An aggregate index built on this local policy; `sources` holds each
    /// subscription source's method in slot order (None follows the global one).
    pub(crate) fn aggregate(
        &self,
        index: radix::Index,
        source_ids: Vec<String>,
        sources: impl IntoIterator<Item = Option<BlockResponse>>,
    ) -> Self {
        let responses = self.aggregate_responses(sources);
        debug_assert_eq!(responses.slots.len(), source_ids.len());
        Self::new(index, source_ids, responses, None)
    }
    fn aggregate_responses(
        &self,
        sources: impl IntoIterator<Item = Option<BlockResponse>>,
    ) -> Responses {
        let mut responses = self.data.responses.clone();
        responses.slots.truncate(usize::from(responses.local_slots));
        responses.slots.extend(sources);
        responses
    }
    /// Covers every block method of an aggregate on this local policy, so a
    /// method change publishes a new generation.
    pub(crate) fn aggregate_responses_digest(
        &self,
        sources: impl IntoIterator<Item = Option<BlockResponse>>,
    ) -> [u8; 32] {
        self.aggregate_responses(sources).digest()
    }
    /// Answer classes of an aggregate's slots; they decide which covered rules
    /// the index keeps, so they name the derived index too.
    pub(crate) fn aggregate_classes(
        &self,
        sources: impl IntoIterator<Item = Option<BlockResponse>>,
    ) -> Vec<u16> {
        self.aggregate_responses(sources).classes()
    }
    /// Digest of the local rule text and settings; empty for an aggregate.
    pub(crate) fn local_digest(&self) -> [u8; 32] {
        self.data
            .local
            .as_ref()
            .map_or([0; 32], |local| local.digest)
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
        if local.rules.enabled {
            builder.account_local_text(local.rules.bytes())?;
        }
        local.add_to(builder, || Ok(()))
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
        self.data
            .index
            .lookup(name)
            .is_some_and(|matched| (matched.group as u8) >= 2)
    }

    /// The answer for a blocked `name`, chosen by the most specific rule:
    /// local exact, local suffix (longest), subscription exact, subscription
    /// suffix (longest); sources in configured order.
    fn block_answer(&self, query: &Message, name: &Name) -> Option<(Message, BlockTrace)> {
        let responses = &self.data.responses;
        let found = self.data.index.block_match(name, responses.local_slots)?;
        let response = responses.get(found.source_slot);
        let rule = self.data.index.rule(name, found);
        // The SOA owner is the question name or an ancestor: the rule's name
        // when it matched the question, else the question name itself.
        let qname = query.queries[0].name();
        let owner = if name == qname {
            Name::from_ascii(format!("{rule}.")).expect("matched rules are DNS names")
        } else {
            qname.clone()
        };
        let id = &self.data.source_ids[usize::from(found.source_slot)];
        Some((
            response.answer(query, &owner, responses.ttl),
            BlockTrace {
                rule,
                source_id: (!id.is_empty()).then(|| id.clone()),
                response: response.kind().into(),
            },
        ))
    }

    pub fn block_query(&self, query: &Message) -> Option<(Message, BlockTrace)> {
        self.block_answer(query, query.queries[0].name())
    }

    /// Only follow answer CNAMEs reachable from the original question and class.
    /// A name's allow exception never skips checks on a different chain target.
    /// A blocked chain name replaces the whole answer with that rule's method.
    pub fn apply_response(&self, query: &Message, response: &mut Message) -> Option<BlockTrace> {
        if self.index_rules() == 0 {
            return None;
        }
        let question = &query.queries[0];
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
            if let Some((answer, trace)) = self.block_answer(query, name) {
                *response = answer;
                return Some(trace);
            }
            if let Some(targets) = links.get(name) {
                pending.extend(targets.iter().copied());
            }
        }
        None
    }
}

#[cfg(test)]
mod tests;

pub(crate) mod canonical;
mod response;

pub(crate) mod radix;
