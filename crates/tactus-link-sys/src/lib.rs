//! Minimal safe Rust wrapper around Ableton Link's C99 `abl_link` API.

pub mod ffi;

/// RAII handle to an Ableton Link session (`abl_link`).
///
/// Intentionally `Send` and `!Sync`: real-time audio-thread session-state methods require `&mut self`
/// so only the owning RT thread may capture/commit audio session state.
pub struct AblLink {
    raw: ffi::abl_link,
}

// SAFETY: `abl_link` methods are documented thread-safe for transfer across threads;
// single-threaded audio session-state access is statically enforced via `&mut self`.
unsafe impl Send for AblLink {}

impl AblLink {
    /// Construct a new Ableton Link instance initialized at `initial_bpm` (network disabled).
    pub fn new(initial_bpm: f64) -> Self {
        // SAFETY: `abl_link_create` allocates a new C++ `ableton::Link` instance and returns it
        // by value; we verify `impl_` is non-null before storing it in `Self`.
        let raw = unsafe { ffi::abl_link_create(initial_bpm) };
        assert!(!raw.impl_.is_null(), "abl_link_create returned null impl");
        Self { raw }
    }

    /// Return whether network discovery/sync is currently enabled.
    pub fn is_enabled(&self) -> bool {
        // SAFETY: `self.raw` is a valid, initialized `abl_link` handle for `&self`'s lifetime.
        unsafe { ffi::abl_link_is_enabled(self.raw) }
    }

    /// Enable or disable Link network communication.
    pub fn enable(&self, enabled: bool) {
        // SAFETY: `self.raw` is a valid `abl_link` handle; `abl_link_enable` is thread-safe.
        unsafe { ffi::abl_link_enable(self.raw, enabled) }
    }

    /// Return whether transport start/stop synchronization is enabled.
    pub fn is_start_stop_sync_enabled(&self) -> bool {
        // SAFETY: `self.raw` is a valid `abl_link` handle for `&self`'s lifetime.
        unsafe { ffi::abl_link_is_start_stop_sync_enabled(self.raw) }
    }

    /// Enable or disable transport start/stop synchronization.
    pub fn enable_start_stop_sync(&self, enabled: bool) {
        // SAFETY: `self.raw` is a valid `abl_link` handle; start/stop sync toggling is thread-safe.
        unsafe { ffi::abl_link_enable_start_stop_sync(self.raw, enabled) }
    }

    /// Return the number of currently connected peers in the Link session.
    pub fn num_peers(&self) -> u64 {
        // SAFETY: `self.raw` is a valid `abl_link` handle for `&self`'s lifetime.
        unsafe { ffi::abl_link_num_peers(self.raw) }
    }

    /// Read the current Link monotonic clock in microseconds (`abl_link_clock_micros`).
    pub fn clock_micros(&self) -> i64 {
        // SAFETY: `self.raw` is a valid `abl_link` handle for `&self`'s lifetime.
        unsafe { ffi::abl_link_clock_micros(self.raw) }
    }

    /// Capture the current session state from the real-time thread (realtime-safe, zero-alloc).
    pub fn capture_audio_session_state(&mut self, state: &mut SessionState) {
        // SAFETY: Both `self.raw` and `state.raw` are valid, uniquely borrowed (`&mut`) handles,
        // ensuring single-writer RT audio-thread access as required by `abl_link`.
        unsafe { ffi::abl_link_capture_audio_session_state(self.raw, state.raw) }
    }

    /// Commit modified session state from the real-time thread (realtime-safe, zero-alloc).
    pub fn commit_audio_session_state(&mut self, state: &SessionState) {
        // SAFETY: `self.raw` is uniquely borrowed (`&mut self`) and `state.raw` is a valid
        // initialized `abl_link_session_state` handle.
        unsafe { ffi::abl_link_commit_audio_session_state(self.raw, state.raw) }
    }

    /// Capture the current session state from a non-RT application thread.
    pub fn capture_app_session_state(&self, state: &mut SessionState) {
        // SAFETY: `self.raw` is a valid `abl_link` handle and `state.raw` is uniquely borrowed.
        unsafe { ffi::abl_link_capture_app_session_state(self.raw, state.raw) }
    }

    /// Commit modified session state from a non-RT application thread.
    pub fn commit_app_session_state(&self, state: &SessionState) {
        // SAFETY: Both `self.raw` and `state.raw` are valid, initialized handles.
        unsafe { ffi::abl_link_commit_app_session_state(self.raw, state.raw) }
    }
}

impl Drop for AblLink {
    fn drop(&mut self) {
        // SAFETY: `AblLink` owns `self.raw` uniquely and `drop` runs exactly once.
        unsafe { ffi::abl_link_destroy(self.raw) }
    }
}

/// Pre-allocated snapshot container for Ableton Link timeline & transport state.
///
/// Construct before entering the real-time loop so capture/commit perform zero heap allocations.
pub struct SessionState {
    raw: ffi::abl_link_session_state,
}

// SAFETY: `SessionState` wraps an owned `ableton::Link::SessionState` value with no shared aliasing.
unsafe impl Send for SessionState {}

