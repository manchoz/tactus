use serde::{Deserialize, Serialize};

use crate::TelemetrySnapshot;

/// Metadata for an ALSA Sequencer MIDI port discovered on the async control plane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MidiPortInfo {
    /// ALSA Sequencer client ID.
    pub client_id: i32,
    /// ALSA Sequencer port ID within the client.
    pub port_id: i32,
    /// Human-readable ALSA client name.
    pub client_name: String,
    /// Human-readable ALSA port name.
    pub port_name: String,
    /// Whether the port supports MIDI input/subscription.
    pub can_read: bool,
    /// Whether the port supports MIDI output/publishing.
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
