use std::thread;
use std::time::Duration;

use tactus_link_sys::{AblLink, SessionState};

#[test]
fn link_session_reads_clock_beat_and_tempo_and_drops_cleanly() {
    let mut link = AblLink::new(120.0);
    let mut state = SessionState::new();

    assert!(!link.is_enabled());
    assert_eq!(link.num_peers(), 0);

    let t1 = link.clock_micros();
    thread::sleep(Duration::from_millis(2));
    let t2 = link.clock_micros();

    assert!(t1 > 0, "expected positive clock_micros, got {t1}");
    assert!(t2 >= t1, "expected monotonic clock_micros: {t2} < {t1}");

    link.capture_app_session_state(&mut state);
    assert!((state.tempo() - 120.0).abs() < 1e-6);

    link.capture_audio_session_state(&mut state);
    assert!((state.tempo() - 120.0).abs() < 1e-6);

    let quantum = 4.0;
    let beat = state.beat_at_time(t2, quantum);
    let phase = state.phase_at_time(t2, quantum);
    assert!(beat.is_finite());
    assert!((0.0..quantum).contains(&phase));

    let roundtrip_time = state.time_at_beat(beat, quantum);
    assert!(
        (roundtrip_time - t2).abs() <= 2,
        "expected time_at_beat({beat}) near {t2}, got {roundtrip_time}"
    );

    state.set_tempo(128.0, t2);
    link.commit_audio_session_state(&state);
    link.capture_audio_session_state(&mut state);
    assert!((state.tempo() - 128.0).abs() < 1e-6);

    link.enable_start_stop_sync(true);
    assert!(link.is_start_stop_sync_enabled());
    state.set_is_playing(true, t2);
    link.commit_audio_session_state(&state);
    link.capture_audio_session_state(&mut state);
    assert!(state.is_playing());
}
