//! One generation-scoped lifecycle fact, shared by readiness and history.
use serde::Serialize;
use std::sync::Mutex;

use crate::clock::unix_ms;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Unconfigured,
    Starting,
    Running,
    /// Serving, but at least one listener is missing and being rebound. Not ready.
    Degraded,
    Stopped,
    Failed,
}

#[derive(Clone, Debug, Serialize)]
pub struct Snapshot {
    pub state: State,
    pub ready: bool,
    pub generation: u64,
    pub changed_at_ms: u64,
    pub code: Option<&'static str>,
}

pub struct Health(Mutex<Snapshot>);
impl Default for Health {
    fn default() -> Self {
        Self(Mutex::new(Snapshot {
            state: State::Unconfigured,
            ready: false,
            generation: 0,
            changed_at_ms: unix_ms(),
            code: None,
        }))
    }
}
impl Health {
    pub fn publish(&self, state: State, generation: u64, code: Option<&'static str>) -> bool {
        let mut current = self.0.lock().unwrap();
        if generation < current.generation {
            return false;
        }
        // A late normal-stop event cannot erase a failure of the same runtime.
        if generation == current.generation
            && current.state == State::Failed
            && state != State::Starting
        {
            return false;
        }
        *current = Snapshot {
            state,
            ready: state == State::Running,
            generation,
            changed_at_ms: unix_ms(),
            code,
        };
        true
    }
    pub fn snapshot(&self) -> Snapshot {
        self.0.lock().unwrap().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn late_old_terminal_never_overwrites_new_runtime_or_failure() {
        let health = Health::default();
        assert!(!health.snapshot().ready);
        health.publish(State::Running, 2, None);
        assert!(!health.publish(State::Failed, 1, Some("task_panic")));
        assert!(health.snapshot().ready);
        health.publish(State::Failed, 2, Some("listener_failed"));
        assert!(!health.publish(State::Stopped, 2, None));
        assert_eq!(health.snapshot().code, Some("listener_failed"));
        health.publish(State::Running, 3, None);
        assert!(health.snapshot().ready);
    }
}
