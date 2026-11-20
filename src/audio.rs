use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct TailDeadline {
    started: Instant,
    deadline_ms: Arc<AtomicU64>,
    generation: Arc<AtomicU64>,
}

impl TailDeadline {
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
            deadline_ms: Arc::new(AtomicU64::new(0)),
            generation: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn arm_after(&self, duration: Duration, generation: u64) {
        self.generation.store(generation, Ordering::Relaxed);
        self.arm_elapsed(self.started.elapsed().saturating_add(duration));
    }

    pub fn clear(&self) {
        self.deadline_ms.store(0, Ordering::Relaxed);
    }

    pub fn is_armed(&self) -> bool {
        self.deadline_ms.load(Ordering::Relaxed) != 0
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub fn deadline_ms(&self) -> u64 {
        self.deadline_ms.load(Ordering::Relaxed)
    }

    pub fn expire_if_due(&self) -> bool {
        self.expire_at(self.started.elapsed())
    }

    fn arm_elapsed(&self, elapsed: Duration) {
        self.deadline_ms.store(millis(elapsed), Ordering::Relaxed);
    }

    fn expire_at(&self, elapsed: Duration) -> bool {
        let deadline = self.deadline_ms.load(Ordering::Relaxed);
        deadline != 0
            && millis(elapsed) >= deadline
            && self
                .deadline_ms
                .compare_exchange(deadline, 0, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
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
        deadline.arm_elapsed(Duration::from_secs(1));
        assert!(deadline.is_armed());
        assert!(!deadline.expire_at(Duration::from_millis(999)));
        assert!(deadline.expire_at(Duration::from_secs(1)));
        assert!(!deadline.is_armed());
        assert!(!deadline.expire_at(Duration::from_secs(60)));
    }

    #[test]
    fn clearing_a_deadline_prevents_expiry() {
        let deadline = TailDeadline::new();
        deadline.arm_elapsed(Duration::from_secs(1));
        deadline.clear();
        assert!(!deadline.expire_at(Duration::from_secs(60)));
    }
}
