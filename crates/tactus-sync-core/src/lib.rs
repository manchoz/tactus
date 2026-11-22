//! Pure-Rust, OS-independent, zero-allocation sync engine core (`SPEC.md §2`).

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
