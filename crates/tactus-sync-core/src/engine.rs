//! Unified pure-Rust synchronization engine state machine (`SyncEngine`).

use tactus_protocol::{Direction, LockState, RtCommand, SyncConfig, TelemetrySnapshot};

use crate::estimator::MidiToLinkEstimator;
use crate::link::{ns_to_us, LinkSession};
use crate::midi::{MidiEvent, MidiOutputBatch, ScheduledMidiEvent};
use crate::ring::{MAX_VALID_BPM, MIN_VALID_BPM};
use crate::scheduler::LinkToMidiScheduler;

/// Result of a periodic [`SyncEngine::poll`] step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PollResult {
    /// Zero or more scheduled MIDI events due up to `now_ns`.
    pub midi_out: MidiOutputBatch,
    /// Next monotonic deadline (`CLOCK_MONOTONIC` nanoseconds) for the RT sleep timer.
    pub next_deadline_ns: Option<i64>,
}

/// Pure-Rust, zero-allocation deterministic bidirectional sync engine (`SPEC.md §2`).
#[derive(Debug, Clone)]
pub struct SyncEngine {
    config: SyncConfig,
    snapshot: TelemetrySnapshot,
    scheduler: LinkToMidiScheduler,
    estimator: MidiToLinkEstimator,
    midi_thru_playing: bool,
}

impl SyncEngine {
    /// Construct a new [`SyncEngine`] initialized from `config`.
    #[must_use]
    pub fn new(config: SyncConfig) -> Self {
        let snapshot = TelemetrySnapshot {
            direction: config.direction,
            lock_state: LockState::Unlocked,
            start_stop_sync: config.start_stop_sync,
            clock_thru: config.clock_thru,
            bpm_link: config.initial_bpm,
            quantum: config.quantum,
            midi_offset_ms: config.midi_offset_ms,
            round_bpm: config.round_bpm,
            ..TelemetrySnapshot::default()
        };
        Self {
            config,
            snapshot,
            scheduler: LinkToMidiScheduler::new(),
            estimator: MidiToLinkEstimator::new(),
            midi_thru_playing: false,
        }
    }

    /// Return the active [`SyncConfig`].
    #[must_use]
    pub const fn config(&self) -> &SyncConfig {
        &self.config
    }

    /// Return the latest strictly `Copy` [`TelemetrySnapshot`].
    #[must_use]
    pub const fn snapshot(&self) -> TelemetrySnapshot {
        self.snapshot
    }

    /// Apply a real-time control command (`RtCommand`), returning any immediate MIDI output.
    pub fn apply_command(
        &mut self,
        cmd: RtCommand,
        now_ns: i64,
        link: &mut impl LinkSession,
    ) -> MidiOutputBatch {
        let mut midi_out = MidiOutputBatch::new();

        match cmd {
            RtCommand::SetDirection(dir) => {
                if self.config.direction != dir {
                    if self.config.direction == Direction::LinkToMidi {
                        midi_out = self
                            .scheduler
                            .stop_if_playing(now_ns, self.config.start_stop_sync);
                    } else {
                        if self.config.clock_thru
                            && self.midi_thru_playing
                            && self.config.start_stop_sync
                        {
                            let _ = midi_out.push(ScheduledMidiEvent {
                                event: MidiEvent::Stop,
                                timestamp_ns: now_ns,
                            });
                        }
                        self.scheduler.reset();
                    }
                    self.estimator.reset();
                    self.midi_thru_playing = false;
                    self.config.direction = dir;
                }
            }
            RtCommand::SetStartStopSync(enabled) => {
                if self.config.start_stop_sync != enabled {
                    self.config.start_stop_sync = enabled;
                    if !enabled {
                        self.midi_thru_playing = false;
                    }
                    if self.config.direction == Direction::LinkToMidi {
                        midi_out = self.scheduler.on_start_stop_sync_changed(
                            enabled,
                            now_ns,
                            &self.config,
                            link,
                        );
                    }
                }
            }
            RtCommand::SetClockThru(enabled) => {
                if self.config.clock_thru
                    && !enabled
                    && self.config.direction == Direction::MidiToLink
                    && self.midi_thru_playing
                    && self.config.start_stop_sync
                {
                    let _ = midi_out.push(ScheduledMidiEvent {
                        event: MidiEvent::Stop,
                        timestamp_ns: now_ns,
                    });
                }
                self.config.clock_thru = enabled;
                if !enabled {
                    self.midi_thru_playing = false;
                }
            }
            RtCommand::SetOffsetMs(offset_ms) => {
                if offset_ms.is_finite() {
                    self.config.midi_offset_ms = offset_ms.clamp(-50.0, 50.0);
                }
            }
            RtCommand::SetQuantum(quantum) => {
                if quantum.is_finite() && quantum > 0.0 {
                    self.config.quantum = quantum;
                }
            }
            RtCommand::ResyncDownbeat => match self.config.direction {
                Direction::LinkToMidi => {
                    self.scheduler
                        .arm_resync_downbeat(now_ns, &self.config, link);
                }
                Direction::MidiToLink => {
                    self.estimator.request_resync_downbeat();
                }
            },
            RtCommand::SetLinkTempoManual(bpm) => {
                if self.config.direction == Direction::LinkToMidi
                    && bpm.is_finite()
                    && (MIN_VALID_BPM..=MAX_VALID_BPM).contains(&bpm)
                {
                    link.set_tempo(bpm, ns_to_us(now_ns));
                }
            }
            RtCommand::SetLinkPlaying(playing) => {
                if self.config.direction == Direction::LinkToMidi {
                    link.set_is_playing(playing, ns_to_us(now_ns));
                }
            }
            RtCommand::SetRoundBpm(enabled) => {
                self.config.round_bpm = enabled;
                if enabled
                    && self.config.direction == Direction::MidiToLink
                    && self.estimator.lock_state() == LockState::Locked
                {
                    if let Some(raw_bpm) = self.estimator.bpm_midi_long() {
                        let rounded = raw_bpm.round().clamp(MIN_VALID_BPM, MAX_VALID_BPM);
                        if (rounded - link.tempo()).abs() > 1e-6 {
                            link.set_tempo(rounded, ns_to_us(now_ns));
                        }
                    }
                }
            }
        }

        self.refresh_snapshot(now_ns, link);
        midi_out
    }

