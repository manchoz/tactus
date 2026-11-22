//! Real-time MIDI Clock and Transport messages and zero-allocation output batches.

/// Real-time MIDI system message handled by the sync engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MidiEvent {
    /// MIDI Timing Clock (`0xF8`, 24 PPQN).
    Clock,
    /// MIDI Start (`0xFA`).
    Start,
    /// MIDI Continue (`0xFB`).
    Continue,
    /// MIDI Stop (`0xFC`).
    Stop,
}

impl MidiEvent {
    /// Convert to the raw MIDI System Real-Time status byte.
    #[must_use]
    pub const fn to_byte(self) -> u8 {
        match self {
            Self::Clock => 0xF8,
            Self::Start => 0xFA,
            Self::Continue => 0xFB,
            Self::Stop => 0xFC,
        }
    }

    /// Parse a raw MIDI System Real-Time status byte into a [`MidiEvent`].
    #[must_use]
    pub const fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            0xF8 => Some(Self::Clock),
            0xFA => Some(Self::Start),
            0xFB => Some(Self::Continue),
            0xFC => Some(Self::Stop),
            _ => None,
        }
    }
}

/// Outgoing MIDI message paired with its scheduled `CLOCK_MONOTONIC` timestamp (nanoseconds).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScheduledMidiEvent {
    /// Real-time MIDI event to emit.
    pub event: MidiEvent,
    /// Target monotonic timestamp in nanoseconds (`TIMER_ABSTIME`).
    pub timestamp_ns: i64,
}

const MAX_BATCH_LEN: usize = 8;

/// Stack-allocated, `Copy` batch of scheduled MIDI events returned from RT engine ticks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MidiOutputBatch {
    events: [ScheduledMidiEvent; MAX_BATCH_LEN],
    len: u8,
}

impl MidiOutputBatch {
    /// Create an empty [`MidiOutputBatch`].
    #[must_use]
    pub const fn new() -> Self {
        let placeholder = ScheduledMidiEvent {
            event: MidiEvent::Clock,
            timestamp_ns: 0,
        };
        Self {
            events: [placeholder; MAX_BATCH_LEN],
            len: 0,
        }
    }

    /// Append a [`ScheduledMidiEvent`], returning `false` if the batch is full.
    pub fn push(&mut self, event: ScheduledMidiEvent) -> bool {
        let idx = self.len as usize;
        if idx >= MAX_BATCH_LEN {
            return false;
        }
        self.events[idx] = event;
        self.len += 1;
        true
    }

    /// View the populated events as a slice.
    #[must_use]
    pub fn as_slice(&self) -> &[ScheduledMidiEvent] {
        &self.events[..self.len as usize]
    }

    /// Number of events in the batch.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len as usize
    }

    /// Returns `true` when the batch contains no events.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Remaining event slots available in this batch.
    #[must_use]
    pub const fn remaining_capacity(&self) -> usize {
        MAX_BATCH_LEN - (self.len as usize)
    }
}

impl Default for MidiOutputBatch {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn midi_event_byte_roundtrip() {
        for (byte, ev) in [
            (0xF8, MidiEvent::Clock),
            (0xFA, MidiEvent::Start),
            (0xFB, MidiEvent::Continue),
            (0xFC, MidiEvent::Stop),
        ] {
            assert_eq!(MidiEvent::from_byte(byte), Some(ev));
            assert_eq!(ev.to_byte(), byte);
        }
        assert_eq!(MidiEvent::from_byte(0xFE), None);
    }

    #[test]
    fn midi_output_batch_respects_fixed_capacity() {
        let mut batch = MidiOutputBatch::new();
        assert!(batch.is_empty());
        assert_eq!(batch.remaining_capacity(), 8);

        for i in 0..8 {
            assert!(batch.push(ScheduledMidiEvent {
                event: MidiEvent::Clock,
                timestamp_ns: i,
            }));
        }
        assert_eq!(batch.len(), 8);
        assert_eq!(batch.remaining_capacity(), 0);
        assert!(!batch.push(ScheduledMidiEvent {
            event: MidiEvent::Stop,
            timestamp_ns: 99,
        }));
        assert_eq!(batch.as_slice().len(), 8);
    }
}
