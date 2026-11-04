//! Deterministic autosave scheduling policy.
//!
//! The scheduler owns no thread and performs no I/O. The application event loop
//! polls it while the application is open, then persists data using the storage
//! and recovery-journal APIs.

use std::time::{Duration, Instant};

/// Timing rules for autosave.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AutosavePolicy {
    /// Save after this much inactivity.
    debounce: Duration,
    /// Save no later than this interval after the first unsaved edit.
    maximum_dirty_age: Duration,
}

impl AutosavePolicy {
    /// Create a policy, rejecting zero or inconsistent intervals.
    pub fn new(debounce: Duration, maximum_dirty_age: Duration) -> Result<Self, &'static str> {
        if debounce.is_zero() {
            return Err("autosave debounce must be greater than zero");
        }
        if maximum_dirty_age < debounce {
            return Err("maximum dirty age must be at least the debounce interval");
        }
        Ok(Self {
            debounce,
            maximum_dirty_age,
        })
    }

    pub fn debounce(&self) -> Duration {
        self.debounce
    }

    pub fn maximum_dirty_age(&self) -> Duration {
        self.maximum_dirty_age
    }
}

impl Default for AutosavePolicy {
    fn default() -> Self {
        Self {
            debounce: Duration::from_secs(2),
            maximum_dirty_age: Duration::from_secs(15),
        }
    }
}

/// Tracks whether unsaved changes have reached their autosave deadline.
///
/// A failed save must not call mark_saved: the scheduler will remain due and
/// the application can retry or ask the user to intervene.
#[derive(Debug)]
pub struct AutosaveScheduler {
    policy: AutosavePolicy,
    dirty_since: Option<Instant>,
    last_change: Option<Instant>,
}

impl AutosaveScheduler {
    pub fn new(policy: AutosavePolicy) -> Self {
        Self {
            policy,
            dirty_since: None,
            last_change: None,
        }
    }

    /// Notify the scheduler that document state changed.
    pub fn mark_dirty(&mut self, now: Instant) {
        self.dirty_since.get_or_insert(now);
        self.last_change = Some(now);
    }

    /// Returns true when an autosave should be started.
    pub fn should_save(&self, now: Instant) -> bool {
        let (Some(dirty_since), Some(last_change)) = (self.dirty_since, self.last_change) else {
            return false;
        };

        now.saturating_duration_since(last_change) >= self.policy.debounce
            || now.saturating_duration_since(dirty_since) >= self.policy.maximum_dirty_age
    }

    /// Call only after the document and its recovery state were saved successfully.
    pub fn mark_saved(&mut self) {
        self.dirty_since = None;
        self.last_change = None;
    }

    /// Whether changes remain unsaved.
    pub fn is_dirty(&self) -> bool {
        self.dirty_since.is_some()
    }

    /// The policy currently used by this scheduler.
    pub fn policy(&self) -> AutosavePolicy {
        self.policy
    }
}

impl Default for AutosaveScheduler {
    fn default() -> Self {
        Self::new(AutosavePolicy::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waits_for_debounce_after_last_edit() {
        let start = Instant::now();
        let mut scheduler = AutosaveScheduler::default();
        scheduler.mark_dirty(start);
        scheduler.mark_dirty(start + Duration::from_secs(1));

        assert!(!scheduler.should_save(start + Duration::from_millis(2_999)));
        assert!(scheduler.should_save(start + Duration::from_secs(3)));
    }

    #[test]
    fn maximum_dirty_age_saves_even_while_user_keeps_typing() {
        let start = Instant::now();
        let mut scheduler = AutosaveScheduler::default();

        for second in 0..15 {
            scheduler.mark_dirty(start + Duration::from_secs(second));
        }

        assert!(scheduler.should_save(start + Duration::from_secs(15)));
    }

    #[test]
    fn successful_save_clears_dirty_state() {
        let start = Instant::now();
        let mut scheduler = AutosaveScheduler::default();
        scheduler.mark_dirty(start);
        assert!(scheduler.is_dirty());

        scheduler.mark_saved();

        assert!(!scheduler.is_dirty());
        assert!(!scheduler.should_save(start + Duration::from_secs(30)));
    }

    #[test]
    fn failed_save_remains_due_until_explicitly_marked_saved() {
        let start = Instant::now();
        let mut scheduler = AutosaveScheduler::default();
        scheduler.mark_dirty(start);
        let deadline = start + Duration::from_secs(2);

        // A failed persistence operation does not reset the scheduler.
        assert!(scheduler.should_save(deadline));
        assert!(scheduler.is_dirty());
        assert!(scheduler.should_save(deadline + Duration::from_secs(1)));
    }

    #[test]
    fn rejects_invalid_policies() {
        assert!(AutosavePolicy::new(Duration::ZERO, Duration::from_secs(10)).is_err());
        assert!(AutosavePolicy::new(Duration::from_secs(5), Duration::from_secs(2)).is_err());
    }
}
