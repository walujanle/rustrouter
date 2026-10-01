//! In-memory progressive login lockout.
//!
//! "Resets on process restart" is correct: losing these buckets only means an
//! attacker gets a fresh attempt budget after a restart, which is not a
//! security property anyone relies on. Do not persist this.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;

/// `MAX_FAILS_BEFORE_LOCK`.
pub const MAX_FAILS_BEFORE_LOCK: u32 = 5;
/// `LOCK_STEPS_MS`: 30s, 2m, 10m, 30m. The last step repeats.
pub const LOCK_STEPS_MS: [i64; 4] = [30_000, 120_000, 600_000, 1_800_000];
/// `FAIL_WINDOW_MS`: an hour since the last failure resets the entry.
pub const FAIL_WINDOW_MS: i64 = 60 * 60 * 1000;
/// Bound on tracked buckets. `record_fail` is reachable pre-auth, so an
/// attacker cycling source IPs would otherwise grow the map for the process
/// lifetime. On overflow the oldest entries are dropped, which only ever gives
/// an attacker back an attempt budget they already had.
const MAX_TRACKED_IPS: usize = 10_000;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Entry {
    fails: u32,
    lock_until: i64,
    lock_level: u32,
    last_fail_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LockStatus {
    pub locked: bool,
    pub retry_after: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FailStatus {
    pub remaining_before_lock: u32,
}

/// The lockout buckets. Process-global, one map for the whole process.
pub struct Limiter {
    attempts: Mutex<HashMap<String, Entry>>,
}

impl Default for Limiter {
    fn default() -> Self {
        Self::new()
    }
}

impl Limiter {
    pub fn new() -> Self {
        Self {
            attempts: Mutex::new(HashMap::new()),
        }
    }

    /// `checkLock(ip)`.
    pub fn check_lock(&self, ip: &str, now_ms: i64) -> LockStatus {
        let mut attempts = self.attempts.lock().unwrap_or_else(|e| e.into_inner());
        let Some(e) = get_entry(&mut attempts, ip, now_ms) else {
            return LockStatus {
                locked: false,
                retry_after: 0,
            };
        };
        if e.lock_until == 0 {
            return LockStatus {
                locked: false,
                retry_after: 0,
            };
        }
        let remaining = e.lock_until - now_ms;
        if remaining <= 0 {
            return LockStatus {
                locked: false,
                retry_after: 0,
            };
        }
        LockStatus {
            locked: true,
            // `Math.ceil(remaining / 1000)`.
            retry_after: (remaining + 999) / 1000,
        }
    }

    /// `recordFail(ip)`.
    pub fn record_fail(&self, ip: &str, now_ms: i64) -> FailStatus {
        let mut attempts = self.attempts.lock().unwrap_or_else(|e| e.into_inner());
        let mut e = get_entry(&mut attempts, ip, now_ms).unwrap_or_default();
        e.fails += 1;
        e.last_fail_at = now_ms;
        if e.fails >= MAX_FAILS_BEFORE_LOCK {
            let step = LOCK_STEPS_MS[(e.lock_level as usize).min(LOCK_STEPS_MS.len() - 1)];
            e.lock_until = now_ms + step;
            e.lock_level += 1;
            e.fails = 0;
        }
        if !attempts.contains_key(ip) && attempts.len() >= MAX_TRACKED_IPS {
            evict_oldest(&mut attempts);
        }
        attempts.insert(ip.to_string(), e);
        FailStatus {
            remaining_before_lock: MAX_FAILS_BEFORE_LOCK.saturating_sub(e.fails),
        }
    }

    /// `recordSuccess(ip)`.
    pub fn record_success(&self, ip: &str) {
        let mut attempts = self.attempts.lock().unwrap_or_else(|e| e.into_inner());
        attempts.remove(ip);
    }
}

/// Drop the least-recently-failing bucket so the map stays bounded. Linear in
/// the map size, but it only runs on insert once the cap is reached.
fn evict_oldest(attempts: &mut HashMap<String, Entry>) {
    let Some(oldest) = attempts
        .iter()
        .min_by_key(|(_, e)| e.last_fail_at)
        .map(|(ip, _)| ip.clone())
    else {
        return;
    };
    attempts.remove(&oldest);
}

/// `getEntry(ip)`: drop the entry when the failure window has lapsed and no
/// lock is currently held.
fn get_entry(attempts: &mut HashMap<String, Entry>, ip: &str, now_ms: i64) -> Option<Entry> {
    let e = *attempts.get(ip)?;
    // `e.lastFailAt &&` — a fresh entry has 0 there and skips the reset.
    if e.last_fail_at != 0
        && now_ms - e.last_fail_at > FAIL_WINDOW_MS
        && (e.lock_until == 0 || now_ms >= e.lock_until)
    {
        attempts.remove(ip);
        return None;
    }
    Some(e)
}

/// The client IP, folded into the proxy-trust decision.
///
/// A forwarding header is trusted only when the socket peer is a loopback
/// reverse proxy; otherwise the socket address wins. In axum the socket is
/// visible directly, so no per-process peer token is needed.
///
/// There is no `"unknown"` fallback: a peer address is always available, so it
/// would never fire. Noted here so nobody adds it back.
pub fn client_ip(peer: IpAddr, x_forwarded_for: Option<&str>, x_real_ip: Option<&str>) -> String {
    let proxy_ip = x_real_ip
        .filter(|s| !s.is_empty())
        .or_else(|| x_forwarded_for.map(|s| s.split(',').next().unwrap_or("").trim()))
        .filter(|s| !s.is_empty());
    if is_loopback_addr(peer)
        && let Some(proxy_ip) = proxy_ip
    {
        return proxy_ip.to_string();
    }
    peer.to_string()
}

/// True when the address is a loopback peer, including the IPv4-mapped IPv6
/// form `::ffff:127.0.0.1` that a dual-stack listener reports.
pub fn is_loopback_addr(addr: IpAddr) -> bool {
    match addr {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => {
            v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
    }
}

/// `isLoopbackHostname(h)`: handles `localhost`, IPv4, bracketed IPv6, and the
/// `::ffff:` mapped form.
pub fn is_loopback_hostname(host: &str) -> bool {
    let name = host.trim().to_ascii_lowercase();
    if name.is_empty() {
        return false;
    }
    let name = if let Some(rest) = name.strip_prefix('[') {
        // Bracketed IPv6: everything up to the closing bracket.
        match rest.find(']') {
            Some(end) => &rest[..end],
            None => return false,
        }
    } else if name.matches(':').count() == 1 {
        // Exactly one colon is an IPv4 `host:port`.
        &name[..name.find(':').unwrap_or(0)]
    } else {
        &name[..]
    };
    let name = name.strip_prefix("::ffff:").unwrap_or(name);
    matches!(name, "localhost" | "127.0.0.1" | "::1")
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: i64 = 1_700_000_000_000;

    #[test]
    fn locks_on_the_fifth_failure_for_thirty_seconds() {
        let l = Limiter::new();
        for i in 0..4 {
            let s = l.record_fail("1.2.3.4", T0 + i);
            assert_eq!(s.remaining_before_lock, 4 - i as u32);
            assert!(!l.check_lock("1.2.3.4", T0 + i).locked);
        }
        let s = l.record_fail("1.2.3.4", T0 + 4);
        // The counter resets on lock, so the countdown restarts at 5.
        assert_eq!(s.remaining_before_lock, 5);
        let status = l.check_lock("1.2.3.4", T0 + 4);
        assert!(status.locked);
        assert_eq!(status.retry_after, 30);
        // The lock is `fail_time + step`, so 30s set at T0+4 expires at T0+30004.
        assert!(l.check_lock("1.2.3.4", T0 + 30_000).locked);
        assert!(!l.check_lock("1.2.3.4", T0 + 30_004).locked);
    }

    #[test]
    fn lock_steps_escalate_and_then_repeat() {
        let l = Limiter::new();
        let mut now = T0;
        let mut seen = Vec::new();
        for _ in 0..6 {
            for _ in 0..MAX_FAILS_BEFORE_LOCK {
                now += 1;
                l.record_fail("ip", now);
            }
            let status = l.check_lock("ip", now);
            seen.push(status.retry_after);
            // Move past the lock so the next round can re-lock.
            now += LOCK_STEPS_MS[LOCK_STEPS_MS.len() - 1];
        }
        assert_eq!(seen, vec![30, 120, 600, 1800, 1800, 1800]);
    }

    #[test]
    fn success_clears_the_bucket() {
        let l = Limiter::new();
        l.record_fail("ip", T0);
        l.record_fail("ip", T0 + 1);
        l.record_success("ip");
        assert_eq!(l.record_fail("ip", T0 + 2).remaining_before_lock, 4);
    }

    #[test]
    fn failure_window_resets_an_unlocked_entry() {
        let l = Limiter::new();
        l.record_fail("ip", T0);
        l.record_fail("ip", T0 + 1);
        // An hour and a second later the entry is gone.
        assert_eq!(
            l.record_fail("ip", T0 + FAIL_WINDOW_MS + 1000)
                .remaining_before_lock,
            4
        );
    }

    #[test]
    fn an_expired_lock_inside_the_window_still_holds_its_level() {
        let l = Limiter::new();
        for _ in 0..MAX_FAILS_BEFORE_LOCK {
            l.record_fail("ip", T0);
        }
        // 30s later the lock has expired but the failure window has not, so the
        // entry survives and the next lock escalates.
        let after = T0 + 30_000;
        assert!(!l.check_lock("ip", after).locked);
        for _ in 0..MAX_FAILS_BEFORE_LOCK {
            l.record_fail("ip", after);
        }
        assert_eq!(l.check_lock("ip", after).retry_after, 120);
    }

    #[test]
    fn entry_is_dropped_once_the_window_lapses_and_the_lock_expires() {
        let l = Limiter::new();
        for _ in 0..MAX_FAILS_BEFORE_LOCK {
            l.record_fail("ip", T0);
        }
        // An hour later both conditions hold: the entry is deleted outright.
        let late = T0 + FAIL_WINDOW_MS + 1;
        assert!(!l.check_lock("ip", late).locked);
        // Level is back to 0, so the next lock is 30s again, not 120s.
        for _ in 0..MAX_FAILS_BEFORE_LOCK {
            l.record_fail("ip", late);
        }
        assert_eq!(l.check_lock("ip", late).retry_after, 30);
    }

    #[test]
    fn an_expired_lock_is_not_locked_but_keeps_its_level() {
        let l = Limiter::new();
        for _ in 0..MAX_FAILS_BEFORE_LOCK {
            l.record_fail("ip", T0);
        }
        let after = T0 + 30_000;
        assert!(!l.check_lock("ip", after).locked);
        for _ in 0..MAX_FAILS_BEFORE_LOCK {
            l.record_fail("ip", after);
        }
        assert_eq!(l.check_lock("ip", after).retry_after, 120, "level 1 now");
    }

    #[test]
    fn the_map_stays_bounded_under_ip_churn() {
        let l = Limiter::new();
        let mut last = String::new();
        for i in 0..(MAX_TRACKED_IPS as i64 + 500) {
            last = format!("10.{}.{}.{}", i / 65536, (i / 256) % 256, i % 256);
            l.record_fail(&last, T0 + i);
        }
        let len = l.attempts.lock().unwrap().len();
        assert!(len <= MAX_TRACKED_IPS, "bounded at {len}");
        // The most recently failing bucket survived the eviction.
        assert_eq!(
            l.record_fail(&last, T0 + 1_000_000).remaining_before_lock,
            3,
            "the fresh bucket's first fail is still counted"
        );
    }

    #[test]
    fn distinct_ips_have_distinct_buckets() {
        let l = Limiter::new();
        for _ in 0..MAX_FAILS_BEFORE_LOCK {
            l.record_fail("a", T0);
        }
        assert!(l.check_lock("a", T0).locked);
        assert!(!l.check_lock("b", T0).locked);
    }

    #[test]
    fn client_ip_prefers_real_ip_then_xff_from_loopback() {
        let loopback: IpAddr = "127.0.0.1".parse().unwrap();
        let mapped: IpAddr = "::ffff:127.0.0.1".parse().unwrap();
        let public: IpAddr = "203.0.113.9".parse().unwrap();

        assert_eq!(client_ip(loopback, None, None), "127.0.0.1");
        assert_eq!(
            client_ip(mapped, Some("10.0.0.1, 10.0.0.2"), None),
            "10.0.0.1"
        );
        assert_eq!(
            client_ip(loopback, Some("10.0.0.1"), Some("10.0.0.9")),
            "10.0.0.9"
        );
        // A non-loopback peer keeps its own address; the header is ignored.
        assert_eq!(client_ip(public, Some("10.0.0.1"), None), "203.0.113.9");
        // An empty header falls back to the socket.
        assert_eq!(client_ip(loopback, Some(""), Some("")), "127.0.0.1");
        assert_eq!(client_ip(loopback, Some("  10.0.0.1  "), None), "10.0.0.1");
    }

    #[test]
    fn loopback_hostnames_cover_the_reference_forms() {
        for yes in [
            "localhost",
            "LOCALHOST",
            "127.0.0.1",
            "127.0.0.1:20129",
            "::1",
            "[::1]",
            "[::1]:20129",
            "::ffff:127.0.0.1",
        ] {
            assert!(is_loopback_hostname(yes), "{yes} should be loopback");
        }
        for no in ["", "example.com", "10.0.0.1", "[::1", "192.168.1.1:80"] {
            assert!(!is_loopback_hostname(no), "{no} should not be loopback");
        }
    }
}
