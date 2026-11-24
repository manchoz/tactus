//! Fixed-capacity circular sample buffer and numerically stable two-pass OLS regression.

/// Capacity of the long OLS window (`SPEC.md §2.3.3`: covers `30 s` at `340 BPM` or `10 s` at `999 BPM`).
pub const LONG_RING_CAPACITY: usize = 4096;

/// Capacity of the fast step-detection window (covers up to 512 BPM over 1.5 s with headroom).
pub const FAST_RING_CAPACITY: usize = 512;

/// Standard MIDI Clock resolution (pulses per quarter note).
pub const TICKS_PER_BEAT: f64 = 24.0;

/// Lower plausibility bound for estimated BPM.
pub const MIN_VALID_BPM: f64 = 20.0;

/// Upper plausibility bound for estimated BPM.
pub const MAX_VALID_BPM: f64 = 999.0;

/// Single incoming `0xF8` clock tick paired with its monotonic timestamp.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClockSample {
    /// Monotonically increasing tick index inside the active acquisition window.
    pub tick_index: u64,
    /// Monotonic arrival timestamp (engine monotonic clock nanoseconds; `CLOCK_MONOTONIC_RAW` on Linux).
    pub timestamp_ns: i64,
}

/// Output of Ordinary Least Squares linear regression over a [`SampleRing`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OlsEstimate {
    /// Estimated tempo in Beats Per Minute.
    pub bpm: f64,
    /// Fitted clock period in nanoseconds per MIDI tick (`1/24` beat).
    pub slope_ns_per_tick: f64,
    /// Denoised OLS timestamp prediction for the newest sample in the ring.
    pub predicted_newest_ns: i64,
}

/// Fixed-capacity zero-allocation circular buffer of [`ClockSample`]s.
#[derive(Debug, Clone)]
pub struct SampleRing<const N: usize> {
    buf: [ClockSample; N],
    head: usize,
    len: usize,
}

impl<const N: usize> SampleRing<N> {
    /// Construct an empty [`SampleRing`].
    #[must_use]
    pub const fn new() -> Self {
        assert!(N > 0, "SampleRing capacity must be non-zero");
        Self {
            buf: [ClockSample {
                tick_index: 0,
                timestamp_ns: 0,
            }; N],
            head: 0,
            len: 0,
        }
    }

    /// Discard all samples without deallocating.
    pub fn clear(&mut self) {
        self.head = 0;
        self.len = 0;
    }

    /// Current number of samples in the ring.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Returns `true` if no samples are stored.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Oldest retained sample, if any.
    #[must_use]
    pub fn oldest(&self) -> Option<ClockSample> {
        if self.len == 0 {
            None
        } else {
            Some(self.buf[self.head])
        }
    }

    /// Newest retained sample, if any.
    #[must_use]
    pub fn newest(&self) -> Option<ClockSample> {
        if self.len == 0 {
            None
        } else {
            let tail = (self.head + self.len - 1) % N;
            Some(self.buf[tail])
        }
    }

    /// Append a sample, overwriting the oldest entry when full.
    pub fn push(&mut self, sample: ClockSample) {
        if self.len < N {
            let tail = (self.head + self.len) % N;
            self.buf[tail] = sample;
            self.len += 1;
        } else {
            self.buf[self.head] = sample;
            self.head = (self.head + 1) % N;
        }
    }

    /// Drop samples from the front whose `timestamp_ns < cutoff_ns`.
    pub fn evict_older_than(&mut self, cutoff_ns: i64) {
        self.evict_older_than_keeping(cutoff_ns, 0);
    }

    /// Drop samples older than `cutoff_ns` while keeping at least `min_keep` newest samples.
    pub fn evict_older_than_keeping(&mut self, cutoff_ns: i64, min_keep: usize) {
        while self.len > min_keep && self.buf[self.head].timestamp_ns < cutoff_ns {
            self.head = (self.head + 1) % N;
            self.len -= 1;
        }
    }

