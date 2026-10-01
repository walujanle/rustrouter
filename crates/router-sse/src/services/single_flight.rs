//! Single-flight calls, shared by token refresh and the usage caches.
//!
//! Token refresh, the OAuth credential manager's refresh locks and the usage
//! caches all need the same shape: the first caller runs the work, everyone who
//! arrives while it is running waits on the same future, and an optional TTL
//! keeps the finished result around for a burst of retries.
//!
//! Two properties matter and are easy to lose in a rewrite:
//!
//! * **Waiters must be released even when the call never finishes.** A cancelled
//!   axum task, a panic, or a `tokio::time::timeout` aborting the owner would
//!   otherwise leave every waiter parked forever. `Publisher`'s `Drop` publishes
//!   `T::default()` in that case, which every caller already treats as "no
//!   result".
//! * **The map must not be held across the await.** The lock guards the entry
//!   bookkeeping only; `f` runs with the map released.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// One in-flight call, or a finished result that is still fresh.
pub enum Slot<T> {
    Pending(tokio::sync::watch::Receiver<Option<T>>),
    Done { value: T, expires_at: Instant },
}

/// The map type every caller names in its `static`.
pub type Slots<T> = Mutex<HashMap<String, Slot<T>>>;

/// Publishes the value a single-flight call produced, and — if the call is
/// dropped without producing one — publishes `T::default()` so waiters never
/// hang on a channel whose sender is gone.
struct Publisher<T: Default> {
    tx: Option<tokio::sync::watch::Sender<Option<T>>>,
}

impl<T: Clone + Default> Publisher<T> {
    fn publish(mut self, value: T) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(Some(value));
        }
    }
}

impl<T: Default> Drop for Publisher<T> {
    fn drop(&mut self) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(Some(T::default()));
        }
    }
}

/// Await a pending single-flight call.
async fn wait_for<T: Clone + Default>(rx: &mut tokio::sync::watch::Receiver<Option<T>>) -> T {
    loop {
        if let Some(value) = rx.borrow().clone() {
            return value;
        }
        if rx.changed().await.is_err() {
            // The publisher's `Drop` guarantees a value, so this is unreachable
            // in practice; returning the default keeps a cancelled caller from
            // taking the process down.
            return T::default();
        }
    }
}

/// Removes the pending entry when its owner's future is dropped without
/// reaching the normal cleanup — a cancelled axum task, a panic, or a timeout
/// aborting the owner. Without it a cancelled owner leaves `Slot::Pending`
/// behind forever, and the next caller takes the `Wait` branch on a channel
/// whose publisher is gone: it gets `T::default()` ("no result") instead of
/// running its own call.
///
/// Disarmed before the normal cleanup so a finished call keeps its `Done` entry.
struct SlotOwner<T: 'static> {
    map: &'static Slots<T>,
    key: String,
    armed: bool,
}

impl<T: 'static> Drop for SlotOwner<T> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut guard = self.map.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(guard.get(&self.key), Some(Slot::Pending(_))) {
            guard.remove(&self.key);
        }
    }
}

/// What this caller has to do, decided under the lock.
enum Plan<T> {
    Wait(tokio::sync::watch::Receiver<Option<T>>),
    Done(T),
    Run(tokio::sync::watch::Sender<Option<T>>),
}

