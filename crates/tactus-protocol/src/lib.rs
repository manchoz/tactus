//! Shared command, RT-safe telemetry, async application state, and configuration types.

pub mod command;
pub mod config;
pub mod state;
pub mod telemetry;

pub use command::{Direction, EngineCommand, RtCommand};
pub use config::{
    normalize_output_matches, ConfigError, McuBridgeConfig, MidiConfig, SyncConfig, TactusConfig,
    WebConfig, MAX_MIDI_OUTPUTS,
};
pub use state::{AppState, MidiPortInfo};
pub use telemetry::{LockState, TelemetrySnapshot};

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_copy<T: Copy>() {}

    #[test]
    fn rt_types_are_copy() {
        assert_copy::<TelemetrySnapshot>();
        assert_copy::<RtCommand>();
        assert_copy::<Direction>();
        assert_copy::<LockState>();
    }

    #[test]
    fn engine_command_to_rt_command_filters_async_only_variants() {
        let rt = RtCommand::try_from(&EngineCommand::SetDirection(Direction::MidiToLink));
        assert_eq!(rt, Ok(RtCommand::SetDirection(Direction::MidiToLink)));

        let rt = RtCommand::try_from(&EngineCommand::SetStartStopSync(false));
        assert_eq!(rt, Ok(RtCommand::SetStartStopSync(false)));

        let rt = RtCommand::try_from(&EngineCommand::SetClockThru(false));
        assert_eq!(rt, Ok(RtCommand::SetClockThru(false)));

        let rt = RtCommand::try_from(&EngineCommand::SetOffsetMs(-2.5));
        assert_eq!(rt, Ok(RtCommand::SetOffsetMs(-2.5)));

        let rt = RtCommand::try_from(&EngineCommand::SetQuantum(4.0));
        assert_eq!(rt, Ok(RtCommand::SetQuantum(4.0)));

        let rt = RtCommand::try_from(&EngineCommand::ResyncDownbeat);
        assert_eq!(rt, Ok(RtCommand::ResyncDownbeat));

        let rt = RtCommand::try_from(&EngineCommand::SetLinkTempoManual(124.0));
        assert_eq!(rt, Ok(RtCommand::SetLinkTempoManual(124.0)));

        let non_rt = RtCommand::try_from(&EngineCommand::SelectMidiPorts {
            input: "WIDI Uhost".to_string(),
            outputs: vec!["WIDI Uhost".to_string(), "OP-XY".to_string()],
        });
        assert_eq!(non_rt, Err(()));
    }

    #[test]
    fn spec_toml_parses_to_defaults_and_validates() {
        let spec_toml = r#"
[sync]
direction = "link_to_midi"
start_stop_sync = true
clock_thru = true
quantum = 4.0
midi_offset_ms = 0.0
ols_window_secs = 10.0
fast_window_secs = 1.5
bpm_deadband = 0.05
step_change_bpm = 1.5
step_change_beats = 3
phase_deadband_ms = 4.0
max_phase_nudge_ms = 1.0

[midi]
input_port_match = "WIDI"
output_port_matches = ["WIDI"]

[web]
listen_addr = "0.0.0.0:80"

[mcu_bridge]
enabled = false
socket_path = "/var/run/arduino-router.sock"
"#;

        let parsed: TactusConfig = toml::from_str(spec_toml).expect("SPEC.md TOML should parse");
        assert_eq!(parsed, TactusConfig::default());
        assert_eq!(parsed.validate(), Ok(()));
    }

    #[test]
    fn legacy_output_port_match_and_multi_outputs_migrate_cleanly() {
        let legacy_widi: TactusConfig = toml::from_str(
            r#"
[midi]
input_port_match = "WIDI"
output_port_match = "WIDI"
"#,
        )
        .expect("legacy WIDI TOML should parse");
        assert_eq!(legacy_widi, TactusConfig::default());

        let legacy_empty: TactusConfig = toml::from_str(
            r#"
[midi]
input_port_match = "OP-XY"
output_port_match = ""
"#,
        )
        .expect("legacy empty output TOML should parse");
        assert_eq!(legacy_empty.midi.input_port_match, "OP-XY");
        assert!(legacy_empty.midi.output_port_matches.is_empty());

        let normalized: TactusConfig = toml::from_str(
            r#"
[sync]
clock_thru = false

[midi]
input_port_match = "OP-XY"
output_port_matches = ["WIDI", " OP-XY ", "widi", "   "]
"#,
        )
        .expect("multi-output TOML should parse");
        assert!(!normalized.sync.clock_thru);
        assert_eq!(
            normalized.midi.output_port_matches,
            vec!["WIDI".to_string(), "OP-XY".to_string()]
        );
    }

    #[test]
    fn empty_and_partial_toml_apply_defaults() {
        let empty: TactusConfig = toml::from_str("").expect("empty TOML should parse");
        assert_eq!(empty, TactusConfig::default());

        let partial: TactusConfig = toml::from_str(
            r#"
[sync]
direction = "midi_to_link"
midi_offset_ms = -3.5
"#,
        )
        .expect("partial TOML should parse");
        assert_eq!(partial.sync.direction, Direction::MidiToLink);
        assert_eq!(partial.sync.midi_offset_ms, -3.5);
        assert!(partial.sync.clock_thru);
        assert_eq!(partial.sync.quantum, 4.0);
        assert_eq!(partial.midi, MidiConfig::default());
        assert_eq!(partial.web, WebConfig::default());
        assert_eq!(partial.mcu_bridge, McuBridgeConfig::default());
        assert_eq!(partial.validate(), Ok(()));
    }

    #[test]
    fn validation_rejects_out_of_bounds_values() {
        let mut cfg = TactusConfig::default();
        cfg.sync.midi_offset_ms = 50.5;
        assert!(cfg.validate().is_err());

        cfg.sync.midi_offset_ms = -50.5;
        assert!(cfg.validate().is_err());

        cfg.sync.midi_offset_ms = f32::NAN;
        assert!(cfg.validate().is_err());

        let cfg = TactusConfig {
            sync: SyncConfig {
                quantum: 0.0,
                ..SyncConfig::default()
            },
            ..TactusConfig::default()
        };
        assert!(cfg.validate().is_err());

        let cfg = TactusConfig {
            sync: SyncConfig {
                ols_window_secs: 1.0,
                fast_window_secs: 1.5,
                ..SyncConfig::default()
            },
            ..TactusConfig::default()
        };
        assert!(cfg.validate().is_err());

        let cfg = TactusConfig {
            sync: SyncConfig {
                step_change_beats: 0,
                ..SyncConfig::default()
            },
            ..TactusConfig::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn json_round_trip_for_commands_and_app_state() {
        let commands = [
            (
                EngineCommand::SetDirection(Direction::MidiToLink),
                r#"{"set_direction":"midi_to_link"}"#,
            ),
            (
                EngineCommand::SetStartStopSync(false),
                r#"{"set_start_stop_sync":false}"#,
            ),
            (
                EngineCommand::SetClockThru(false),
                r#"{"set_clock_thru":false}"#,
            ),
            (
                EngineCommand::SetOffsetMs(12.5),
                r#"{"set_offset_ms":12.5}"#,
            ),
            (EngineCommand::SetQuantum(3.0), r#"{"set_quantum":3.0}"#),
            (
                EngineCommand::SelectMidiPorts {
                    input: "OP-XY".to_string(),
                    outputs: vec!["WIDI".to_string(), "OP-XY".to_string()],
                },
                r#"{"select_midi_ports":{"input":"OP-XY","outputs":["WIDI","OP-XY"]}}"#,
            ),
            (EngineCommand::ResyncDownbeat, r#""resync_downbeat""#),
            (
                EngineCommand::SetLinkTempoManual(128.0),
                r#"{"set_link_tempo_manual":128.0}"#,
            ),
        ];

        for (cmd, expected_wire) in commands {
            let json = serde_json::to_string(&cmd).expect("command serialize");
            assert_eq!(json, expected_wire);
            let back: EngineCommand = serde_json::from_str(&json).expect("command deserialize");
            assert_eq!(back, cmd);
        }

        let state = AppState {
            telemetry: TelemetrySnapshot {
                direction: Direction::MidiToLink,
                lock_state: LockState::Locked,
                start_stop_sync: true,
                clock_thru: true,
                bpm_link: 126.0,
                bpm_midi_long: Some(126.02),
                bpm_midi_fast: Some(126.01),
                phase_error_ms: -0.4,
                beat_phase: 1.25,
                quantum: 4.0,
                link_peers: 2,
                is_playing: true,
                midi_offset_ms: 1.5,
            },
            available_midi_ports: vec![MidiPortInfo {
                id: "hw:1,0,0".to_string(),
                card_name: "WIDI Uhost".to_string(),
                port_name: "WIDI Uhost MIDI 1".to_string(),
                can_read: true,
                can_write: true,
            }],
            selected_midi_input: Some("hw:2,0,0".to_string()),
            selected_midi_outputs: vec!["hw:1,0,0".to_string(), "hw:2,0,0".to_string()],
            midi_input_match: "OP-XY".to_string(),
            midi_output_matches: vec!["WIDI".to_string(), "OP-XY".to_string()],
        };

        let json = serde_json::to_string(&state).expect("state serialize");
        let back: AppState = serde_json::from_str(&json).expect("state deserialize");
        assert_eq!(back, state);

        // Verify backward compatibility when new fields are omitted.
        let legacy_json = json
            .replace(r#","midi_input_match":"OP-XY""#, "")
            .replace(r#","midi_output_matches":["WIDI","OP-XY"]"#, "")
            .replace(r#","selected_midi_outputs":["hw:1,0,0","hw:2,0,0"]"#, "");
        let legacy_back: AppState =
            serde_json::from_str(&legacy_json).expect("legacy AppState deserialize");
        assert_eq!(legacy_back.midi_input_match, "");
        assert!(legacy_back.midi_output_matches.is_empty());
        assert!(legacy_back.selected_midi_outputs.is_empty());
        assert_eq!(legacy_back.selected_midi_input, state.selected_midi_input);
    }
}
