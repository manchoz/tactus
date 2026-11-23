use serde::{Deserialize, Serialize};

use crate::TelemetrySnapshot;

/// Metadata for an ALSA MIDI port discovered on the async control plane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MidiPortInfo {
    /// Stable backend port identifier (e.g. `"hw:1,0,0"` for RawMIDI, `"20:0"` for `snd_seq`).
    pub id: String,
    /// Human-readable hardware/card/client name (e.g. `"WIDI Uhost"`).
    pub card_name: String,
    /// Human-readable device/subdevice port name (e.g. `"WIDI Uhost MIDI 1"`).
    pub port_name: String,
    /// Whether the port supports MIDI input.
    pub can_read: bool,
    /// Whether the port supports MIDI output.
    pub can_write: bool,
}

/// Complete application state maintained on the `tokio` async control plane (`watch::Sender<AppState>`).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct AppState {
    /// Latest RT engine telemetry snapshot.
    pub telemetry: TelemetrySnapshot,
    /// Available ALSA MIDI ports enumerated outside the RT hot path.
    pub available_midi_ports: Vec<MidiPortInfo>,
    /// Currently selected MIDI input port match or identifier.
    pub selected_midi_input: Option<String>,
    /// Currently selected MIDI output port match or identifier.
    pub selected_midi_output: Option<String>,
}
