//! Clock-domain bridge between `abl_link`'s `CLOCK_MONOTONIC_RAW` and `CLOCK_MONOTONIC` (`timerfd`).

#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

/// Period between automatic `CLOCK_MONOTONIC` vs `CLOCK_MONOTONIC_RAW` anchor refreshes.
pub const CLOCK_BRIDGE_REFRESH_NS: i64 = 100_000_000;

/// Zero-allocation bridge translating `CLOCK_MONOTONIC_RAW` deadlines into `CLOCK_MONOTONIC`.
///
/// On Linux, Ableton Link (`abl_link_clock_micros()`) reads `CLOCK_MONOTONIC_RAW`, whereas
/// `timerfd_create(CLOCK_MONOTONIC, ...)` arms against NTP-slewed `CLOCK_MONOTONIC`. By running
/// `SyncEngine` in `CLOCK_MONOTONIC_RAW` nanoseconds and maintaining a bracketed offset sample
/// `CLOCK_MONOTONIC - CLOCK_MONOTONIC_RAW` every 100 ms, worst-case slew error stays under ~10 us
/// per 20 ms tick without accumulating drift.
#[derive(Debug, Clone, Copy, Default)]
pub struct ClockBridge {
    offset_mono_minus_raw_ns: i64,
    last_sample_raw_ns: i64,
    initialized: bool,
}

impl ClockBridge {
    /// Construct an uninitialized bridge (`offset == 0`).
    #[must_use]
    pub const fn new() -> Self {
        Self {
            offset_mono_minus_raw_ns: 0,
            last_sample_raw_ns: 0,
            initialized: false,
        }
    }

    /// Current sampled `CLOCK_MONOTONIC - CLOCK_MONOTONIC_RAW` offset in nanoseconds.
    #[cfg(test)]
    #[must_use]
    pub const fn offset_ns(&self) -> i64 {
        self.offset_mono_minus_raw_ns
    }

    /// Take 3 bracketed `(mono_a, raw, mono_b)` reads and pick the tightest bracket.
    pub fn sample_with(
        &mut self,
        mut read_mono: impl FnMut() -> i64,
        mut read_raw: impl FnMut() -> i64,
    ) {
        let mut best_width = i64::MAX;
        let mut best_offset = 0;
        let mut best_raw = 0;
        let mut found = false;

        for _ in 0..3 {
            let mono_a = read_mono();
            let raw = read_raw();
            let mono_b = read_mono();
            let width = mono_b.saturating_sub(mono_a);
            if width >= 0 && width < best_width {
                best_width = width;
                let mono_mid = mono_a.saturating_add(width / 2);
                best_offset = mono_mid.saturating_sub(raw);
                best_raw = raw;
                found = true;
            } else if !found {
                best_offset = mono_b.saturating_sub(raw);
                best_raw = raw;
            }
        }

        self.offset_mono_minus_raw_ns = best_offset;
        self.last_sample_raw_ns = best_raw;
        self.initialized = true;
    }

    /// Refresh the offset sample if uninitialized, forced, or stale (`>= 100 ms`).
    pub fn maybe_refresh_with(
        &mut self,
        now_raw_ns: i64,
        force: bool,
        read_mono: impl FnMut() -> i64,
        read_raw: impl FnMut() -> i64,
    ) -> bool {
        if !self.initialized
            || force
            || now_raw_ns.saturating_sub(self.last_sample_raw_ns).abs() >= CLOCK_BRIDGE_REFRESH_NS
        {
            self.sample_with(read_mono, read_raw);
            true
        } else {
            false
        }
    }

    /// Translate a `CLOCK_MONOTONIC_RAW` timestamp (ns) into `CLOCK_MONOTONIC` (ns).
    #[must_use]
    pub const fn raw_to_mono_ns(&self, target_raw_ns: i64) -> i64 {
        target_raw_ns.saturating_add(self.offset_mono_minus_raw_ns)
    }
}

/// Sample the OS `CLOCK_MONOTONIC` and `CLOCK_MONOTONIC_RAW` domains on Linux.
#[cfg(target_os = "linux")]
impl ClockBridge {
    /// Sample real Linux `CLOCK_MONOTONIC` vs `CLOCK_MONOTONIC_RAW`.
    pub fn sample_os(&mut self) {
        self.sample_with(now_mono_ns, now_raw_ns);
    }

    /// Refresh the bridge against OS clocks when due or forced.
    pub fn maybe_refresh_os(&mut self, now_raw_ns_val: i64, force: bool) -> bool {
        self.maybe_refresh_with(now_raw_ns_val, force, now_mono_ns, now_raw_ns)
    }
}

