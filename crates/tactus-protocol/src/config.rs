use std::fmt;

use serde::{Deserialize, Serialize};

use crate::Direction;

/// Error returned when a [`TactusConfig`] fails bounds or invariant validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    pub field: &'static str,
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
    pub sync: SyncConfig,
    pub midi: MidiConfig,
    pub web: WebConfig,
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
    pub direction: Direction,
    pub start_stop_sync: bool,
    pub quantum: f64,
    pub midi_offset_ms: f32,
    pub ols_window_secs: f64,
    pub fast_window_secs: f64,
    pub bpm_deadband: f64,
    pub step_change_bpm: f64,
    pub step_change_beats: u32,
    pub phase_deadband_ms: f32,
    pub max_phase_nudge_ms: f32,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            direction: Direction::LinkToMidi,
            start_stop_sync: true,
            quantum: 4.0,
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
        if !self.ols_window_secs.is_finite() || self.ols_window_secs < self.fast_window_secs {
            return Err(ConfigError::new(
                "sync.ols_window_secs",
                "must be finite and >= fast_window_secs",
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

/// ALSA Sequencer port matching rules (`[midi]` section).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MidiConfig {
    pub input_port_match: String,
    pub output_port_match: String,
}

impl Default for MidiConfig {
    fn default() -> Self {
        Self {
            input_port_match: "WIDI".to_string(),
            output_port_match: "WIDI".to_string(),
        }
    }
}

/// Embedded HTTP/WebSocket UI server configuration (`[web]` section).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WebConfig {
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
    pub enabled: bool,
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