    /// Process an incoming MIDI real-time event captured at `now_ns`.
    ///
    /// In `MidiToLink` mode with `clock_thru = true`, returns events to relay directly to other
    /// selected MIDI outputs.
    pub fn on_midi_event(
        &mut self,
        event: MidiEvent,
        now_ns: i64,
        link: &mut impl LinkSession,
    ) -> MidiOutputBatch {
        if self.config.direction != Direction::MidiToLink {
            return MidiOutputBatch::new();
        }

        match event {
            MidiEvent::Clock => self.estimator.on_clock(now_ns, &self.config, link),
            MidiEvent::Start => self.estimator.on_start(now_ns, &self.config, link),
            MidiEvent::Continue => self.estimator.on_continue(now_ns, &self.config, link),
            MidiEvent::Stop => self.estimator.on_stop(now_ns, &self.config, link),
        }

        let mut relay = MidiOutputBatch::new();
        if self.config.clock_thru {
            match event {
                MidiEvent::Clock => {
                    let _ = relay.push(ScheduledMidiEvent {
                        event: MidiEvent::Clock,
                        timestamp_ns: now_ns,
                    });
                }
                MidiEvent::Start | MidiEvent::Continue => {
                    if self.config.start_stop_sync {
                        let _ = relay.push(ScheduledMidiEvent {
                            event,
                            timestamp_ns: now_ns,
                        });
                        self.midi_thru_playing = true;
                    }
                }
                MidiEvent::Stop => {
                    if self.config.start_stop_sync {
                        let _ = relay.push(ScheduledMidiEvent {
                            event: MidiEvent::Stop,
                            timestamp_ns: now_ns,
                        });
                        self.midi_thru_playing = false;
                    }
                }
            }
        }

        self.refresh_snapshot(now_ns, link);
        relay
    }

    /// Advance scheduled MIDI clock output (`LinkToMidi`) or timeout watchdog (`MidiToLink`).
    pub fn poll(&mut self, now_ns: i64, link: &mut impl LinkSession) -> PollResult {
        let result = match self.config.direction {
            Direction::LinkToMidi => {
                let (midi_out, next_deadline_ns) = self.scheduler.poll(now_ns, &self.config, link);
                PollResult {
                    midi_out,
                    next_deadline_ns: Some(next_deadline_ns),
                }
            }
            Direction::MidiToLink => {
                if self.estimator.check_timeout(now_ns) {
                    self.midi_thru_playing = false;
                }
                PollResult {
                    midi_out: MidiOutputBatch::new(),
                    next_deadline_ns: self.estimator.next_timeout_deadline_ns(),
                }
            }
        };

        self.refresh_snapshot(now_ns, link);
        result
    }

    fn refresh_snapshot(&mut self, now_ns: i64, link: &impl LinkSession) {
        let now_us = ns_to_us(now_ns);
        self.snapshot = TelemetrySnapshot {
            direction: self.config.direction,
            lock_state: self.estimator.lock_state(),
            start_stop_sync: self.config.start_stop_sync,
            clock_thru: self.config.clock_thru,
            bpm_link: link.tempo(),
            bpm_midi_long: self.estimator.bpm_midi_long(),
            bpm_midi_fast: self.estimator.bpm_midi_fast(),
            phase_error_ms: self.estimator.phase_error_ms(),
            beat_phase: link.phase_at_time(now_us, self.config.quantum),
            quantum: self.config.quantum,
            link_peers: link.num_peers(),
            is_playing: link.is_playing(),
            midi_offset_ms: self.config.midi_offset_ms,
            round_bpm: self.config.round_bpm,
        };
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
