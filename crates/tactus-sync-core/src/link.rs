//! Zero-cost abstraction over an Ableton Link session state and clock conversion helpers.

/// Convert monotonic nanoseconds to Link clock microseconds (rounded to nearest microsecond).
#[inline]
#[must_use]
pub(crate) fn ns_to_us(ns: i64) -> i64 {
    ns.saturating_add(500).div_euclid(1_000)
}

/// Convert Link clock microseconds to monotonic nanoseconds.
#[inline]
#[must_use]
pub(crate) fn us_to_ns(us: i64) -> i64 {
    us.saturating_mul(1_000)
}

/// Convert signed millisecond latency offset to nanoseconds.
#[inline]
#[must_use]
pub(crate) fn offset_ms_to_ns(offset_ms: f32) -> i64 {
    (f64::from(offset_ms) * 1_000_000.0).round() as i64
}

/// Abstraction over an `abl_link` session state captured on the RT thread.
///
/// Implemented over real `tactus-link-sys` handles in `tactusd` (M3) and over deterministic
/// simulation mocks in `tactus-sync-core` tests.
pub trait LinkSession {
    /// Number of currently connected Link peers.
    fn num_peers(&self) -> usize;

    /// Current timeline tempo in BPM.
    fn tempo(&self) -> f64;

    /// Map a Link clock timestamp (`time_us`) to beats for `quantum`.
    fn beat_at_time(&self, time_us: i64, quantum: f64) -> f64;

    /// Query the session phase in `[0, quantum)` at `time_us`.
    fn phase_at_time(&self, time_us: i64, quantum: f64) -> f64;

    /// Map a beat value to the Link clock timestamp (`time_us`) for `quantum`.
    fn time_at_beat(&self, beat: f64, quantum: f64) -> i64;

    /// Return whether Link transport is currently playing.
    fn is_playing(&self) -> bool;

    /// Set timeline tempo to `bpm`, taking effect at `at_time_us`.
    fn set_tempo(&mut self, bpm: f64, at_time_us: i64);

    /// Request mapping `beat` to `time_us` (quantized when other peers are present).
    fn request_beat_at_time(&mut self, beat: f64, time_us: i64, quantum: f64);

    /// Unconditionally map `beat` to `time_us` across the Link session.
    fn force_beat_at_time(&mut self, beat: f64, time_us: i64, quantum: f64);

    /// Set Link transport playing state, taking effect at `at_time_us`.
    fn set_is_playing(&mut self, is_playing: bool, at_time_us: i64);

    /// Atomically set transport playing state and request `beat` at `time_us`.
    fn set_is_playing_and_request_beat_at_time(
        &mut self,
        is_playing: bool,
        time_us: i64,
        beat: f64,
        quantum: f64,
    );
}