impl SessionState {
    /// Allocate a new reusable [`SessionState`] handle outside the real-time loop.
    pub fn new() -> Self {
        // SAFETY: `abl_link_create_session_state` allocates a fresh session-state handle; we
        // verify `impl_` is non-null before storing it in `Self`.
        let raw = unsafe { ffi::abl_link_create_session_state() };
        assert!(
            !raw.impl_.is_null(),
            "abl_link_create_session_state returned null impl"
        );
        Self { raw }
    }

    /// Current session tempo in BPM.
    pub fn tempo(&self) -> f64 {
        // SAFETY: `self.raw` is a valid, non-null `abl_link_session_state` handle.
        unsafe { ffi::abl_link_tempo(self.raw) }
    }

    /// Set the session tempo to `bpm`, taking effect at `at_time_micros`.
    pub fn set_tempo(&mut self, bpm: f64, at_time_micros: i64) {
        // SAFETY: `self.raw` is a valid, uniquely borrowed `abl_link_session_state` handle.
        unsafe { ffi::abl_link_set_tempo(self.raw, bpm, at_time_micros) }
    }

    /// Map a Link clock timestamp (`time_micros`) to beats for `quantum`.
    pub fn beat_at_time(&self, time_micros: i64, quantum: f64) -> f64 {
        // SAFETY: `self.raw` is a valid, non-null `abl_link_session_state` handle.
        unsafe { ffi::abl_link_beat_at_time(self.raw, time_micros, quantum) }
    }

    /// Query the session phase in `[0, quantum)` at `time_micros`.
    pub fn phase_at_time(&self, time_micros: i64, quantum: f64) -> f64 {
        // SAFETY: `self.raw` is a valid, non-null `abl_link_session_state` handle.
        unsafe { ffi::abl_link_phase_at_time(self.raw, time_micros, quantum) }
    }

    /// Map a beat value to the Link clock timestamp (`micros`) for `quantum`.
    pub fn time_at_beat(&self, beat: f64, quantum: f64) -> i64 {
        // SAFETY: `self.raw` is a valid, non-null `abl_link_session_state` handle.
        unsafe { ffi::abl_link_time_at_beat(self.raw, beat, quantum) }
    }

    /// Request mapping `beat` to `time_micros` (quantizes cleanly when peers are present).
    pub fn request_beat_at_time(&mut self, beat: f64, time_micros: i64, quantum: f64) {
        // SAFETY: `self.raw` is a valid, uniquely borrowed `abl_link_session_state` handle.
        unsafe { ffi::abl_link_request_beat_at_time(self.raw, beat, time_micros, quantum) }
    }

    /// Unconditionally map `beat` to `time_micros` across the session.
    pub fn force_beat_at_time(&mut self, beat: f64, time_micros: i64, quantum: f64) {
        // SAFETY: `self.raw` is a valid, uniquely borrowed `abl_link_session_state` handle.
        unsafe { ffi::abl_link_force_beat_at_time(self.raw, beat, time_micros, quantum) }
    }

    /// Return whether transport is currently playing.
    pub fn is_playing(&self) -> bool {
        // SAFETY: `self.raw` is a valid, non-null `abl_link_session_state` handle.
        unsafe { ffi::abl_link_is_playing(self.raw) }
    }

    /// Set transport playing state, taking effect at `at_time_micros`.
    pub fn set_is_playing(&mut self, is_playing: bool, at_time_micros: i64) {
        // SAFETY: `self.raw` is a valid, uniquely borrowed `abl_link_session_state` handle.
        unsafe { ffi::abl_link_set_is_playing(self.raw, is_playing, at_time_micros) }
    }

    /// Return the Link clock timestamp (`micros`) when transport state last changed.
    pub fn time_for_is_playing(&self) -> i64 {
        // SAFETY: `self.raw` is a valid, non-null `abl_link_session_state` handle.
        unsafe { ffi::abl_link_time_for_is_playing(self.raw) }
    }

    /// Map `beat` to the start-playing time in the context of `quantum`.
    pub fn request_beat_at_start_playing_time(&mut self, beat: f64, quantum: f64) {
        // SAFETY: `self.raw` is a valid, uniquely borrowed `abl_link_session_state` handle.
        unsafe { ffi::abl_link_request_beat_at_start_playing_time(self.raw, beat, quantum) }
    }

    /// Atomically set transport playing state and request `beat` at `time_micros`.
    pub fn set_is_playing_and_request_beat_at_time(
        &mut self,
        is_playing: bool,
        time_micros: i64,
        beat: f64,
        quantum: f64,
    ) {
        // SAFETY: `self.raw` is a valid, uniquely borrowed `abl_link_session_state` handle.
        unsafe {
            ffi::abl_link_set_is_playing_and_request_beat_at_time(
                self.raw,
                is_playing,
                time_micros,
                beat,
                quantum,
            )
        }
    }
}

impl Default for SessionState {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for SessionState {
    fn drop(&mut self) {
        // SAFETY: `SessionState` owns `self.raw` uniquely and `drop` runs exactly once.
        unsafe { ffi::abl_link_destroy_session_state(self.raw) }
    }
}
