//! Claim an `AtomicBool` for the duration of a task.
//!
//! A plain `swap(true)` / `store(false)` pair leaks the flag if the guarded
//! body panics: the reset never runs and the task is disabled for the life of
//! the process. The guard clears the flag on drop instead, so a panic unwinds
//! through it and the next tick can run.

use std::sync::atomic::{AtomicBool, Ordering};

/// Clears the flag on drop.
pub struct InFlightGuard(&'static AtomicBool);

impl InFlightGuard {
    /// Claims `flag`, or returns `None` when a run is already in flight.
    pub fn acquire(flag: &'static AtomicBool) -> Option<Self> {
        if flag.swap(true, Ordering::SeqCst) {
            return None;
        }
        Some(Self(flag))
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static FLAG: AtomicBool = AtomicBool::new(false);

    #[test]
    fn a_second_acquire_fails_while_held_and_a_drop_frees_it() {
        let guard = InFlightGuard::acquire(&FLAG).expect("first acquire");
        assert!(InFlightGuard::acquire(&FLAG).is_none());
        drop(guard);
        assert!(InFlightGuard::acquire(&FLAG).is_some());
    }
}
