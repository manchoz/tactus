//! Raw C99 FFI declarations for `extensions/abl_link/include/abl_link.h`.
//!
//! Link Audio (`abl_link_audio_*`) APIs are intentionally omitted.

use std::ffi::c_void;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct abl_link {
    pub impl_: *mut c_void,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct abl_link_session_state {
    pub impl_: *mut c_void,
}

#[allow(dead_code)]
extern "C" {
    pub fn abl_link_create(bpm: f64) -> abl_link;
    pub fn abl_link_destroy(link: abl_link);
    pub fn abl_link_is_enabled(link: abl_link) -> bool;
    pub fn abl_link_enable(link: abl_link, enable: bool);
    pub fn abl_link_is_start_stop_sync_enabled(link: abl_link) -> bool;
    pub fn abl_link_enable_start_stop_sync(link: abl_link, enabled: bool);
    pub fn abl_link_num_peers(link: abl_link) -> u64;
    pub fn abl_link_set_num_peers_callback(
        link: abl_link,
        callback: unsafe extern "C" fn(num_peers: u64, context: *mut c_void),
        context: *mut c_void,
    );
    pub fn abl_link_set_tempo_callback(
        link: abl_link,
        callback: unsafe extern "C" fn(tempo: f64, context: *mut c_void),
        context: *mut c_void,
    );
    pub fn abl_link_set_start_stop_callback(
        link: abl_link,
        callback: unsafe extern "C" fn(is_playing: bool, context: *mut c_void),
        context: *mut c_void,
    );
    pub fn abl_link_clock_micros(link: abl_link) -> i64;

    pub fn abl_link_create_session_state() -> abl_link_session_state;
    pub fn abl_link_destroy_session_state(session_state: abl_link_session_state);
    pub fn abl_link_capture_audio_session_state(
        link: abl_link,
        session_state: abl_link_session_state,
    );
    pub fn abl_link_commit_audio_session_state(
        link: abl_link,
        session_state: abl_link_session_state,
    );
    pub fn abl_link_capture_app_session_state(
        link: abl_link,
        session_state: abl_link_session_state,
    );
    pub fn abl_link_commit_app_session_state(link: abl_link, session_state: abl_link_session_state);

    pub fn abl_link_tempo(session_state: abl_link_session_state) -> f64;
    pub fn abl_link_set_tempo(session_state: abl_link_session_state, bpm: f64, at_time: i64);
    pub fn abl_link_beat_at_time(
        session_state: abl_link_session_state,
        time: i64,
        quantum: f64,
    ) -> f64;
    pub fn abl_link_phase_at_time(
        session_state: abl_link_session_state,
        time: i64,
        quantum: f64,
    ) -> f64;
    pub fn abl_link_time_at_beat(
        session_state: abl_link_session_state,
        beat: f64,
        quantum: f64,
    ) -> i64;
    pub fn abl_link_request_beat_at_time(
        session_state: abl_link_session_state,
        beat: f64,
        time: i64,
        quantum: f64,
    );
    pub fn abl_link_force_beat_at_time(
        session_state: abl_link_session_state,
        beat: f64,
        time: i64,
        quantum: f64,
    );
    pub fn abl_link_set_is_playing(
        session_state: abl_link_session_state,
        is_playing: bool,
        time: i64,
    );
    pub fn abl_link_is_playing(session_state: abl_link_session_state) -> bool;
    pub fn abl_link_time_for_is_playing(session_state: abl_link_session_state) -> i64;
    pub fn abl_link_request_beat_at_start_playing_time(
        session_state: abl_link_session_state,
        beat: f64,
        quantum: f64,
    );
    pub fn abl_link_set_is_playing_and_request_beat_at_time(
        session_state: abl_link_session_state,
        is_playing: bool,
        time: i64,
        beat: f64,
        quantum: f64,
    );
}
