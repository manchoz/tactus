use serde::{Deserialize, Serialize};

use crate::Direction;

/// Estimator lock state for `MidiToLink` tempo acquisition and tracking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LockState {
    /// No valid MIDI Clock stream is currently locked.
    #[default]
    Unlocked,
    /// Collecting initial fast-window ticks prior to committing BPM to Link.
    Acquiring,
    /// Tracking MIDI Clock via the long OLS window with step-change detection.
    Locked,
}

/// Strictly `Copy`, zero-allocation telemetry snapshot emitted by the RT thread over `rtrb`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TelemetrySnapshot {
    /// Current synchronization direction.
    pub direction: Direction,
    /// Current tempo estimator lock state.
    pub lock_state: LockState,
    /// Whether start/stop transport sync (`0xFA`/`0xFC`) is enabled.
    pub start_stop_sync: bool,
    /// Whether direct RT MIDI Clock/transport relay (`MidiToLink` mode) is enabled.
    pub clock_thru: bool,
    /// Current Ableton Link session tempo in BPM.
    pub bpm_link: f64,
    /// Long-window OLS MIDI BPM estimate, when available.
    pub bpm_midi_long: Option<f64>,
    /// Fast-window OLS MIDI BPM estimate, when available.
    pub bpm_midi_fast: Option<f64>,
    /// Measured beat-boundary phase error in milliseconds.
    pub phase_error_ms: f32,
    /// Current session phase within `[0.0, quantum)`.
    pub beat_phase: f64,
    /// Active quantum in beats.
    pub quantum: f64,
    /// Number of connected Ableton Link peers.
    pub link_peers: usize,
    /// Whether transport is currently playing.
    pub is_playing: bool,
    /// Active signed MIDI clock offset in milliseconds.
    pub midi_offset_ms: f32,
    /// Whether integer BPM rounding is active in `MidiToLink` mode.
    #[serde(default)]
    pub round_bpm: bool,
}

impl Default for TelemetrySnapshot {
    fn default() -> Self {
        Self {
            direction: Direction::LinkToMidi,
            lock_state: LockState::Unlocked,
            start_stop_sync: true,
            clock_thru: true,
            bpm_link: 120.0,
            bpm_midi_long: None,
            bpm_midi_fast: None,
            phase_error_ms: 0.0,
            beat_phase: 0.0,
            quantum: 4.0,
            link_peers: 0,
            is_playing: false,
            midi_offset_ms: 0.0,
            round_bpm: false,
        }
    }
}