    /// Return the active samples as two contiguous slices (`head..end`, `0..tail`).
    #[must_use]
    pub fn as_slices(&self) -> (&[ClockSample], &[ClockSample]) {
        if self.len == 0 {
            return (&[], &[]);
        }
        let first_len = self.len.min(N - self.head);
        let second_len = self.len - first_len;
        (
            &self.buf[self.head..self.head + first_len],
            &self.buf[..second_len],
        )
    }

    /// Compute a shifted two-pass Ordinary Least Squares tempo and newest-timestamp estimate.
    ///
    /// Shifting coordinates relative to the oldest sample `(x0, y0)` avoids precision loss
    /// even when `timestamp_ns` is at multi-day/multi-week monotonic uptimes.
    #[must_use]
    pub fn ols_estimate(&self) -> Option<OlsEstimate> {
        if self.len < 2 {
            return None;
        }

        let oldest = self.oldest()?;
        let newest = self.newest()?;
        let x0 = oldest.tick_index;
        let y0 = oldest.timestamp_ns;
        let (first, second) = self.as_slices();

        let mut sum_x = 0.0_f64;
        let mut sum_y = 0.0_f64;
        for slice in [first, second] {
            for sample in slice {
                let dx = sample.tick_index.saturating_sub(x0) as f64;
                let dy = sample.timestamp_ns.saturating_sub(y0) as f64;
                sum_x += dx;
                sum_y += dy;
            }
        }

        let count = self.len as f64;
        let mean_x = sum_x / count;
        let mean_y = sum_y / count;

        let mut s_xx = 0.0_f64;
        let mut s_xy = 0.0_f64;
        for slice in [first, second] {
            for sample in slice {
                let cx = (sample.tick_index.saturating_sub(x0) as f64) - mean_x;
                let cy = (sample.timestamp_ns.saturating_sub(y0) as f64) - mean_y;
                s_xx += cx * cx;
                s_xy += cx * cy;
            }
        }

        if !s_xx.is_finite() || s_xx <= 0.0 || !s_xy.is_finite() {
            return None;
        }

        let slope = s_xy / s_xx;
        if !slope.is_finite() || slope <= 0.0 {
            return None;
        }

        let bpm = 60_000_000_000.0 / (TICKS_PER_BEAT * slope);
        if !bpm.is_finite() || !(MIN_VALID_BPM..=MAX_VALID_BPM).contains(&bpm) {
            return None;
        }

        let dx_newest = newest.tick_index.saturating_sub(x0) as f64;
        let predicted_dy = mean_y + slope * (dx_newest - mean_x);
        if !predicted_dy.is_finite() {
            return None;
        }
        let predicted_newest_ns = y0.saturating_add(predicted_dy.round() as i64);

        Some(OlsEstimate {
            bpm,
            slope_ns_per_tick: slope,
            predicted_newest_ns,
        })
    }
}

impl<const N: usize> Default for SampleRing<N> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_wraparound_and_eviction() {
        let mut ring: SampleRing<4> = SampleRing::new();
        for i in 0..6_u64 {
            ring.push(ClockSample {
                tick_index: i,
                timestamp_ns: (i as i64) * 10_000_000,
            });
        }
        assert_eq!(ring.len(), 4);
        assert_eq!(ring.oldest().map(|s| s.tick_index), Some(2));
        assert_eq!(ring.newest().map(|s| s.tick_index), Some(5));

        ring.evict_older_than(35_000_000);
        assert_eq!(ring.len(), 2);
        assert_eq!(ring.oldest().map(|s| s.tick_index), Some(4));

