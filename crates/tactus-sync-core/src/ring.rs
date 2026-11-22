//! Fixed-capacity circular sample buffer and numerically stable two-pass OLS regression.

/// Capacity of the long OLS window (`SPEC.md §2.3.3`: covers up to 512 BPM over 10 s).
pub const LONG_RING_CAPACITY: usize = 2048;

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
    /// Monotonic arrival timestamp (`CLOCK_MONOTONIC` nanoseconds).
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
        while self.len > 0 && self.buf[self.head].timestamp_ns < cutoff_ns {
            self.head = (self.head + 1) % N;
            self.len -= 1;
        }
    }

    /// Iterate over retained samples from oldest to newest.
    pub fn iter(&self) -> SampleRingIter<'_, N> {
        SampleRingIter {
            ring: self,
            offset: 0,
        }
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

        let mut sum_x = 0.0_f64;
        let mut sum_y = 0.0_f64;
        for sample in self.iter() {
            let dx = sample.tick_index.saturating_sub(x0) as f64;
            let dy = sample.timestamp_ns.saturating_sub(y0) as f64;
            sum_x += dx;
            sum_y += dy;
        }

        let count = self.len as f64;
        let mean_x = sum_x / count;
        let mean_y = sum_y / count;

        let mut s_xx = 0.0_f64;
        let mut s_xy = 0.0_f64;
        for sample in self.iter() {
            let cx = (sample.tick_index.saturating_sub(x0) as f64) - mean_x;
            let cy = (sample.timestamp_ns.saturating_sub(y0) as f64) - mean_y;
            s_xx += cx * cx;
            s_xy += cx * cy;
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

/// Zero-allocation iterator over [`SampleRing`] in chronological order.
pub struct SampleRingIter<'a, const N: usize> {
    ring: &'a SampleRing<N>,
    offset: usize,
}

impl<const N: usize> Iterator for SampleRingIter<'_, N> {
    type Item = ClockSample;

    fn next(&mut self) -> Option<Self::Item> {
        if self.offset >= self.ring.len {
            return None;
        }
        let idx = (self.ring.head + self.offset) % N;
        self.offset += 1;
        Some(self.ring.buf[idx])
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let rem = self.ring.len - self.offset;
        (rem, Some(rem))
    }
}

impl<const N: usize> ExactSizeIterator for SampleRingIter<'_, N> {}

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
        assert_eq!(ring.len(), LONG_RING_CAPACITY);
        let est = ring.ols_estimate().expect("OLS estimate at 512 BPM");
        assert!((est.bpm - 512.0).abs() < 0.01);
    }
}