/// Convert an absolute monotonic nanosecond timestamp into `(tv_sec, tv_nsec)` for `timerfd`.
///
/// Values `<= 0` are clamped to `1 ns` because `itimerspec { 0, 0 }` disarms `timerfd_settime`.
#[must_use]
pub const fn abs_ns_to_sec_nsec(abs_mono_ns: i64) -> (i64, i64) {
    let clamped = if abs_mono_ns <= 0 { 1 } else { abs_mono_ns };
    (clamped / 1_000_000_000, clamped % 1_000_000_000)
}

/// Build an absolute `itimerspec` with `it_interval = 0` and `it_value >= 1 ns`.
#[cfg(target_os = "linux")]
#[must_use]
#[allow(clippy::unnecessary_cast)]
pub fn abs_ns_to_itimerspec(abs_mono_ns: i64) -> libc::itimerspec {
    let (sec, nsec) = abs_ns_to_sec_nsec(abs_mono_ns);
    libc::itimerspec {
        it_interval: libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        },
        it_value: libc::timespec {
            tv_sec: sec as libc::time_t,
            tv_nsec: nsec as libc::c_long,
        },
    }
}

/// Read `CLOCK_MONOTONIC` in nanoseconds.
#[must_use]
pub fn now_mono_ns() -> i64 {
    read_clock_ns(libc::CLOCK_MONOTONIC)
}

/// Read `CLOCK_MONOTONIC_RAW` in nanoseconds on Linux.
#[cfg(target_os = "linux")]
#[must_use]
pub fn now_raw_ns() -> i64 {
    read_clock_ns(libc::CLOCK_MONOTONIC_RAW)
}

#[allow(clippy::unnecessary_cast)]
fn read_clock_ns(clock_id: libc::clockid_t) -> i64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let rc = unsafe { libc::clock_gettime(clock_id, &raw mut ts) };
    if rc != 0 {
        return 0;
    }
    (ts.tv_sec as i64)
        .saturating_mul(1_000_000_000)
        .saturating_add(ts.tv_nsec as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_mono_ns_is_non_negative_and_monotonic() {
        let t1 = now_mono_ns();
        let t2 = now_mono_ns();
        assert!(t1 >= 0);
        assert!(t2 >= t1);
    }

    #[test]
    fn sample_with_picks_tightest_bracket_under_preemption_jitter() {
        let mut bridge = ClockBridge::new();

        // Reads: (mono_a, mono_b) pairs paired with raw reads:
        // Probe 0: wide preemption bracket [1000, 50_000], raw = 500
        // Probe 1: tightest bracket [10_200, 10_210], raw = 205 -> offset = 10_205 - 205 = 10_000
        // Probe 2: wider bracket [20_000, 21_000], raw = 10_200
        let mono_seq = [1_000, 50_000, 10_200, 10_210, 20_000, 21_000];
        let raw_seq = [500, 205, 10_200];
        let mut m_idx = 0;
        let mut r_idx = 0;

        bridge.sample_with(
            || {
                let v = mono_seq[m_idx];
                m_idx += 1;
                v
            },
            || {
                let v = raw_seq[r_idx];
                r_idx += 1;
                v
            },
        );

        assert_eq!(bridge.offset_ns(), 10_000);
        assert_eq!(bridge.raw_to_mono_ns(5_000_000), 5_010_000);
    }

    #[test]
    fn supports_negative_offset_and_slew_refresh() {
        let mut bridge = ClockBridge::new();
        let mut raw_base = 1_000_000_000i64;
        let mut offset = -25_000i64;

        let changed = bridge.maybe_refresh_with(raw_base, false, || raw_base + offset, || raw_base);
        assert!(changed);
        assert_eq!(bridge.offset_ns(), -25_000);
        assert_eq!(bridge.raw_to_mono_ns(1_050_000_000), 1_049_975_000);

        // Before 100 ms interval: does not refresh unless forced.
        offset = -40_000;
        assert!(!bridge.maybe_refresh_with(
            raw_base + 50_000_000,
            false,
            || raw_base + 50_000_000 + offset,
            || raw_base + 50_000_000,
        ));
        assert_eq!(bridge.offset_ns(), -25_000);

        // After >= 100 ms: re-anchors to new slewed offset.
        raw_base += CLOCK_BRIDGE_REFRESH_NS;
        assert!(bridge.maybe_refresh_with(raw_base, false, || raw_base + offset, || raw_base,));
        assert_eq!(bridge.offset_ns(), -40_000);
    }

    #[test]
    fn abs_ns_to_sec_nsec_clamps_non_positive_so_timerfd_never_disarms() {
        assert_eq!(abs_ns_to_sec_nsec(0), (0, 1));
        assert_eq!(abs_ns_to_sec_nsec(-500_000), (0, 1));
        assert_eq!(abs_ns_to_sec_nsec(1), (0, 1));
        assert_eq!(abs_ns_to_sec_nsec(2_500_000_123), (2, 500_000_123));
    }
}
