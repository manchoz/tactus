//! Pure-Rust, OS-independent sync engine core (M0 skeleton).

use tactus_protocol::{LockState, SyncConfig, TelemetrySnapshot};

/// Deterministic sync state machine (M0 placeholder; M1 implements OLS/step/phase estimators).
#[derive(Debug, Clone)]
pub struct SyncEngine {
    config: SyncConfig,
    snapshot: TelemetrySnapshot,
}

impl SyncEngine {
    /// Construct a new [`SyncEngine`] initialized from `config`.
    pub fn new(config: SyncConfig) -> Self {
        let snapshot = TelemetrySnapshot {
            direction: config.direction,
            lock_state: LockState::Unlocked,
            start_stop_sync: config.start_stop_sync,
            quantum: config.quantum,
            midi_offset_ms: config.midi_offset_ms,
            ..TelemetrySnapshot::default()
        };
        Self { config, snapshot }
    }

    /// Return the active [`SyncConfig`].
    pub fn config(&self) -> &SyncConfig {
        &self.config
    }

    /// Return a `Copy` telemetry snapshot of the current engine state.
    pub fn snapshot(&self) -> TelemetrySnapshot {
        self.snapshot
    }
}

#[cfg(test)]
mod tests {
    use tactus_protocol::Direction;

    use super::*;

    #[test]
    fn initializes_copy_snapshot_from_config() {
        let cfg = SyncConfig {
            direction: Direction::MidiToLink,
            midi_offset_ms: -1.5,
            ..SyncConfig::default()
        };

        let engine = SyncEngine::new(cfg);
        let snap = engine.snapshot();

        assert_eq!(snap.direction, Direction::MidiToLink);
        assert_eq!(snap.lock_state, LockState::Unlocked);
        assert_eq!(snap.midi_offset_ms, -1.5);
        assert_eq!(snap.quantum, 4.0);
    }
}
