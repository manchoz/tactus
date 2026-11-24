//! Persistent `/var/lib/tactus/config.toml` schema, defaults, normalization, and validation.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::Direction;

/// Maximum simultaneous MIDI output endpoints supported across config, control plane, and RT.
pub const MAX_MIDI_OUTPUTS: usize = 8;

/// Trim, drop empty entries, deduplicate case-insensitively (preserving order), and cap at [`MAX_MIDI_OUTPUTS`].
#[must_use]
pub fn normalize_output_matches(patterns: Vec<String>) -> Vec<String> {
    let mut normalized: Vec<String> = Vec::with_capacity(patterns.len().min(MAX_MIDI_OUTPUTS));
    for raw in patterns {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        if normalized
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(trimmed))
        {
            continue;
        }
        normalized.push(trimmed.to_string());
        if normalized.len() >= MAX_MIDI_OUTPUTS {
            break;
        }
    }
    normalized
}

/// Error returned when a [`TactusConfig`] fails bounds or invariant validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    /// Dotted TOML field path that failed validation (e.g. `"sync.quantum"`).
    pub field: &'static str,
    /// Human-readable description of the violated bound or invariant.
    pub message: &'static str,
}

impl ConfigError {
    const fn new(field: &'static str, message: &'static str) -> Self {
        Self { field, message }
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid config field `{}`: {}", self.field, self.message)
    }
}

impl std::error::Error for ConfigError {}

/// Persistent appliance configuration (`/var/lib/tactus/config.toml`).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TactusConfig {
    /// Synchronization engine parameters (`[sync]`).
    pub sync: SyncConfig,
    /// ALSA MIDI port matching configuration (`[midi]`).
    pub midi: MidiConfig,
    /// Embedded Web UI HTTP/WebSocket server configuration (`[web]`).
    pub web: WebConfig,
    /// Optional STM32U585 Zephyr RPC bridge configuration (`[mcu_bridge]`).
    pub mcu_bridge: McuBridgeConfig,
}

impl TactusConfig {
    /// Validate all configuration fields against `SPEC.md` bounds.
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.sync.validate()
    }
}

/// Sync engine parameters (`[sync]` section).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SyncConfig {
    /// Active sync direction (`link_to_midi` or `midi_to_link`).
    pub direction: Direction,
    /// Synchronize MIDI `Start (0xFA)`/`Stop (0xFC)` with Ableton Link transport.
    pub start_stop_sync: bool,
    /// Relay incoming MIDI Clock/transport directly to outputs (excluding input FD) in `MidiToLink`.
    pub clock_thru: bool,
    /// Ableton Link bar/phrase quantum in beats (finite, `> 0.0`; default `4.0`).
    pub quantum: f64,
    /// Startup Ableton Link session tempo in BPM (`20.0..=999.0`; default `120.0`).
    pub initial_bpm: f64,
    /// Round committed `MidiToLink` BPM estimates to whole integers (`20.0..=999.0`).
    pub round_bpm: bool,
    /// Signed MIDI Clock latency compensation in milliseconds (`-50.0..=50.0`; default `0.0`).
    pub midi_offset_ms: f32,
    /// Long OLS tempo window in seconds (`>= fast_window_secs` and `<= 30.0`; default `10.0`).
    pub ols_window_secs: f64,
    /// Fast OLS step-detection window in seconds (finite, `> 0.0`; default `1.5`).
    pub fast_window_secs: f64,
    /// Minimum BPM delta required to update Link tempo when locked (`>= 0.0`; default `0.05`).
    pub bpm_deadband: f64,
    /// Fast-vs-long BPM divergence threshold in BPM to trigger step-change tracking (`> 0.0`; default `1.5`).
    pub step_change_bpm: f64,
    /// Consecutive beats of divergence required before committing a step tempo jump (`>= 1`; default `3`).
    pub step_change_beats: u32,
    /// Phase error deadband in milliseconds below which phase nudges are suppressed (`>= 0.0`; default `4.0`).
    pub phase_deadband_ms: f32,
    /// Maximum phase correction nudge applied per quarter-note beat in milliseconds (`> 0.0`; default `1.0`).
    pub max_phase_nudge_ms: f32,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            direction: Direction::LinkToMidi,
            start_stop_sync: true,
            clock_thru: true,
            quantum: 4.0,
            initial_bpm: 120.0,
            round_bpm: false,
            midi_offset_ms: 0.0,
            ols_window_secs: 10.0,
            fast_window_secs: 1.5,
            bpm_deadband: 0.05,
            step_change_bpm: 1.5,
            step_change_beats: 3,
            phase_deadband_ms: 4.0,
            max_phase_nudge_ms: 1.0,
        }
    }
}

