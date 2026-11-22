//! Mode B (`MidiToLink`): Dual-window OLS tempo estimator, step-change detector, and phase lock.

use tactus_protocol::{LockState, SyncConfig};

use crate::link::{ns_to_us, LinkSession};
use crate::ring::{ClockSample, SampleRing, FAST_RING_CAPACITY, LONG_RING_CAPACITY};

/// Incoming MIDI clock silence threshold before transitioning to `Unlocked` (`SPEC.md §2.3.5`).
pub const MIDI_TIMEOUT_NS: i64 = 500_000_000;

const MIN_OLS_TICKS: usize = 24;
const TICKS_PER_BEAT_U64: u64 = 24;

/// State machine implementing `SPEC.md §2.3` (`MidiToLink`).
#[derive(Debug, Clone)]
pub struct MidiToLinkEstimator {
    lock_state: LockState,
    long_ring: SampleRing<LONG_RING_CAPACITY>,
    fast_ring: SampleRing<FAST_RING_CAPACITY>,
    long_start_ns: Option<i64>,
    fast_start_ns: Option<i64>,
    monotonic_tick_index: u64,
    has_seen_tick: bool,
    song_tick: u64,
    expecting_downbeat_tick: bool,
    last_clock_rx_ns: Option<i64>,
    committed_bpm: Option<f64>,
    bpm_midi_long: Option<f64>,
    bpm_midi_fast: Option<f64>,
    phase_error_ms: f32,
    step_change_beats_count: u32,
    step_change_sign: i8,
    pending_resync_downbeat: bool,
}

