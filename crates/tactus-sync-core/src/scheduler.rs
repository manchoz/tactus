//! Mode A (`LinkToMidi`): 24 PPQN `TIMER_ABSTIME` scheduler with quantized transport sync.

use tactus_protocol::SyncConfig;

use crate::link::{ns_to_us, us_to_ns, LinkSession};
use crate::midi::{MidiEvent, MidiOutputBatch, ScheduledMidiEvent};
use crate::ring::TICKS_PER_BEAT;

const MAX_CATCHUP_NS: i64 = 100_000_000;
const TICK_SNAP_EPS: f64 = 1e-6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MidiTransportState {
    Stopped,
    ArmedForStart { start_tick: i64 },
    Playing,
}

/// Scheduler generating drift-free 24 PPQN MIDI clock ticks on an Ableton Link timeline.
#[derive(Debug, Clone)]
pub struct LinkToMidiScheduler {
    next_tick_index: Option<i64>,
    transport: MidiTransportState,
    last_link_playing: bool,
}

impl LinkToMidiScheduler {
    /// Construct a fresh scheduler.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            next_tick_index: None,
            transport: MidiTransportState::Stopped,
            last_link_playing: false,
        }
    }

    /// Clear internal tick cursor and transport state.
    pub fn reset(&mut self) {
        self.next_tick_index = None;
        self.transport = MidiTransportState::Stopped;
        self.last_link_playing = false;
    }

    /// Emit `Stop (0xFC)` cleanly if currently playing with `start_stop_sync = true`, then reset.
    pub fn stop_if_playing(&mut self, now_ns: i64, start_stop_sync: bool) -> MidiOutputBatch {
        let mut batch = MidiOutputBatch::new();
        if start_stop_sync && self.transport == MidiTransportState::Playing {
            let _ = batch.push(ScheduledMidiEvent {
                event: MidiEvent::Stop,
                timestamp_ns: now_ns,
            });
        }
        self.reset();
        batch
    }

    /// Synchronize scheduler transport state when `start_stop_sync` is toggled dynamically.
    pub fn on_start_stop_sync_changed(&mut self, enabled: bool, link_is_playing: bool) {
        self.last_link_playing = link_is_playing;
        self.transport = if enabled && link_is_playing {
            MidiTransportState::Playing
        } else {
            MidiTransportState::Stopped
        };
    }

    /// Arm a quantized `Start (0xFA)` on the next Link quantum boundary (if playing & synced).
    pub fn arm_resync_downbeat(
        &mut self,
        now_ns: i64,
        config: &SyncConfig,
        link: &impl LinkSession,
    ) {
        if !config.start_stop_sync || !link.is_playing() {
            return;
        }
        let offset_ns = offset_ms_to_ns(config.midi_offset_ms);
        let min_tick = self.next_tick_index.unwrap_or_else(|| {
            let now_link_us = ns_to_us(now_ns.saturating_sub(offset_ns));
            tick_ceil_from_beat(link.beat_at_time(now_link_us, config.quantum))
        });
        let start_tick = next_quantum_tick(min_tick, config.quantum);
        self.transport = MidiTransportState::ArmedForStart { start_tick };
    }

    /// Advance the scheduler up to `now_ns`, returning due MIDI events and next absolute deadline.
    pub fn poll(
        &mut self,
        now_ns: i64,
        config: &SyncConfig,
        link: &impl LinkSession,
    ) -> (MidiOutputBatch, i64) {
        let mut batch = MidiOutputBatch::new();
        let offset_ns = offset_ms_to_ns(config.midi_offset_ms);
        let now_link_us = ns_to_us(now_ns.saturating_sub(offset_ns));
        let beat_now = link.beat_at_time(now_link_us, config.quantum);

        let mut next_tick = self
            .next_tick_index
            .unwrap_or_else(|| tick_ceil_from_beat(beat_now));

        let link_playing = link.is_playing();
        if config.start_stop_sync {
            if !self.last_link_playing && link_playing {
                let start_tick = next_quantum_tick(next_tick, config.quantum);
                self.transport = MidiTransportState::ArmedForStart { start_tick };
            } else if self.last_link_playing && !link_playing {
                if self.transport == MidiTransportState::Playing {
                    let _ = batch.push(ScheduledMidiEvent {
                        event: MidiEvent::Stop,
                        timestamp_ns: now_ns,
                    });
                }
                self.transport = MidiTransportState::Stopped;
            }
        } else {
            self.transport = MidiTransportState::Stopped;
        }
        self.last_link_playing = link_playing;

        let mut target_ns = tick_target_ns(next_tick, config.quantum, offset_ns, link);

        // Guard against large wake-up gaps or Link timeline jumps causing stale 0xF8 bursts.
        if now_ns.saturating_sub(target_ns) > MAX_CATCHUP_NS {
            next_tick = tick_ceil_from_beat(beat_now);
            if let MidiTransportState::ArmedForStart { start_tick } = self.transport {
                if start_tick < next_tick {
                    self.transport = MidiTransportState::ArmedForStart {
                        start_tick: next_quantum_tick(next_tick, config.quantum),
                    };
                }
            }
            target_ns = tick_target_ns(next_tick, config.quantum, offset_ns, link);
        }

        while now_ns >= target_ns && batch.remaining_capacity() >= 2 {
            if let MidiTransportState::ArmedForStart { start_tick } = self.transport {
                if next_tick >= start_tick {
                    let _ = batch.push(ScheduledMidiEvent {
                        event: MidiEvent::Start,
                        timestamp_ns: target_ns,
                    });
                    self.transport = MidiTransportState::Playing;
                }
            }

            let _ = batch.push(ScheduledMidiEvent {
                event: MidiEvent::Clock,
                timestamp_ns: target_ns,
            });

            next_tick = next_tick.saturating_add(1);
            target_ns = tick_target_ns(next_tick, config.quantum, offset_ns, link);
        }

        self.next_tick_index = Some(next_tick);
        (batch, target_ns)
    }
}

impl Default for LinkToMidiScheduler {
    fn default() -> Self {
        Self::new()
    }
}

#[inline]
fn offset_ms_to_ns(offset_ms: f32) -> i64 {
    (f64::from(offset_ms) * 1_000_000.0).round() as i64
}

#[inline]
fn tick_ceil_from_beat(beat: f64) -> i64 {
    let tick_f = beat * TICKS_PER_BEAT;
    let rounded = tick_f.round();
    if (tick_f - rounded).abs() < TICK_SNAP_EPS {
        rounded as i64
    } else {
        tick_f.ceil() as i64
    }
}

#[inline]
fn next_quantum_tick(min_tick: i64, quantum: f64) -> i64 {
    let min_beat = min_tick as f64 / TICKS_PER_BEAT;
    let quantum_index = ((min_beat - TICK_SNAP_EPS) / quantum).ceil();
    let start_beat = quantum_index * quantum;
    let mut start_tick = (start_beat * TICKS_PER_BEAT).round() as i64;
    if start_tick < min_tick {
        let quantum_ticks = ((quantum * TICKS_PER_BEAT).round() as i64).max(1);
        start_tick = start_tick.saturating_add(quantum_ticks);
    }
    start_tick
}

#[inline]
fn tick_target_ns(tick: i64, quantum: f64, offset_ns: i64, link: &impl LinkSession) -> i64 {
    let beat = tick as f64 / TICKS_PER_BEAT;
    let link_time_us = link.time_at_beat(beat, quantum);
    us_to_ns(link_time_us).saturating_add(offset_ns)
}
