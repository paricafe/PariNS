//! Bounded in-flight sharing, driven by callers rather than detached tasks.
//! Weak registry references cannot keep IO alive after the last caller drops.

use crate::config::CoalescingConfig;
use futures_util::future::{BoxFuture, FutureExt, Shared, WeakShared};
use hickory_proto::op::Message;
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

/// Raw shared work result. Per-client stale fallback is intentionally not shared.
#[derive(Clone)]
pub struct Answer {
    pub response: Result<(Message, Option<u8>), ()>,
    pub stale_eligible: bool,
    pub admitted: bool,
    pub upstream: Option<String>,
    pub outgoing_ecs: Option<String>,
    pub cached: bool,
    pub cache_store: Option<crate::cache::StoreDecision>,
    pub cache_scope: Option<String>,
    pub upstream_trace: Option<crate::upstreams::diagnostics::Trace>,
    pub failure_stage: Option<crate::upstreams::diagnostics::Stage>,
    pub failure_reason: Option<crate::upstreams::diagnostics::Reason>,
    pub prefetch: bool,
}
type Work = BoxFuture<'static, Answer>;
/// Shared work, the flag it sets once its query reached a source, and the
/// number of its holds.
type Group = (WeakShared<Work>, Arc<AtomicBool>, Arc<AtomicUsize>);

/// One holder's share of work. Holds are counted explicitly, so of several
/// holders that leave at once exactly one is the last.
pub struct Hold {
    work: Option<Shared<Work>>,
    holds: Arc<AtomicUsize>,
}

impl Hold {
    fn new(work: Shared<Work>, holds: Arc<AtomicUsize>) -> Self {
        holds.fetch_add(1, Ordering::AcqRel);
        Self {
            work: Some(work),
            holds,
        }
    }

    pub async fn result(&mut self) -> Answer {
        self.work.as_mut().expect("held work").await
    }

    /// Leaves the work, or returns this hold when it is the last. The last
    /// hold stays counted, so whoever takes it over keeps the work alive.
    pub fn leave(mut self) -> Option<Self> {
        let last = self
            .holds
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |holds| {
                (holds > 1).then(|| holds - 1)
            })
            .is_err();
        if !last {
            // Already uncounted above.
            self.work = None;
        }
        last.then_some(self)
    }
}

impl Drop for Hold {
    fn drop(&mut self) {
        if self.work.is_some() {
            self.holds.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

pub enum Role {
    Bypass,
    Leader,
    Joined,
}

pub struct Flights {
    config: CoalescingConfig,
    groups: Mutex<HashMap<Vec<u8>, Group>>,
}

impl Flights {
    pub fn new(config: CoalescingConfig) -> Self {
        Self {
            config,
            groups: Mutex::new(HashMap::new()),
        }
    }

    /// Noneligible requests bypass sharing; overload fails without extra IO.
    /// `sent` is the flag that `work` sets once its query reached a source; a
    /// joined caller gets the flag of the work it joins.
    pub fn join(
        &self,
        query: &Message,
        work: Work,
        sent: Arc<AtomicBool>,
    ) -> Result<(Hold, Role, Arc<AtomicBool>), ()> {
        let Some(key) = self.config.enabled.then(|| key(query)).flatten() else {
            return Ok((Hold::new(work.shared(), Arc::default()), Role::Bypass, sent));
        };
        let mut groups = self.groups.lock().expect("flight lock poisoned");
        // Reap completed/dead weak entries before applying the group budget.
        groups.retain(|_, (weak, ..)| weak.upgrade().is_some_and(|f| f.peek().is_none()));
        if let Some((shared, sent, holds)) = groups
            .get(&key)
            .and_then(|(weak, sent, holds)| Some((weak.upgrade()?, sent.clone(), holds.clone())))
        {
            // All new holds are made under this lock. Other holders can only leave.
            if holds.load(Ordering::Acquire) >= self.config.max_waiters {
                return Err(());
            }
            return Ok((Hold::new(shared, holds), Role::Joined, sent));
        }
        if groups.len() >= self.config.max_groups {
            return Err(());
        }
        let shared = work.shared();
        let holds = Arc::default();
        groups.insert(
            key,
            (
                shared.downgrade().expect("unpolled work"),
                sent.clone(),
                Arc::clone(&holds),
            ),
        );
        Ok((Hold::new(shared, holds), Role::Leader, sent))
    }
}

fn key(query: &Message) -> Option<Vec<u8>> {
    crate::protocol::canonical_work_key(query)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hickory_proto::{
        op::{MessageType, OpCode, Query},
        rr::{Name, RecordType, rdata::opt::EdnsOption},
    };

    #[test]
    fn of_holders_that_leave_at_once_exactly_one_is_the_last() {
        let flights = Flights::new(CoalescingConfig::default());
        let mut q = Message::new(1, MessageType::Query, OpCode::Query);
        q.add_query(Query::query(
            Name::from_ascii("example.test.").unwrap(),
            RecordType::A,
        ));
        let work = || futures_util::future::pending::<Answer>().boxed();
        let sent = Arc::new(AtomicBool::new(true));
        for _ in 0..2000 {
            let (first, ..) = flights.join(&q, work(), sent.clone()).unwrap();
            let (second, ..) = flights.join(&q, work(), sent.clone()).unwrap();
            let barrier = std::sync::Barrier::new(2);
            let lasts: Vec<_> = std::thread::scope(|scope| {
                [first, second]
                    .map(|hold| {
                        let barrier = &barrier;
                        scope.spawn(move || {
                            barrier.wait();
                            hold.leave()
                        })
                    })
                    .map(|thread| thread.join().unwrap())
            })
            .into_iter()
            .flatten()
            .collect();
            assert_eq!(lasts.len(), 1);
            // The last hold stays counted: a later holder is not the last.
            let (later, role, _) = flights.join(&q, work(), sent.clone()).unwrap();
            assert!(matches!(role, Role::Joined));
            assert!(later.leave().is_none());
        }
    }

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
