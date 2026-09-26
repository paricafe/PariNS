//! One active aggregate and at most one request-held retired generation.
use crate::policy::Policy;
use anyhow::{Result, ensure};
use std::sync::{Arc, RwLock, Weak};

pub struct Generation {
    pub number: u64,
    pub policy: Policy,
}

struct State {
    current: Arc<Generation>,
    retired: Weak<Generation>,
}

pub struct PolicyHandle(RwLock<State>);

impl PolicyHandle {
    pub fn new(policy: Policy) -> Arc<Self> {
        Arc::new(Self(RwLock::new(State {
            current: Arc::new(Generation { number: 1, policy }),
            retired: Weak::new(),
        })))
    }

    pub fn snapshot(&self) -> Arc<Generation> {
        self.0.read().expect("filter generation").current.clone()
    }

    pub(crate) fn ensure_available(&self) -> Result<()> {
        let state = self.0.read().expect("filter generation");
        ensure!(
            state.retired.strong_count() == 0,
            "subscription_busy: old requests still use a retired generation"
        );
        Ok(())
    }

    pub(crate) fn retained_bytes_with_locals(&self, locals: &[&Policy]) -> usize {
        let state = self.0.read().expect("filter generation");
        let retired = state.retired.upgrade();
        let mut policies = vec![&state.current.policy];
        for policy in retired
            .as_ref()
            .map(|generation| &generation.policy)
            .into_iter()
            .chain(locals.iter().copied())
        {
            if !policies
                .iter()
                .any(|existing| existing.same_allocation(policy))
            {
                policies.push(policy);
            }
        }
        policies.into_iter().fold(0_usize, |bytes, policy| {
            bytes.saturating_add(policy.owned_bytes())
        })
    }

    /// The single compilation/publication owner checked availability before commit.
    /// Readers can only keep an old generation alive, never resurrect a released one.
    pub(crate) fn publish(&self, policy: Policy) {
        self.try_publish(policy)
            .expect("single filter publication owner");
    }

    pub(crate) fn try_publish(&self, policy: Policy) -> Result<()> {
        self.try_publish_with(policy, || Ok(()))
    }

    pub(crate) fn try_publish_with(
        &self,
        policy: Policy,
        before_publish: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        let mut state = self.0.write().expect("filter generation");
        ensure!(
            state.retired.strong_count() == 0,
            "subscription_busy: old requests still use a retired generation"
        );
        before_publish()?;
        let number = state.current.number.saturating_add(1);
        state.retired = Arc::downgrade(&state.current);
        state.current = Arc::new(Generation { number, policy });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn memory_counts_shared_local_once_and_distinct_old_local_until_release() {
        let local: Policy = toml::from_str("enabled=true\nblock_exact=['one.test']").unwrap();
        let handle = PolicyHandle::new(local.clone());
        assert_eq!(
            handle.retained_bytes_with_locals(&[&local]),
            local.owned_bytes()
        );
        let request = handle.snapshot();
        let next: Policy = toml::from_str("enabled=true\nblock_exact=['two.test']").unwrap();
        handle.publish(next.clone());
        let total = local.owned_bytes() + next.owned_bytes();
        assert_eq!(handle.retained_bytes_with_locals(&[&local, &next]), total);
        drop(request);
        assert_eq!(handle.retained_bytes_with_locals(&[]), next.owned_bytes());
        assert_eq!(handle.retained_bytes_with_locals(&[&local, &next]), total);
    }

    #[test]
    fn requests_pin_only_one_retired_generation() {
        let handle = PolicyHandle::new(Policy::default());
        let request = handle.snapshot();
        handle.publish(Policy::default());
        assert_eq!(request.number, 1);
        assert_eq!(handle.snapshot().number, 2);
        assert!(handle.ensure_available().is_err());
        drop(request);
        handle.ensure_available().unwrap();
        handle.publish(Policy::default());
        assert_eq!(handle.snapshot().number, 3);
    }
}
