use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Default)]
struct TailState {
    deadline_ms: u64,
    generation: u64,
}

#[derive(Clone, Debug)]
pub struct TailDeadline {
    started: Instant,
    state: Arc<std::sync::Mutex<TailState>>,
}

impl TailDeadline {
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
            state: Arc::new(std::sync::Mutex::new(TailState::default())),
        }
    }

    pub fn arm_after(&self, duration: Duration, generation: u64) {
        self.arm_elapsed(self.started.elapsed().saturating_add(duration), generation);
    }

    pub fn clear(&self) {
        self.state.lock().unwrap().deadline_ms = 0;
    }

    pub fn is_armed(&self) -> bool {
        self.state.lock().unwrap().deadline_ms != 0
    }

    #[cfg(test)]
    pub fn deadline_ms(&self) -> u64 {
        self.state.lock().unwrap().deadline_ms
    }

    pub fn expire_if_due(&self) -> Option<u64> {
        self.expire_at(self.started.elapsed())
    }

    fn arm_elapsed(&self, elapsed: Duration, generation: u64) {
        *self.state.lock().unwrap() = TailState {
            deadline_ms: millis(elapsed),
            generation,
        };
    }

    fn expire_at(&self, elapsed: Duration) -> Option<u64> {
        let mut state = self.state.try_lock().ok()?;
        if state.deadline_ms != 0 && millis(elapsed) >= state.deadline_ms {
            state.deadline_ms = 0;
            return Some(state.generation);
        }
        None
    }
}

fn millis(duration: Duration) -> u64 {
    duration.as_millis().min(u64::MAX as u128) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deadline_expires_only_once_at_the_monotonic_boundary() {
        let deadline = TailDeadline::new();
        deadline.arm_elapsed(Duration::from_secs(1), 7);
        assert!(deadline.is_armed());
        assert_eq!(deadline.expire_at(Duration::from_millis(999)), None);
        assert_eq!(deadline.expire_at(Duration::from_secs(1)), Some(7));
        assert!(!deadline.is_armed());
        assert_eq!(deadline.expire_at(Duration::from_secs(60)), None);
    }

    #[test]
    fn clearing_a_deadline_prevents_expiry() {
        let deadline = TailDeadline::new();
        deadline.arm_elapsed(Duration::from_secs(1), 0);
        deadline.clear();
        assert_eq!(deadline.expire_at(Duration::from_secs(60)), None);
    }
}
