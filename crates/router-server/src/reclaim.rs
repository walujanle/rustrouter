//! Idle-gated memory reclaim.
//!
//! The pool caps bound how much the process holds while it works: the SQLite
//! page cache, the reqwest client cache, the worker threads. This module is the
//! other half, giving the memory back once the work stops. A burst of agentic
//! traffic leaves freed pages in two heaps — mimalloc for Rust, the C allocator
//! for the bundled SQLite — and neither returns them to the OS until something
//! asks. The trim only runs after the request streams have been gone a while, so
//! it can never page out a live response.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures::Stream;
use futures::StreamExt;

/// Request streams currently being served.
static INFLIGHT: AtomicUsize = AtomicUsize::new(0);

/// How often the idle check runs.
const TICK: Duration = Duration::from_secs(5);

/// Consecutive idle ticks before a trim is worth the page faults it causes.
const IDLE_TICKS: u32 = 6;

/// The number of request streams currently in flight.
///
/// Exposed so a route test can assert the wiring: the counter has to move when
/// a chat response body is built, or the idle gate is meaningless.
#[cfg(test)]
pub(crate) fn in_flight() -> usize {
    INFLIGHT.load(Ordering::Relaxed)
}

/// Wrap a response body stream so the reclaim thread can tell whether a request
/// is in flight.
///
/// Only request streams are tracked. The dashboard's console-log and stats
/// feeds are long-lived by design, so counting them would hold the process busy
/// for as long as any tab is open and the trim would never run.
pub fn tracked<S>(stream: S) -> impl Stream<Item = S::Item>
where
    S: Stream + Send + 'static,
{
    INFLIGHT.fetch_add(1, Ordering::Relaxed);
    let guard = Guard;
    async_stream::stream! {
        // Dropped with the generator, on completion or on a client disconnect.
        let _guard = guard;
        futures::pin_mut!(stream);
        while let Some(item) = stream.next().await {
            yield item;
        }
    }
}

struct Guard;

impl Drop for Guard {
    fn drop(&mut self) {
        INFLIGHT.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Start the reclaim thread. Called once from the server boot path.
pub fn spawn() {
    let _ = std::thread::Builder::new()
        .name("reclaim".into())
        .spawn(|| {
            let mut idle = 0u32;
            loop {
                std::thread::sleep(TICK);
                if INFLIGHT.load(Ordering::Relaxed) != 0 {
                    idle = 0;
                    continue;
                }
                idle += 1;
                if idle < IDLE_TICKS {
                    continue;
                }
                idle = 0;
                trim();
            }
        });
}

/// Ask each heap to hand its free pages back to the OS.
///
/// The Rust heap is mimalloc, which releases abandoned pages on its next
/// allocation-side event rather than on a clock; this thread's own tick and the
/// checkpoint ticker keep that path warm, so the calls here target the other
/// heap: the C allocator the bundled SQLite uses. Every call is best-effort;
/// where a platform has none, the process relies on the pool caps alone.
fn trim() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        // SAFETY: no arguments, and the return value is a plain status flag.
        let released = unsafe { malloc_trim(0) };
        tracing::debug!(target: "rustrouter::mem", "reclaim: malloc_trim released={released}");
    }

    #[cfg(target_os = "android")]
    {
        // bionic's `M_PURGE`; the value is fixed by the platform header.
        const M_PURGE: i32 = -101;
        // `mallopt` is `__INTRODUCED_IN(26)` and cargo-ndk defaults to API 21,
        // so a direct call fails to link. Resolve it at runtime instead: the
        // symbol is absent on older platforms, where this is a no-op, and the
        // C heap keeps its pages (the pool caps still bound it).
        if let Some(mallopt) = android_mallopt() {
            // SAFETY: two integers, return value ignored.
            let _ = unsafe { mallopt(M_PURGE, 0) };
            tracing::debug!(target: "rustrouter::mem", "reclaim: mallopt(M_PURGE)");
        }
    }

    #[cfg(target_os = "macos")]
    {
        // SAFETY: a null zone means "all zones", and a zero goal means "as much
        // as possible"; the return value is a byte count.
        let freed = unsafe { malloc_zone_pressure_relief(std::ptr::null_mut(), 0) };
        tracing::debug!(target: "rustrouter::mem", "reclaim: pressure_relief freed={freed}");
    }