        ring.evict_older_than_keeping(99_000_000, 1);
        assert_eq!(ring.len(), 1);
        assert_eq!(ring.oldest().map(|s| s.tick_index), Some(5));
    }

    #[test]
    fn ols_stays_accurate_at_high_uptime() {
        // ~11.5 days uptime in nanoseconds (~10^15 ns).
        let base_ns: i64 = 1_000_000_000_000_000;
        let bpm = 123.456_f64;
        let tick_ns = 60_000_000_000.0 / (24.0 * bpm);

        let mut ring: SampleRing<LONG_RING_CAPACITY> = SampleRing::new();
        for i in 0..240_u64 {
            ring.push(ClockSample {
                tick_index: i,
                timestamp_ns: base_ns + ((i as f64) * tick_ns).round() as i64,
            });
        }

        let est = ring.ols_estimate().expect("OLS estimate should exist");
        assert!((est.bpm - bpm).abs() < 1e-4, "got bpm {}", est.bpm);
        let expected_last = base_ns + (239.0 * tick_ns).round() as i64;
        assert!((est.predicted_newest_ns - expected_last).abs() <= 2);
    }

    #[test]
    fn long_ring_capacity_covers_512_bpm_for_10_seconds() {
        // 512 BPM * 24 PPQN / 60s * 10s = 2048 ticks -> 2048 samples fit within LONG_RING_CAPACITY.
        let mut ring: SampleRing<LONG_RING_CAPACITY> = SampleRing::new();
        let tick_ns = 60_000_000_000.0 / (24.0 * 512.0);
        for i in 0..2048_u64 {
            ring.push(ClockSample {
                tick_index: i,
                timestamp_ns: ((i as f64) * tick_ns).round() as i64,
            });
        }
        assert_eq!(ring.len(), 2048);
        let est = ring.ols_estimate().expect("OLS estimate at 512 BPM");
        assert!((est.bpm - 512.0).abs() < 0.01);
    }

    #[test]
    fn long_ring_capacity_covers_340_bpm_for_30_seconds() {
        // 340 BPM * 24 PPQN / 60s * 30s = 4080 ticks <= LONG_RING_CAPACITY (4096).
        let mut ring: SampleRing<LONG_RING_CAPACITY> = SampleRing::new();
        let tick_ns = 60_000_000_000.0 / (24.0 * 340.0);
        for i in 0..4080_u64 {
            ring.push(ClockSample {
                tick_index: i,
                timestamp_ns: ((i as f64) * tick_ns).round() as i64,
            });
        }
        assert_eq!(ring.len(), 4080);
        let est = ring.ols_estimate().expect("OLS estimate at 340 BPM");
        assert!((est.bpm - 340.0).abs() < 0.01);
    }

    #[test]
    fn as_slices_handles_empty_wrapped_and_evicted_rings() {
        let mut ring: SampleRing<4> = SampleRing::new();
        assert_eq!(ring.as_slices(), (&[][..], &[][..]));

        for i in 0..3_u64 {
            ring.push(ClockSample {
                tick_index: i,
                timestamp_ns: (i as i64) * 10_000_000,
            });
        }
        let (a, b) = ring.as_slices();
        let ticks: Vec<u64> = a.iter().chain(b.iter()).map(|s| s.tick_index).collect();
        assert_eq!(ticks, vec![0, 1, 2]);
        assert!(b.is_empty());

        for i in 3..6_u64 {
            ring.push(ClockSample {
                tick_index: i,
                timestamp_ns: (i as i64) * 10_000_000,
            });
        }
        let (a, b) = ring.as_slices();
        let ticks: Vec<u64> = a.iter().chain(b.iter()).map(|s| s.tick_index).collect();
        assert_eq!(ticks, vec![2, 3, 4, 5]);
        assert_eq!(a.len(), 2);
        assert_eq!(b.len(), 2);
        let est = ring.ols_estimate().expect("OLS across wrapped slices");
        assert!((est.bpm - 250.0).abs() < 1e-6);

        ring.evict_older_than(45_000_000);
        let (a, b) = ring.as_slices();
        let ticks: Vec<u64> = a.iter().chain(b.iter()).map(|s| s.tick_index).collect();
        assert_eq!(ticks, vec![5]);
        assert!(b.is_empty());
    }
}