impl MidiToLinkEstimator {
    /// Create a fresh unlocked estimator.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            lock_state: LockState::Unlocked,
            long_ring: SampleRing::new(),
            fast_ring: SampleRing::new(),
            long_start_ns: None,
            fast_start_ns: None,
            monotonic_tick_index: 0,
            has_seen_tick: false,
            song_tick: 0,
            expecting_downbeat_tick: true,
            last_clock_rx_ns: None,
            committed_bpm: None,
            bpm_midi_long: None,
            bpm_midi_fast: None,
            phase_error_ms: 0.0,
            step_change_beats_count: 0,
            step_change_sign: 0,
            pending_resync_downbeat: false,
        }
    }

    /// Reset estimator state and clear both OLS windows (`Unlocked`).
    pub fn reset(&mut self) {
        self.lock_state = LockState::Unlocked;
        self.long_ring.clear();
        self.fast_ring.clear();
        self.long_start_ns = None;
        self.fast_start_ns = None;
        self.monotonic_tick_index = 0;
        self.has_seen_tick = false;
        self.song_tick = 0;
        self.expecting_downbeat_tick = true;
        self.last_clock_rx_ns = None;
        self.committed_bpm = None;
        self.bpm_midi_long = None;
        self.bpm_midi_fast = None;
        self.phase_error_ms = 0.0;
        self.step_change_beats_count = 0;
        self.step_change_sign = 0;
        self.pending_resync_downbeat = false;
    }

    /// Current acquisition/lock state.
    #[must_use]
    pub const fn lock_state(&self) -> LockState {
        self.lock_state
    }

    /// Latest long-window OLS BPM estimate, if ready.
    #[must_use]
    pub const fn bpm_midi_long(&self) -> Option<f64> {
        self.bpm_midi_long
    }

    /// Latest fast-window OLS BPM estimate, if ready.
    #[must_use]
    pub const fn bpm_midi_fast(&self) -> Option<f64> {
        self.bpm_midi_fast
    }

    /// Most recently measured quarter-note phase error in milliseconds.
    #[must_use]
    pub const fn phase_error_ms(&self) -> f32 {
        self.phase_error_ms
    }

    /// Next monotonic timestamp when a MIDI clock timeout will trigger, if active.
    #[must_use]
    pub fn next_timeout_deadline_ns(&self) -> Option<i64> {
        self.last_clock_rx_ns
            .map(|last| last.saturating_add(MIDI_TIMEOUT_NS))
    }

    /// Arm a quantized downbeat alignment on the next quarter-note boundary.
    pub fn request_resync_downbeat(&mut self) {
        self.pending_resync_downbeat = true;
    }

    /// Check if MIDI Clock has been silent for `> 500 ms`; if so, reset to `Unlocked`.
    pub fn check_timeout(&mut self, now_ns: i64) -> bool {
        if let Some(last_ns) = self.last_clock_rx_ns {
            if now_ns.saturating_sub(last_ns) > MIDI_TIMEOUT_NS {
                self.reset();
                return true;
            }
        }
        false
    }

    /// Handle incoming MIDI `Start (0xFA)`.
    pub fn on_start(&mut self, now_ns: i64, config: &SyncConfig, link: &mut impl LinkSession) {
        self.song_tick = 0;
        self.expecting_downbeat_tick = true;
        self.phase_error_ms = 0.0;

        let now_us = ns_to_us(now_ns);
        if config.start_stop_sync {
            link.set_is_playing_and_request_beat_at_time(true, now_us, 0.0, config.quantum);
        } else {
            link.request_beat_at_time(0.0, now_us, config.quantum);
        }
    }

    /// Handle incoming MIDI `Continue (0xFB)`.
    pub fn on_continue(&mut self, now_ns: i64, config: &SyncConfig, link: &mut impl LinkSession) {
        if config.start_stop_sync {
            link.set_is_playing(true, ns_to_us(now_ns));
        }
    }

    /// Handle incoming MIDI `Stop (0xFC)`.
    pub fn on_stop(&mut self, now_ns: i64, config: &SyncConfig, link: &mut impl LinkSession) {
        if config.start_stop_sync {
            link.set_is_playing(false, ns_to_us(now_ns));
        }
    }

    /// Process an incoming MIDI `Clock (0xF8)` tick timestamped at `now_ns`.
    pub fn on_clock(&mut self, now_ns: i64, config: &SyncConfig, link: &mut impl LinkSession) {
        self.check_timeout(now_ns);

        if self.has_seen_tick {
            self.monotonic_tick_index = self.monotonic_tick_index.saturating_add(1);
        } else {
            self.monotonic_tick_index = 0;
            self.has_seen_tick = true;
        }

        let was_downbeat = self.expecting_downbeat_tick;
        if self.expecting_downbeat_tick {
            self.song_tick = 0;
            self.expecting_downbeat_tick = false;
        } else {
            self.song_tick = self.song_tick.saturating_add(1);
        }
        let is_beat_boundary = !was_downbeat && self.song_tick.is_multiple_of(TICKS_PER_BEAT_U64);

        let sample = ClockSample {
            tick_index: self.monotonic_tick_index,
            timestamp_ns: now_ns,
        };
        if self.long_start_ns.is_none() {
            self.long_start_ns = Some(now_ns);
        }
        if self.fast_start_ns.is_none() {
            self.fast_start_ns = Some(now_ns);
        }
        self.long_ring.push(sample);
        self.fast_ring.push(sample);
        self.last_clock_rx_ns = Some(now_ns);

        let long_window_ns = secs_to_ns(config.ols_window_secs);
        let fast_window_ns = secs_to_ns(config.fast_window_secs);

        self.long_ring
            .evict_older_than(now_ns.saturating_sub(long_window_ns));
        self.fast_ring
            .evict_older_than(now_ns.saturating_sub(fast_window_ns));

        let long_ready = self
            .long_start_ns
            .is_some_and(|start| now_ns.saturating_sub(start) >= fast_window_ns)
            && self.long_ring.len() >= MIN_OLS_TICKS;
        let fast_ready = self
            .fast_start_ns
            .is_some_and(|start| now_ns.saturating_sub(start) >= fast_window_ns)
            && self.fast_ring.len() >= MIN_OLS_TICKS;

        let long_est = if long_ready {
            self.long_ring.ols_estimate()
        } else {
            None
        };
        let fast_est = if fast_ready {
            self.fast_ring.ols_estimate()
        } else {
            None
        };

        self.bpm_midi_long = long_est.map(|e| e.bpm);
        self.bpm_midi_fast = fast_est.map(|e| e.bpm);

        match self.lock_state {
            LockState::Unlocked => {
                self.lock_state = LockState::Acquiring;
            }
            LockState::Acquiring => {
                if let Some(est) = long_est {
                    link.set_tempo(est.bpm, ns_to_us(now_ns));
                    self.committed_bpm = Some(est.bpm);
                    self.lock_state = LockState::Locked;
                    self.step_change_beats_count = 0;
                    self.step_change_sign = 0;
                }
            }
            LockState::Locked => {
                let mut step_triggered = false;
                if is_beat_boundary {
                    if let (Some(fast), Some(committed)) = (fast_est, self.committed_bpm) {
                        let diff = fast.bpm - committed;
                        if diff.abs() > config.step_change_bpm {
                            let sign = if diff > 0.0 { 1 } else { -1 };
                            if sign == self.step_change_sign {
                                self.step_change_beats_count =
                                    self.step_change_beats_count.saturating_add(1);
                            } else {
                                self.step_change_sign = sign;
                                self.step_change_beats_count = 1;
                            }

                            if self.step_change_beats_count >= config.step_change_beats {
                                // Intentional tempo change: flush long window and commit fast BPM immediately.
                                self.long_ring.clear();
                                self.long_ring.push(sample);
                                self.long_start_ns = Some(now_ns);
                                self.bpm_midi_long = None;
                                link.set_tempo(fast.bpm, ns_to_us(now_ns));
                                self.committed_bpm = Some(fast.bpm);
                                self.step_change_beats_count = 0;
                                self.step_change_sign = 0;
                                step_triggered = true;
                            }
                        } else {
                            self.step_change_beats_count = 0;
                            self.step_change_sign = 0;
                        }
                    }
                }

                if !step_triggered {
                    if let (Some(long), Some(committed)) = (long_est, self.committed_bpm) {
                        if (long.bpm - committed).abs() > config.bpm_deadband {
                            link.set_tempo(long.bpm, ns_to_us(now_ns));
                            self.committed_bpm = Some(long.bpm);
                        }
                    }
                }

                if is_beat_boundary && !step_triggered {
                    let denoised_ns = long_est
                        .or(fast_est)
                        .map_or(now_ns, |est| est.predicted_newest_ns);
                    let beat_ts_us = ns_to_us(denoised_ns);

                    if self.pending_resync_downbeat {
                        self.pending_resync_downbeat = false;
                        link.request_beat_at_time(0.0, beat_ts_us, config.quantum);
                        self.phase_error_ms = 0.0;
                    } else {
                        let nearest_beat = link.beat_at_time(beat_ts_us, config.quantum).round();
                        let link_beat_time_us = link.time_at_beat(nearest_beat, config.quantum);
                        let phase_error_us = beat_ts_us.saturating_sub(link_beat_time_us);
                        let phase_error_ms = (phase_error_us as f64 / 1_000.0) as f32;
                        self.phase_error_ms = phase_error_ms;

                        if phase_error_ms.abs() > config.phase_deadband_ms {
                            let max_nudge_us =
                                (f64::from(config.max_phase_nudge_ms) * 1_000.0).round() as i64;
                            let nudge_us = phase_error_us.clamp(-max_nudge_us, max_nudge_us);
                            link.force_beat_at_time(
                                nearest_beat,
                                link_beat_time_us.saturating_add(nudge_us),
                                config.quantum,
                            );
                        }
                    }
                }
            }
        }
    }
}

impl Default for MidiToLinkEstimator {
    fn default() -> Self {
        Self::new()
    }
}

#[inline]
fn secs_to_ns(secs: f64) -> i64 {
    (secs * 1_000_000_000.0).round() as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ring::TICKS_PER_BEAT;

    #[test]
    fn ticks_per_beat_constant_matches_midi_spec() {
        assert_eq!(TICKS_PER_BEAT, 24.0);
        assert_eq!(TICKS_PER_BEAT_U64, 24);
    }
}
