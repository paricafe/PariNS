//! Immutable local filtering policy. No IO, cache ownership, or upstream state.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use anyhow::{Result, ensure};
use hickory_proto::{
    op::{Message, ResponseCode},
    rr::{Name, RData},
};
use serde::Deserialize;

#[derive(Clone, Debug, Default)]
struct Node {
    children: HashMap<Vec<u8>, Node>,
    block_exact: bool,
    block_suffix: bool,
    allow_exact: bool,
    allow_suffix: bool,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(try_from = "Rules")]
pub struct Policy {
    enabled: bool,
    root: Arc<Node>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Rules {
    enabled: bool,
    block_exact: Vec<String>,
    block_suffix: Vec<String>,
    allow_exact: Vec<String>,
    allow_suffix: Vec<String>,
}

impl TryFrom<Rules> for Policy {
    type Error = anyhow::Error;

    fn try_from(rules: Rules) -> Result<Self> {
        let groups = [
            (&rules.block_exact, false, false),
            (&rules.block_suffix, false, true),
            (&rules.allow_exact, true, false),
            (&rules.allow_suffix, true, true),
        ];
        ensure!(
            groups.iter().map(|(g, _, _)| g.len()).sum::<usize>() <= 100_000,
            "filter exceeds 100000 rules"
        );
        ensure!(
            groups
                .iter()
                .flat_map(|(g, _, _)| g.iter())
                .map(String::len)
                .sum::<usize>()
                <= 8 * 1024 * 1024,
            "filter exceeds 8 MiB of rule text"
        );
        let mut root = Node::default();
        for (group, allow, suffix) in groups {
            for rule in group {
                let domain = rule.strip_suffix('.').unwrap_or(rule);
                ensure!(
                    !domain.is_empty()
                        && domain.len() <= 253
                        && domain.split('.').all(|label| {
                            !label.is_empty()
                                && label.len() <= 63
                                && label
                                    .bytes()
                                    .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
                        }),
                    "invalid filter domain {rule:?}: expected ASCII DNS labels, not rule syntax"
                );
                let mut node = &mut root;
                for label in domain.rsplit('.') {
                    node = node
                        .children
                        .entry(label.to_ascii_lowercase().into_bytes())
                        .or_default();
                }
                match (allow, suffix) {
                    (false, false) => node.block_exact = true,
                    (false, true) => node.block_suffix = true,
                    (true, false) => node.allow_exact = true,
                    (true, true) => node.allow_suffix = true,
                }
            }
        }
        Ok(Self {
            enabled: rules.enabled,
            root: Arc::new(root),
        })
    }
}

impl Policy {
    pub fn blocks(&self, name: &Name) -> bool {
        if !self.enabled {
            return false;
        }
        let normalized = name.to_lowercase();
        let mut labels = normalized.iter().rev().peekable();
        let mut node = self.root.as_ref();
        let mut blocked = false;
        while let Some(label) = labels.next() {
            let Some(child) = node.children.get(label) else {
                break;
            };
            node = child;
            let exact = labels.peek().is_none();
            if node.allow_suffix || (exact && node.allow_exact) {
                return false;
            }
            blocked |= node.block_suffix || (exact && node.block_exact);
        }
        blocked
    }

    pub fn blocks_query(&self, query: &Message) -> bool {
        query.queries.first().is_some_and(|q| self.blocks(q.name()))
    }

    /// Only follow answer CNAMEs reachable from the original question and class.
    /// A name's allow exception never skips checks on a different chain target.
    pub fn apply_response(&self, query: &Message, response: &mut Message) {
        if !self.enabled {
            return;
        }
        let Some(question) = query.queries.first() else {
            return;
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
                return;
            }
            if let Some(targets) = links.get(name) {
                pending.extend(targets.iter().copied());
            }
        }
    }
}

#[cfg(test)]
mod tests;
