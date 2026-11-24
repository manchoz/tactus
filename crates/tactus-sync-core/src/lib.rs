//! Pure-Rust, OS-independent, zero-allocation sync engine core (`SPEC.md §2`).
//!
//! Designed to compile and run deterministic simulation tests on any host without ALSA or
//! Ableton Link hardware attached (`LinkSession` abstracts the Link timeline):
//! - `engine`: unified [`SyncEngine`] state machine dispatching commands, MIDI input, and polls.
//! - `scheduler`: Mode A (`LinkToMidi`) 24 PPQN `TIMER_ABSTIME` clock and start/stop scheduler.
//! - `estimator` & `ring`: Mode B (`MidiToLink`) dual-window OLS tempo estimator and phase lock.
//! - `midi`: zero-allocation MIDI real-time event encoding and fixed-capacity [`MidiOutputBatch`].
//! - `link`: [`LinkSession`] trait and microsecond/nanosecond conversion helpers.

pub mod engine;
pub mod estimator;
pub mod link;
pub mod midi;
pub mod ring;
pub mod scheduler;

pub use engine::{PollResult, SyncEngine};
pub use estimator::{MidiToLinkEstimator, MIDI_TIMEOUT_NS};
pub use link::LinkSession;
pub use midi::{MidiEvent, MidiOutputBatch, ScheduledMidiEvent};
pub use ring::{
    ClockSample, OlsEstimate, SampleRing, FAST_RING_CAPACITY, LONG_RING_CAPACITY, MAX_VALID_BPM,
    MIN_VALID_BPM, TICKS_PER_BEAT,
};
pub use scheduler::LinkToMidiScheduler;
