//! Bounded in-flight sharing, driven by callers rather than detached tasks.
//! Weak registry references cannot keep IO alive after the last caller drops.

use crate::config::CoalescingConfig;
use futures_util::future::{BoxFuture, FutureExt, Shared, WeakShared};
use hickory_proto::{
    op::Message,
    rr::{DNSClass, rdata::opt::EdnsCode},
};
use std::{collections::HashMap, sync::Mutex};

pub type Answer = Result<(Message, Option<u8>), ()>;
type Work = BoxFuture<'static, Answer>;
pub enum Role {
    Bypass,
    Leader,
    Joined,
}

pub struct Flights {
    config: CoalescingConfig,
    groups: Mutex<HashMap<Vec<u8>, WeakShared<Work>>>,
}

impl Flights {
    pub fn new(config: CoalescingConfig) -> Self {
        Self {
            config,
            groups: Mutex::new(HashMap::new()),
        }
    }

    /// Noneligible requests bypass sharing; overload fails without extra IO.
    pub fn join(&self, query: &Message, work: Work) -> Result<(Shared<Work>, Role), ()> {
        let Some(key) = self.config.enabled.then(|| key(query)).flatten() else {
            return Ok((work.shared(), Role::Bypass));
        };
        let mut groups = self.groups.lock().expect("flight lock poisoned");
        // Reap completed/dead weak entries before applying the group budget.
        groups.retain(|_, weak| weak.upgrade().is_some_and(|f| f.peek().is_none()));
        if let Some(shared) = groups.get(&key).and_then(WeakShared::upgrade) {
            // All new clones are made under this lock. Other callers can only drop.
            if shared.strong_count().unwrap_or(0) > self.config.max_waiters {
                return Err(());
            }
            return Ok((shared, Role::Joined));
        }
        if groups.len() >= self.config.max_groups {
            return Err(());
        }
        let shared = work.shared();
        groups.insert(key, shared.downgrade().expect("unpolled work"));
        Ok((shared, Role::Leader))
    }
}

fn key(query: &Message) -> Option<Vec<u8>> {
    if query.queries.len() != 1
        || query.queries[0].query_class() != DNSClass::IN
        || query.edns.as_ref().is_some_and(|e| {
            e.options()
                .options
                .iter()
                .any(|(code, _)| *code != EdnsCode::Subnet)
        })
    {
        return None;
    }
    let mut normalized = query.clone();
    normalized.metadata.id = 0;
    normalized.queries[0].set_name(query.queries[0].name().to_lowercase());
    normalized.to_vec().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use hickory_proto::{
        op::{MessageType, OpCode, Query},
        rr::{Name, RecordType, rdata::opt::EdnsOption},
    };

    #[test]
    fn key_preserves_outbound_semantics_except_id_and_case() {
        let mut q = Message::new(1, MessageType::Query, OpCode::Query);
        q.add_query(Query::query(
            Name::from_ascii("Example.Test.").unwrap(),
            RecordType::A,
        ));
        crate::ecs::set_subnet(&mut q, Some("192.0.2.0/24".parse().unwrap()));
        let original = key(&q).unwrap();
        q.metadata.id = 2;
        q.queries[0].set_name(Name::from_ascii("example.test.").unwrap());
        assert_eq!(key(&q).unwrap(), original);
        for subnet in [
            "192.0.3.0/24",
            "192.0.2.0/25",
            "0.0.0.0/0",
            "::/0",
            "2001:db8::/56",
        ] {
            let mut other = q.clone();
            crate::ecs::set_subnet(&mut other, Some(subnet.parse().unwrap()));
            assert_ne!(key(&other).unwrap(), original);
        }
        for flag in 0..3 {
            let mut other = q.clone();
            match flag {
                0 => other.metadata.checking_disabled = true,
                1 => other.metadata.recursion_desired = true,
                _ => {
                    other.edns.as_mut().unwrap().set_dnssec_ok(true);
                }
            }
            assert_ne!(key(&other).unwrap(), original);
        }
        q.edns
            .as_mut()
            .unwrap()
            .options_mut()
            .insert(EdnsOption::Unknown(10, vec![1; 8]));
        assert!(key(&q).is_none());
    }
}
