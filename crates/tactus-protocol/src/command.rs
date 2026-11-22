use serde::{Deserialize, Serialize};

/// Active synchronization direction between Ableton Link and MIDI Clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// Ableton Link timeline drives outgoing 24 PPQN MIDI Clock.
    #[default]
    LinkToMidi,
    /// Incoming 24 PPQN MIDI Clock drives Ableton Link tempo and phase.
    MidiToLink,
}

/// Control-plane commands shared across WebSocket JSON and `arduino-router` RPC.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EngineCommand {
    /// Switch active sync direction.
    SetDirection(Direction),
    /// Enable or disable transport start/stop synchronization (`0xFA`/`0xFC`).
    SetStartStopSync(bool),
    /// Set signed MIDI Clock latency compensation in milliseconds (`-50.0..=50.0`).
    SetOffsetMs(f32),
    /// Set Ableton Link bar/phrase quantum in beats.
    SetQuantum(f64),
    /// Select ALSA MIDI input and output ports on the async control plane.
    SelectMidiPorts { input: String, output: String },
    /// Quantize/re-align downbeat phase on the active session.
    ResyncDownbeat,
    /// Manually set Link tempo in BPM (used in `LinkToMidi` mode).
    SetLinkTempoManual(f64),
}

/// Strictly `Copy`, zero-allocation command subset sent over `rtrb` to the `SCHED_FIFO` RT thread.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RtCommand {
    /// Switch active sync direction.
    SetDirection(Direction),
    /// Enable or disable transport start/stop synchronization.
    SetStartStopSync(bool),
    /// Set signed MIDI Clock latency compensation in milliseconds.
    SetOffsetMs(f32),
    /// Set Ableton Link quantum in beats.
    SetQuantum(f64),
    /// Quantize/re-align downbeat phase.
    ResyncDownbeat,
    /// Manually set Link tempo in BPM.
    SetLinkTempoManual(f64),
}

impl TryFrom<&EngineCommand> for RtCommand {
    type Error = ();

    fn try_from(cmd: &EngineCommand) -> Result<Self, Self::Error> {
        match *cmd {
            EngineCommand::SetDirection(dir) => Ok(Self::SetDirection(dir)),
            EngineCommand::SetStartStopSync(enabled) => Ok(Self::SetStartStopSync(enabled)),
            EngineCommand::SetOffsetMs(offset_ms) => Ok(Self::SetOffsetMs(offset_ms)),
            EngineCommand::SetQuantum(quantum) => Ok(Self::SetQuantum(quantum)),
            // Async-only command owning heap `String`s; never crosses into the RT ring buffer.
            EngineCommand::SelectMidiPorts { .. } => Err(()),
            EngineCommand::ResyncDownbeat => Ok(Self::ResyncDownbeat),
            EngineCommand::SetLinkTempoManual(bpm) => Ok(Self::SetLinkTempoManual(bpm)),
        }
    }
}