/// Run `f` once per key and share its result with everyone who asked while it
/// was running.
///
/// `ttl_for` decides whether the finished value is worth remembering, and for
/// how long. `|_| None` is a pure lock: the entry disappears as soon as the call
/// finishes, so the next caller runs its own. `|_| Some(10s)` is a cache.
pub async fn single_flight<T, F, Fut, D>(map: &'static Slots<T>, key: String, ttl_for: D, f: F) -> T
where
    T: Clone + Default + Send + 'static,
    F: FnOnce() -> Fut,
    Fut: Future<Output = T>,
    D: Fn(&T) -> Option<Duration>,
{
    // The lock is released before the first await: a `MutexGuard` is not `Send`,
    // and a caller that parks on a pending slot must not hold the map.
    let plan = {
        let mut guard = map.lock().unwrap_or_else(|e| e.into_inner());
        match guard.get(&key) {
            Some(Slot::Pending(rx)) => Plan::Wait(rx.clone()),
            // Only ever stored with a TTL, so the expiry alone decides freshness.
            Some(Slot::Done { value, expires_at }) if *expires_at > Instant::now() => {
                Plan::Done(value.clone())
            }
            _ => {
                guard.remove(&key);
                // Drop expired `Done` entries now, while the lock is held. An
                // expired entry is already treated as stale on read, so this
                // only reclaims the memory of keys that are never looked up
                // again — without it a TTL cache grows one dead entry per key
                // for the process lifetime.
                guard.retain(|_, slot| match slot {
                    Slot::Done { expires_at, .. } => *expires_at > Instant::now(),
                    Slot::Pending(_) => true,
                });
                let (tx, rx) = tokio::sync::watch::channel(None::<T>);
                guard.insert(key.clone(), Slot::Pending(rx));
                Plan::Run(tx)
            }
        }
    };

    let tx = match plan {
        Plan::Wait(mut rx) => return wait_for(&mut rx).await,
        Plan::Done(value) => return value,
        Plan::Run(tx) => tx,
    };

    // Created before the call so a panic or a cancelled task inside `f` still
    // resolves the waiters, and a cancellation still removes the entry.
    let publisher = Publisher { tx: Some(tx) };
    let mut owner = SlotOwner {
        map,
        key: key.clone(),
        armed: true,
    };
    let value = f().await;
    owner.armed = false;
    publisher.publish(value.clone());

    let mut guard = map.lock().unwrap_or_else(|e| e.into_inner());
    match ttl_for(&value) {
        Some(ttl) => {
            guard.insert(
                key,
                Slot::Done {
                    value: value.clone(),
                    expires_at: Instant::now() + ttl,
                },
            );
        }
        None => {
            guard.remove(&key);
        }
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    static NO_TTL: std::sync::LazyLock<Slots<u32>> =
        std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));
    static WITH_TTL: std::sync::LazyLock<Slots<u32>> =
        std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

    #[tokio::test]
    async fn concurrent_callers_share_one_run() {
        let calls = Arc::new(AtomicU32::new(0));
        let counter = calls.clone();
        let first = tokio::spawn(async move {
            single_flight(
                &NO_TTL,
                "shared".into(),
                |_| None,
                || async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(40)).await;
                    7u32
                },
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(10)).await;
        let second = single_flight(
            &NO_TTL,
            "shared".into(),
            |_| None,
            || async { panic!("must reuse") },
        )
        .await;

        assert_eq!(first.await.unwrap(), 7);
        assert_eq!(second, 7);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_lock_without_a_ttl_reruns_the_next_time() {
        let calls = Arc::new(AtomicU32::new(0));
        for _ in 0..2 {
            let counter = calls.clone();
            single_flight(
                &NO_TTL,
                "rerun".into(),
                |_| None,
                || async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    1u32
                },
            )
            .await;
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_ttl_serves_the_cached_value() {
        let calls = Arc::new(AtomicU32::new(0));
        for _ in 0..2 {
            let counter = calls.clone();
            let value = single_flight(
                &WITH_TTL,
                "cached".into(),
                |_| Some(Duration::from_secs(30)),
                || async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    5u32
                },
            )
            .await;
            assert_eq!(value, 5);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_value_the_caller_declines_to_cache_is_not_remembered() {
        let calls = Arc::new(AtomicU32::new(0));
        for _ in 0..2 {
            let counter = calls.clone();
            // Zero is the "no buckets" sentinel the weekly overlay refuses to pin.
            single_flight(
                &WITH_TTL,
                "empty".into(),
                |v| (*v > 0).then(|| Duration::from_secs(30)),
                || async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    0u32
                },
            )
            .await;
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_cancelled_owner_still_releases_a_parked_waiter() {
        // A caller already parked on the pending slot must not hang when the
        // owner is aborted; `Publisher`'s `Drop` publishes the default, which
        // every caller reads as "no result".
        let owner = tokio::spawn(async {
            single_flight(
                &NO_TTL,
                "cancelled".into(),
                |_| None,
                || async {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    9u32
                },
            )
            .await
        });
        // Park a waiter on the same key while the owner is still running.
        let waiter = tokio::spawn(async {
            single_flight(
                &NO_TTL,
                "cancelled".into(),
                |_| None,
                || async { panic!("the parked waiter must not run its own call") },
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(10)).await;
        owner.abort();
        let _ = owner.await;

        let released = tokio::time::timeout(Duration::from_secs(2), waiter)
            .await
            .expect("the waiter must be released")
            .unwrap();
        assert_eq!(released, 0, "the cancelled owner yields the default");
    }

    #[tokio::test]
    async fn a_cancelled_owner_does_not_leave_a_pending_entry() {
        // A cancelled owner must clear its slot, not just release the waiters:
        // otherwise the next caller waits on the dead slot and gets the
        // "no result" default instead of running its own call.
        let owner = tokio::spawn(async {
            single_flight(
                &NO_TTL,
                "leak".into(),
                |_| None,
                || async {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    1u32
                },
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(10)).await;
        owner.abort();
        let _ = owner.await;

        let calls = Arc::new(AtomicU32::new(0));
        let counter = calls.clone();
        let value = single_flight(
            &NO_TTL,
            "leak".into(),
            |_| None,
            || async move {
                counter.fetch_add(1, Ordering::SeqCst);
                42u32
            },
        )
        .await;
        assert_eq!(value, 42, "the next caller runs its own call");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn expired_entries_are_swept_on_the_next_run() {
        // A key that is cached once and never looked up again must not linger.
        single_flight(
            &WITH_TTL,
            "sweep-old".into(),
            |_| Some(Duration::from_millis(1)),
            || async { 1u32 },
        )
        .await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        // A different key runs and sweeps the expired one.
        single_flight(&WITH_TTL, "sweep-new".into(), |_| None, || async { 2u32 }).await;
        let map = WITH_TTL.lock().unwrap();
        assert!(
            !map.contains_key("sweep-old"),
            "the expired entry was reclaimed"
        );
    }
}