impl SyncConfig {
    /// Validate `[sync]` parameters against `SPEC.md §2` bounds.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if !self.quantum.is_finite() || self.quantum <= 0.0 {
            return Err(ConfigError::new(
                "sync.quantum",
                "must be finite and greater than 0.0",
            ));
        }
        if !self.initial_bpm.is_finite() || !(20.0..=999.0).contains(&self.initial_bpm) {
            return Err(ConfigError::new(
                "sync.initial_bpm",
                "must be finite and within 20.0..=999.0 BPM",
            ));
        }
        if !self.midi_offset_ms.is_finite() || !(-50.0..=50.0).contains(&self.midi_offset_ms) {
            return Err(ConfigError::new(
                "sync.midi_offset_ms",
                "must be finite and within -50.0..=50.0 ms",
            ));
        }
        if !self.fast_window_secs.is_finite() || self.fast_window_secs <= 0.0 {
            return Err(ConfigError::new(
                "sync.fast_window_secs",
                "must be finite and greater than 0.0",
            ));
        }
        if !self.ols_window_secs.is_finite()
            || self.ols_window_secs < self.fast_window_secs
            || self.ols_window_secs > 30.0
        {
            return Err(ConfigError::new(
                "sync.ols_window_secs",
                "must be finite, >= fast_window_secs, and <= 30.0",
            ));
        }
        if !self.bpm_deadband.is_finite() || self.bpm_deadband < 0.0 {
            return Err(ConfigError::new(
                "sync.bpm_deadband",
                "must be finite and >= 0.0",
            ));
        }
        if !self.step_change_bpm.is_finite() || self.step_change_bpm <= 0.0 {
            return Err(ConfigError::new(
                "sync.step_change_bpm",
                "must be finite and greater than 0.0",
            ));
        }
        if self.step_change_beats < 1 {
            return Err(ConfigError::new("sync.step_change_beats", "must be >= 1"));
        }
        if !self.phase_deadband_ms.is_finite() || self.phase_deadband_ms < 0.0 {
            return Err(ConfigError::new(
                "sync.phase_deadband_ms",
                "must be finite and >= 0.0",
            ));
        }
        if !self.max_phase_nudge_ms.is_finite() || self.max_phase_nudge_ms <= 0.0 {
            return Err(ConfigError::new(
                "sync.max_phase_nudge_ms",
                "must be finite and greater than 0.0",
            ));
        }
        Ok(())
    }
}

/// ALSA MIDI (RawMIDI or Sequencer) port matching rules (`[midi]` section).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "RawMidiConfig")]
pub struct MidiConfig {
    /// Exact port ID or case-insensitive substring rule selecting the MIDI input port.
    pub input_port_match: String,
    /// Ordered match rules (up to [`MAX_MIDI_OUTPUTS`]) selecting MIDI output fan-out endpoints.
    pub output_port_matches: Vec<String>,
}

#[derive(Deserialize)]
struct RawMidiConfig {
    input_port_match: Option<String>,
    output_port_matches: Option<Vec<String>>,
    output_port_match: Option<String>,
}

impl From<RawMidiConfig> for MidiConfig {
    fn from(raw: RawMidiConfig) -> Self {
        let input_port_match = raw.input_port_match.unwrap_or_else(|| "WIDI".to_string());
        let output_port_matches = if let Some(list) = raw.output_port_matches {
            normalize_output_matches(list)
        } else if let Some(single) = raw.output_port_match {
            normalize_output_matches(vec![single])
        } else {
            vec!["WIDI".to_string()]
        };
        Self {
            input_port_match,
            output_port_matches,
        }
    }
}

impl Default for MidiConfig {
    fn default() -> Self {
        Self {
            input_port_match: "WIDI".to_string(),
            output_port_matches: vec!["WIDI".to_string()],
        }
    }
}

/// Embedded HTTP/WebSocket UI server configuration (`[web]` section).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WebConfig {
    /// Socket address to bind the embedded HTTP/WebSocket server (default `"0.0.0.0:80"`).
    pub listen_addr: String,
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            listen_addr: "0.0.0.0:80".to_string(),
        }
    }
}

/// STM32U585 Zephyr RPC bridge configuration (`[mcu_bridge]` section).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct McuBridgeConfig {
    /// Enable the optional `Arduino_RouterBridge` Unix-socket client (default `false`).
    pub enabled: bool,
    /// Path to the `arduino-router` MessagePack-RPC Unix socket (default `"/var/run/arduino-router.sock"`).
    pub socket_path: String,
}

impl Default for McuBridgeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            socket_path: "/var/run/arduino-router.sock".to_string(),
        }
    }
}
