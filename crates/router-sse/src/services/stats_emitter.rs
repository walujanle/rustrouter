//! The stats emitter, as a broadcast channel.
//!
//! A process-wide emitter fires `"update"` when a request's usage is saved and
//! `"pending"` when a request is tracked, each behind a debounce so a burst of
//! writes produces one SSE frame. [`DEBOUNCE`] is 250 ms for both events.
//!
//! `router-db/src/stats.rs` owns the write path and cannot reach this module
//! (no dependency edge back to `router-sse`), so the call sites emit: the chat
//! handler's `DbHooks::track_pending_request` fires [`emit_pending`], and its
//! `build_save_usage` fires [`emit_update`] when a new history row landed.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::broadcast;

/// `scheduleStatsEvent`'s debounce window.
pub const DEBOUNCE: Duration = Duration::from_millis(250);

/// The two events the emitter sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatsEvent {
    /// A request finished and the full stats need a recalc.
    Update,
    /// A request started or ended; only the live fields changed.
    Pending,
}

static SENDER: OnceLock<broadcast::Sender<StatsEvent>> = OnceLock::new();

fn sender() -> &'static broadcast::Sender<StatsEvent> {
    SENDER.get_or_init(|| broadcast::channel(16).0)
}

/// Subscribe to stats events. A receiver only sees events sent after it is
/// created, which is what the SSE handler wants: its first frame is the
/// snapshot it builds itself.
pub fn subscribe() -> broadcast::Receiver<StatsEvent> {
    sender().subscribe()
}

static UPDATE_SCHEDULED: AtomicBool = AtomicBool::new(false);
static PENDING_SCHEDULED: AtomicBool = AtomicBool::new(false);

/// `scheduleStatsEvent(event)`: one event per debounce window, later calls
/// inside the window are dropped rather than queued.
fn schedule(event: StatsEvent, flag: &'static AtomicBool) {
    if flag.swap(true, Ordering::SeqCst) {
        return;
    }
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        // No runtime to debounce on; send now so a sync caller still works.
        flag.store(false, Ordering::SeqCst);
        let _ = sender().send(event);
        return;
    };
    handle.spawn(async move {
        tokio::time::sleep(DEBOUNCE).await;
        flag.store(false, Ordering::SeqCst);
        let _ = sender().send(event);
    });
}

/// `statsEmitter.emit("update")`, debounced.
pub fn emit_update() {
    schedule(StatsEvent::Update, &UPDATE_SCHEDULED);
}

/// `statsEmitter.emit("pending")`, debounced.
pub fn emit_pending() {
    schedule(StatsEvent::Pending, &PENDING_SCHEDULED);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_burst_collapses_to_one_event() {
        let mut rx = subscribe();
        emit_update();
        emit_update();
        let first = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("an event arrives")
            .unwrap();
        assert_eq!(first, StatsEvent::Update);
        assert!(
            tokio::time::timeout(Duration::from_millis(400), rx.recv())
                .await
                .is_err(),
            "the second call inside the window must not be queued"
        );
    }
}
