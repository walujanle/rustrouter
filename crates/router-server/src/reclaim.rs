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

/// Tune mimalloc's purge timers for a long-lived, bursty server.
///
/// mimalloc keeps freed pages for `purge_delay` ms before decommitting them, and
/// arenas multiply that by `arena_purge_mult` (10 by default), so a burst's
/// arenas stay resident for ~100 s after the last request even though nothing
/// will reuse them. Pinning the base delay to one second and the arena
/// multiplier to one makes the allocator return that memory on its own, which
/// matters more than the (small) cost of re-faulting pages on the next burst.
///
/// `mi_option_set` is documented as not thread safe, so this runs from the boot
/// path before the runtime and any request exist. The numeric option ids come
/// from mimalloc v3's `mi_option_e`; the sys crate does not export constants for
/// the newer entries.
pub fn tune_allocator() {
    // Ids are positions in mimalloc v3's `mi_option_e` (include/mimalloc.h).
    // The sys crate exports constants only up to the legacy names, so the
    // newer entries are spelled by position here; the boot log prints the
    // values read back so a mismatch is visible immediately.
    /// `mi_option_purge_delay` — milliseconds before a freed page is purged.
    const PURGE_DELAY: i32 = 15;
    /// `mi_option_arena_purge_mult` — multiplier applied to the arena delay.
    const ARENA_PURGE_MULT: i32 = 24;
    /// `mi_option_page_full_retain` — full pages kept per size class.
    const PAGE_FULL_RETAIN: i32 = 36;
    // SAFETY: plain integers, valid option ids for the vendored mimalloc v3,
    // called single-threaded at startup.
    unsafe {
        // mimalloc keeps freed pages for `purge_delay` ms, and arenas multiply
        // that by `arena_purge_mult` (10 by default), so a burst's pages stay
        // resident for ~100 s after the last request. One millisecond with a
        // multiplier of one makes a purge effectively immediate.
        //
        // Not zero: `mi_arenas_try_purge` returns early when the delay is
        // `<= 0` ("nothing will be scheduled"), so zero disables arena purging
        // rather than forcing it.
        libmimalloc_sys::mi_option_set(PURGE_DELAY, 1);
        libmimalloc_sys::mi_option_set(ARENA_PURGE_MULT, 1);
        // Two retained full pages per size class is mimalloc's default; zero
        // keeps the allocator from parking a burst's largest classes.
        libmimalloc_sys::mi_option_set(PAGE_FULL_RETAIN, 0);
        tracing::info!(
            target: "rustrouter::mem",
            "allocator: purge_delay={}ms arena_purge_mult={} page_full_retain={}",
            libmimalloc_sys::mi_option_get(PURGE_DELAY),
            libmimalloc_sys::mi_option_get(ARENA_PURGE_MULT),
            libmimalloc_sys::mi_option_get(PAGE_FULL_RETAIN),
        );
    }
}

/// Start the reclaim thread. Called once from the server boot path.
///
/// [`tune_allocator`] must already have run: the bin crate calls it before
/// anything allocates, which is the only point mimalloc honours the arena size.
pub fn spawn() {
    // Captured here, on the async caller: a plain OS thread has no runtime
    // context, so `Handle::try_current()` inside the loop always fails and the
    // per-worker collect would silently never run.
    let handle = tokio::runtime::Handle::try_current().ok();
    let _ = std::thread::Builder::new()
        .name("reclaim".into())
        .spawn(move || {
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
                trim_all_threads(handle.as_ref());
            }
        });
}

/// Collect every runtime worker's heap, then the process-wide heaps.
///
/// mimalloc 3 keeps a separate heap ("theap") per thread, and `mi_collect` only
/// collects the *calling* thread's heap. The memory a burst leaves behind is
/// almost entirely in the tokio workers' heaps, so collecting from this idle
/// thread alone frees almost nothing: in a burst test it returned 576 kB of a
/// 94 MB heap.
///
/// `spawn_blocking` runs each job on a blocking-pool thread, not a worker, which
/// is the wrong heap. `block_in_place` would run on a worker but panics on a
/// current-thread runtime. The portable answer is to hand each job a marker that
/// runs `mi_collect` on the worker that executes it: a task spawned on the
/// runtime is polled by a worker, so the collect runs in that worker's thread.
///
/// In practice a burst touches most workers, and collecting from each spawned
/// task covers the worker that polls it. Any heap not visited this round is
/// visited on the next idle window, so the footprint converges without a
/// dedicated thread per worker.
fn trim_all_threads(handle: Option<&tokio::runtime::Handle>) {
    // The calling thread's own heap plus the arenas and the C heap.
    trim();

    let Some(handle) = handle else {
        return;
    };
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(1, 16);
    for _ in 0..workers {
        handle.spawn(async {
            // A yield keeps this task from being polled inline by the caller;
            // the worker that resumes it owns the heap we want to collect.
            tokio::task::yield_now().await;
            collect_this_thread();
        });
    }
}

/// `mi_collect` on the calling thread's heap.
///
/// Separate from [`trim`] because the spawned worker tasks must not also run
/// `malloc_trim` and the per-OS working-set calls once per worker.
fn collect_this_thread() {
    // SAFETY: no arguments, no return; safe on any thread.
    unsafe { libmimalloc_sys::mi_collect(true) };
}

/// Ask each heap to hand its free pages back to the OS.
///
/// The Rust heap is mimalloc, which releases abandoned pages on its next
/// allocation-side event rather than on a clock; this thread's own tick and the
/// checkpoint ticker keep that path warm, so the calls here target the other
/// heap: the C allocator the bundled SQLite uses. Every call is best-effort;
/// where a platform has none, the process relies on the pool caps alone.
fn trim() {
    // The Rust heap first. `mi_collect(true)` abandons the per-thread free
    // lists and releases the segments they hold; without it mimalloc keeps a
    // burst's pages in its own arenas and the process stays at its high-water
    // mark. It is a no-op on a heap with nothing to reclaim, and it runs only
    // from this idle thread, never on the request path.
    //
    // SAFETY: `mi_collect` takes a bool and returns nothing; it is safe to call
    // on any thread and does not touch user memory.
    unsafe { libmimalloc_sys::mi_collect(true) };

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