    #[cfg(windows)]
    {
        // Trims the working set. This is what Task Manager's Memory column
        // shows, so it is the number the dashboard user watches; the pages
        // fault back in on the next request, which is why the trim waits for
        // idle.
        const TRIM: usize = usize::MAX;
        // SAFETY: the current-process pseudo-handle needs no close, and
        // `(TRIM, TRIM)` is the documented "remove as many pages as possible".
        let ok = unsafe { SetProcessWorkingSetSizeEx(GetCurrentProcess(), TRIM, TRIM, 0) };
        tracing::debug!(target: "rustrouter::mem", "reclaim: working set trim ok={ok}");
    }
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
unsafe extern "C" {
    fn malloc_trim(pad: usize) -> i32;
}

/// bionic's `mallopt`, resolved at runtime.
///
/// `mallopt` is `__INTRODUCED_IN(26)` and cargo-ndk links against API 21, so a
/// direct call fails with an undefined symbol. `dlsym` is always present, so
/// this returns `None` where the platform predates the symbol.
#[cfg(target_os = "android")]
fn android_mallopt() -> Option<unsafe extern "C" fn(i32, i32) -> i32> {
    /// LP64 `RTLD_DEFAULT`; `aarch64-linux-android` is LP64, where it is null.
    const RTLD_DEFAULT: *mut core::ffi::c_void = core::ptr::null_mut();

    // SAFETY: `RTLD_DEFAULT` is a valid handle and the name is a static,
    // NUL-terminated C string.
    let sym = unsafe { dlsym(RTLD_DEFAULT, c"mallopt".as_ptr()) };
    if sym.is_null() {
        return None;
    }
    // SAFETY: non-null, and where present `mallopt` has this signature.
    Some(unsafe { std::mem::transmute(sym) })
}

#[cfg(target_os = "android")]
unsafe extern "C" {
    fn dlsym(
        handle: *mut core::ffi::c_void,
        symbol: *const core::ffi::c_char,
    ) -> *mut core::ffi::c_void;
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn malloc_zone_pressure_relief(zone: *mut core::ffi::c_void, goal: usize) -> usize;
}

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentProcess() -> *mut core::ffi::c_void;
    fn SetProcessWorkingSetSizeEx(
        process: *mut core::ffi::c_void,
        min: usize,
        max: usize,
        flags: u32,
    ) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// The counter is process-global and the test runner is parallel, so the
    /// tests that read it take turns; without this they would see each other's
    /// increments.
    static SERIAL: Mutex<()> = Mutex::new(());

    fn inflight() -> usize {
        INFLIGHT.load(Ordering::Relaxed)
    }

    /// The counter has to be up before the first poll and back to zero once the
    /// body is dropped, or the idle gate never opens and the trim never runs.
    #[test]
    fn tracked_counts_a_body_for_its_whole_life() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let before = inflight();
        let body = tracked(futures::stream::empty::<u8>());
        assert_eq!(inflight(), before + 1, "counted at construction");

        let body2 = tracked(futures::stream::empty::<u8>());
        assert_eq!(inflight(), before + 2);

        drop(body);
        assert_eq!(inflight(), before + 1);
        drop(body2);
        assert_eq!(inflight(), before, "released on drop");
    }

    /// A body that is drained to the end also releases its slot.
    #[test]
    fn a_fully_consumed_body_releases_its_slot() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let before = inflight();
        let mut body = Box::pin(tracked(futures::stream::iter(vec![1u8, 2])));
        let collected = futures::executor::block_on(async {
            let mut out = Vec::new();
            while let Some(item) = body.next().await {
                out.push(item);
            }
            out
        });
        assert_eq!(collected, vec![1, 2]);
        drop(body);
        assert_eq!(inflight(), before);
    }

    /// Exercise the per-OS trim on the host. A wrong `extern` declaration
    /// (calling convention, argument count, signature) is undefined behaviour
    /// that only shows at the call, so the test calls it rather than trusting
    /// the declaration.
    #[test]
    fn trim_runs_on_this_host() {
        trim();
    }
}
