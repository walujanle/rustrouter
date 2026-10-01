//! Env-gated resident-memory sampler (`RUSTROUTER_MEM_REPORT=1`).
//!
//! Diagnostic only. It exists so a burst can be bracketed from the dashboard's
//! console log page instead of a second terminal watching Task Manager. The
//! thread starts only when the variable is set, so normal runs pay nothing.
//!
//! Windows reports the working set and private commit separately on purpose:
//! Task Manager's "Memory" column is the private working set, which excludes the
//! file-backed pages of the SQLite `mmap_size` window, so it understates what the
//! process has committed.

use std::time::Duration;

const SAMPLE_INTERVAL: Duration = Duration::from_secs(10);

/// Start the sampler when `RUSTROUTER_MEM_REPORT` is set to a truthy value.
pub fn spawn_if_enabled() {
    let enabled = std::env::var("RUSTROUTER_MEM_REPORT")
        .map(|v| !matches!(v.as_str(), "" | "0" | "false"))
        .unwrap_or(false);
    if !enabled {
        return;
    }
    let _ = std::thread::Builder::new()
        .name("mem-report".into())
        .spawn(|| {
            loop {
                if let Some(line) = sample() {
                    tracing::info!(target: "rustrouter::mem", "{line}");
                }
                std::thread::sleep(SAMPLE_INTERVAL);
            }
        });
}

#[cfg(windows)]
fn sample() -> Option<String> {
    // `PROCESS_MEMORY_COUNTERS_EX` from psapi.h. `cb` must be the size of this
    // struct, which is how the callee knows it may fill `PrivateUsage`.
    #[repr(C)]
    struct Counters {
        cb: u32,
        page_fault_count: u32,
        peak_working_set_size: usize,
        working_set_size: usize,
        quota_peak_paged_pool_usage: usize,
        quota_paged_pool_usage: usize,
        quota_peak_non_paged_pool_usage: usize,
        quota_non_paged_pool_usage: usize,
        pagefile_usage: usize,
        peak_pagefile_usage: usize,
        private_usage: usize,
    }

    unsafe extern "system" {
        fn GetCurrentProcess() -> *mut core::ffi::c_void;
        fn GetProcessMemoryInfo(
            process: *mut core::ffi::c_void,
            counters: *mut Counters,
            cb: u32,
        ) -> i32;
    }

    let mut counters = Counters {
        cb: size_of::<Counters>() as u32,
        page_fault_count: 0,
        peak_working_set_size: 0,
        working_set_size: 0,
        quota_peak_paged_pool_usage: 0,
        quota_paged_pool_usage: 0,
        quota_peak_non_paged_pool_usage: 0,
        quota_non_paged_pool_usage: 0,
        pagefile_usage: 0,
        peak_pagefile_usage: 0,
        private_usage: 0,
    };
    // SAFETY: the pointer is the current process pseudo-handle and the struct
    // is sized by its own `cb`.
    let ok = unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb) };
    if ok == 0 {
        return None;
    }
    Some(format!(
        "RSS {:.1} MB working set · private commit {:.1} MB · peak working set {:.1} MB",
        counters.working_set_size as f64 / 1_048_576.0,
        counters.private_usage as f64 / 1_048_576.0,
        counters.peak_working_set_size as f64 / 1_048_576.0,
    ))
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn sample() -> Option<String> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let kb = |key: &str| -> Option<u64> {
        status
            .lines()
            .find(|line| line.starts_with(key))
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|value| value.parse().ok())
    };
    let rss = kb("VmRSS")?;
    let peak = kb("VmHWM").unwrap_or(0);
    let swap = kb("VmSwap").unwrap_or(0);
    Some(format!(
        "RSS {:.1} MB · peak {:.1} MB · swap {:.1} MB",
        rss as f64 / 1024.0,
        peak as f64 / 1024.0,
        swap as f64 / 1024.0,
    ))
}

#[cfg(target_os = "macos")]
fn sample() -> Option<String> {
    let pid = std::process::id().to_string();
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=,vsz=", "-p", &pid])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut parts = text.split_whitespace();
    let rss: u64 = parts.next()?.parse().ok()?;
    let vsz: u64 = parts.next().unwrap_or("0").parse().unwrap_or(0);
    Some(format!(
        "RSS {:.1} MB · virtual {:.1} MB",
        rss as f64 / 1024.0,
        vsz as f64 / 1024.0,
    ))
}

#[cfg(not(any(
    windows,
    target_os = "linux",
    target_os = "android",
    target_os = "macos"
)))]
fn sample() -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sampling_reports_a_plausible_reading_on_this_host() {
        let line = sample().expect("a supported host reports a reading");
        assert!(line.starts_with("RSS "), "{line}");
        // The test binary is a few MB, never zero or absurd.
        let first = line
            .split_whitespace()
            .nth(1)
            .and_then(|v| v.parse::<f64>().ok())
            .expect("a numeric MB value");
        assert!(first > 0.1 && first < 10_000.0, "{line}");
    }
}
