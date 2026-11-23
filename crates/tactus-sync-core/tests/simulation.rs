//! Deterministic host simulation tests for `tactus-sync-core` (`MILESTONES.md M1`).

use tactus_protocol::{Direction, LockState, RtCommand, SyncConfig};
use tactus_sync_core::{LinkSession, MidiEvent, ScheduledMidiEvent, SyncEngine};

struct MockLinkSession {
    tempo: f64,
    beat_origin_us: i64,
    beat_at_origin: f64,
    playing: bool,
    peers: usize,
    tempo_commits: Vec<(i64, f64)>,
    phase_nudges_us: Vec<(i64, i64)>,
    request_beats: Vec<(f64, i64)>,
    forced_beats: Vec<(f64, i64)>,
    play_changes: Vec<(bool, i64)>,
}

impl MockLinkSession {
    fn new(initial_bpm: f64) -> Self {
        Self {
            tempo: initial_bpm,
            beat_origin_us: 0,
            beat_at_origin: 0.0,
            playing: false,
            peers: 0,
            tempo_commits: Vec::new(),
            phase_nudges_us: Vec::new(),
            request_beats: Vec::new(),
            forced_beats: Vec::new(),
            play_changes: Vec::new(),
        }
    }
}

impl LinkSession for MockLinkSession {
    fn num_peers(&self) -> usize {
        self.peers
    }

    fn tempo(&self) -> f64 {
        self.tempo
    }

    fn beat_at_time(&self, time_us: i64, _quantum: f64) -> f64 {
        let dt_us = (time_us - self.beat_origin_us) as f64;
        self.beat_at_origin + dt_us * (self.tempo / 60_000_000.0)
    }

    fn phase_at_time(&self, time_us: i64, quantum: f64) -> f64 {
        self.beat_at_time(time_us, quantum).rem_euclid(quantum)
    }

    fn time_at_beat(&self, beat: f64, _quantum: f64) -> i64 {
        let dt_us = (beat - self.beat_at_origin) * (60_000_000.0 / self.tempo);
        self.beat_origin_us + dt_us.round() as i64
    }

    fn is_playing(&self) -> bool {
        self.playing
    }

    fn set_tempo(&mut self, bpm: f64, at_time_us: i64) {
        let beat_now = self.beat_at_time(at_time_us, 4.0);
        self.beat_at_origin = beat_now;
        self.beat_origin_us = at_time_us;
        self.tempo = bpm;
        self.tempo_commits.push((at_time_us, bpm));
    }

    fn request_beat_at_time(&mut self, beat: f64, time_us: i64, quantum: f64) {
        self.request_beats.push((beat, time_us));
        if self.peers == 0 {
            self.beat_at_origin = beat;
            self.beat_origin_us = time_us;
        } else {
            let current_phase = self.phase_at_time(time_us, quantum);
            let target_phase = beat.rem_euclid(quantum);
            let delta_phase = (target_phase - current_phase).rem_euclid(quantum);
            let delta_us = (delta_phase * (60_000_000.0 / self.tempo)).round() as i64;
            self.beat_at_origin = beat;
            self.beat_origin_us = time_us + delta_us;
        }
    }

    fn force_beat_at_time(&mut self, beat: f64, time_us: i64, quantum: f64) {
        let prev_time = self.time_at_beat(beat, quantum);
        self.forced_beats.push((beat, time_us));
        self.phase_nudges_us.push((time_us, time_us - prev_time));
        self.beat_at_origin = beat;
        self.beat_origin_us = time_us;
    }

    fn set_is_playing(&mut self, is_playing: bool, at_time_us: i64) {
        self.playing = is_playing;
        self.play_changes.push((is_playing, at_time_us));
    }

    fn set_is_playing_and_request_beat_at_time(
        &mut self,
        is_playing: bool,
        time_us: i64,
        beat: f64,
        quantum: f64,
    ) {
        self.set_is_playing(is_playing, time_us);
        self.request_beat_at_time(beat, time_us, quantum);
    }
}

/// Deterministic `SplitMix64` PRNG (zero external crates).
struct DeterministicRng {
    state: u64,
}

impl DeterministicRng {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform integer in `[-max_abs, +max_abs]`.
    fn signed_range(&mut self, max_abs: i64) -> i64 {
        let span = (2 * max_abs + 1) as u64;
        (self.next_u64() % span) as i64 - max_abs
    }

    /// Uniform integer in `[0, max_inclusive]`.
    fn non_negative(&mut self, max_inclusive: i64) -> i64 {
        (self.next_u64() % (max_inclusive as u64 + 1)) as i64
    }
}

fn midi_to_link_config() -> SyncConfig {
    SyncConfig {
        direction: Direction::MidiToLink,
        ..SyncConfig::default()
    }
}

#[test]
fn midi_to_link_ble_jitter_5ms_stability() {
    let bpm = 120.0;
    let tick_ns = 60_000_000_000.0 / (24.0 * bpm);

    // Part A: tempo acquisition from mismatched initial Link BPM (100 -> 120) under ±5 ms jitter.
    {
        let mut engine = SyncEngine::new(midi_to_link_config());
        let mut link = MockLinkSession::new(100.0);
        let mut rng = DeterministicRng::new(0x5EED_B1E5);

        engine.on_midi_event(MidiEvent::Start, 0, &mut link);
        let mut reached_locked = false;

        for i in 0..(20 * 48) {
            let ideal_ns = ((i as f64) * tick_ns).round() as i64;
            let jitter_ns = if i == 0 {
                0
            } else {
                rng.signed_range(5_000_000)
            };
            let ts_ns = (ideal_ns + jitter_ns).max(0);
            engine.on_midi_event(MidiEvent::Clock, ts_ns, &mut link);

            let snap = engine.snapshot();
            if i == 0 {
                assert_eq!(snap.lock_state, LockState::Acquiring);
                assert!(link.tempo_commits.is_empty());
            }
            if snap.lock_state == LockState::Locked {
                reached_locked = true;
                assert!(
                    (snap.bpm_link - bpm).abs() < 0.25,
                    "tick {i}: Link BPM drifted to {}",
                    snap.bpm_link
                );
            }
        }
        assert!(reached_locked, "should reach Locked state");
        assert!(
            (engine.snapshot().bpm_link - bpm).abs() <= 0.05,
            "expected Link BPM within 120.0 ± 0.05, got {}",
            engine.snapshot().bpm_link
        );
    }

    // Part B: 60 s steady-state BLE ±5 ms jitter from phase-aligned lock (zero spurious nudges).
    {
        let mut engine = SyncEngine::new(midi_to_link_config());
        let mut link = MockLinkSession::new(bpm);
        let mut rng = DeterministicRng::new(0x5EED_CAFE);
        let total_ticks = 60 * 48;

        engine.on_midi_event(MidiEvent::Start, 0, &mut link);
        let mut locked_commit_count = None;

        for i in 0..total_ticks {
            let ideal_ns = ((i as f64) * tick_ns).round() as i64;
            let jitter_ns = if i == 0 {
                0
            } else {
                rng.signed_range(5_000_000)
            };
            let ts_ns = (ideal_ns + jitter_ns).max(0);
            engine.on_midi_event(MidiEvent::Clock, ts_ns, &mut link);

            let snap = engine.snapshot();
            if snap.lock_state == LockState::Locked && locked_commit_count.is_none() {
                locked_commit_count = Some(link.tempo_commits.len());
            }
            if snap.lock_state == LockState::Locked {
                assert!((snap.bpm_link - bpm).abs() < 0.15);
            }
        }

        let final_snap = engine.snapshot();
        assert_eq!(final_snap.lock_state, LockState::Locked);
        assert!(
            (final_snap.bpm_link - bpm).abs() <= 0.05,
            "expected Link BPM within 120.0 ± 0.05, got {}",
            final_snap.bpm_link
        );
        assert!(
            link.tempo_commits.len() <= locked_commit_count.unwrap() + 4,
            "unexpected tempo commits under steady ±5ms BLE jitter: {:?}",
            link.tempo_commits
        );
        // With a ±0.05 BPM tempo deadband, any sub-deadband frequency offset from the 1.5 s
        // initial lock causes slow unidirectional phase drift (~5–25 ms/min), absorbed smoothly
        // without beat-by-beat jitter thrashing or ± sign oscillation.
        assert!(
            link.phase_nudges_us.len() <= 25,
            "excessive phase nudges over 120 beats under ±5ms BLE jitter: {:?}",
            link.phase_nudges_us
        );
        let has_pos = link.phase_nudges_us.iter().any(|&(_, n)| n > 0);
        let has_neg = link.phase_nudges_us.iter().any(|&(_, n)| n < 0);
        assert!(
            !(has_pos && has_neg),
            "oscillating ± phase nudges detected under OLS-denoised BLE jitter: {:?}",
            link.phase_nudges_us
        );
        assert!(final_snap.phase_error_ms.abs() <= 5.0);
    }
}

#[test]
fn midi_to_link_quartz_drift_40ppm_over_12_minutes() {
    let mut engine = SyncEngine::new(midi_to_link_config());
    let mut link = MockLinkSession::new(120.0);

    // Lock cleanly on 120.0 BPM for 2 s, then apply 40 ppm quartz clock drift for 12 minutes.
    let nominal_bpm = 120.0;
    let nominal_tick_ns = 60_000_000_000.0 / (24.0 * nominal_bpm);
    let drifted_bpm = nominal_bpm * (1.0 + 40e-6);
    let drifted_tick_ns = 60_000_000_000.0 / (24.0 * drifted_bpm);

    engine.on_midi_event(MidiEvent::Start, 0, &mut link);

    let warmup_ticks = 2 * 48;
    let mut base_ns = 0_i64;
    for i in 0..warmup_ticks {
        let ts_ns = ((i as f64) * nominal_tick_ns).round() as i64;
        engine.on_midi_event(MidiEvent::Clock, ts_ns, &mut link);
        base_ns = ts_ns;
    }
    assert_eq!(engine.snapshot().lock_state, LockState::Locked);

    let drift_ticks = 720 * 48; // 12 minutes at 120 BPM.
    let mut max_abs_phase_err_ms = 0.0_f32;
    for j in 1..=drift_ticks {
        let ts_ns = base_ns + ((j as f64) * drifted_tick_ns).round() as i64;
        engine.on_midi_event(MidiEvent::Clock, ts_ns, &mut link);

        let snap = engine.snapshot();
        if snap.lock_state == LockState::Locked {
            max_abs_phase_err_ms = max_abs_phase_err_ms.max(snap.phase_error_ms.abs());
        }
    }

    assert_eq!(engine.snapshot().lock_state, LockState::Locked);
    assert!(
        !link.phase_nudges_us.is_empty(),
        "expected phase-lock nudges over 12 min of 40 ppm drift (28.8 ms total drift)"
    );
    for &(_, nudge_us) in &link.phase_nudges_us {
        assert!(
            nudge_us.abs() <= 1_000,
            "phase nudge exceeded 1.0 ms bound: {nudge_us} us"
        );
    }
    assert!(
        max_abs_phase_err_ms <= 5.0,
        "phase error exceeded 5.0 ms bound during 12-min drift run: {max_abs_phase_err_ms} ms"
    );
    // 40 ppm of 120 BPM is 0.0048 BPM (< 0.05 deadband), so Link tempo commits stay clean.
    assert_eq!(
        link.tempo_commits.len(),
        1,
        "unexpected tempo chatter for 40 ppm drift: {:?}",
        link.tempo_commits
    );
}

#[test]
fn midi_to_link_step_change_110_to_128_and_back() {
    let mut engine = SyncEngine::new(midi_to_link_config());
    let mut link = MockLinkSession::new(110.0);
    let mut rng = DeterministicRng::new(0x110_128);

    engine.on_midi_event(MidiEvent::Start, 0, &mut link);

    let mut now_ns: i64 = 0;
    let segments: [(f64, f64); 3] = [(110.0, 15.0), (128.0, 15.0), (110.0, 15.0)];

    for (seg_idx, (bpm, secs)) in segments.iter().enumerate() {
        let tick_ns = 60_000_000_000.0 / (24.0 * bpm);
        let ticks = ((*secs * bpm / 60.0) * 24.0).round() as i64;
        let seg_start_ns = now_ns;

        for t in 0..ticks {
            let ideal_ts = seg_start_ns + ((t as f64) * tick_ns).round() as i64;
            let jitter = if seg_idx == 0 && t == 0 {
                0
            } else {
                rng.signed_range(1_500_000)
            };
            let ts = (ideal_ts + jitter).max(now_ns + 1);
            now_ns = ts;
            engine.on_midi_event(MidiEvent::Clock, ts, &mut link);

            let snap = engine.snapshot();
            assert!(
                (108.0..=130.0).contains(&snap.bpm_link),
                "wild overshoot detected at seg {seg_idx} tick {t}: bpm_link={}",
                snap.bpm_link
            );
        }

        let end_snap = engine.snapshot();
        assert_eq!(end_snap.lock_state, LockState::Locked);
        assert!(
            (end_snap.bpm_link - bpm).abs() <= 0.05,
            "segment {seg_idx}: expected Link BPM near {bpm}, got {}",
            end_snap.bpm_link
        );

        // Advance cleanly to next segment boundary.
        now_ns = seg_start_ns + ((ticks as f64) * tick_ns).round() as i64;
    }
}

#[test]
fn midi_to_link_clock_dropout_and_recovery() {
    let mut engine = SyncEngine::new(midi_to_link_config());
    let mut link = MockLinkSession::new(120.0);

    let tick_120_ns = 60_000_000_000.0 / (24.0 * 120.0);
    let mut now_ns: i64 = 0;

    // Acquire and lock at 120 BPM over 3 seconds.
    for i in 0..144 {
        now_ns = ((i as f64) * tick_120_ns).round() as i64;
        engine.on_midi_event(MidiEvent::Clock, now_ns, &mut link);
    }
    assert_eq!(engine.snapshot().lock_state, LockState::Locked);
    assert!((engine.snapshot().bpm_link - 120.0).abs() < 0.05);

    let commits_before_dropout = link.tempo_commits.len();

    // Poll at 450 ms silence: must still be Locked.
    let res_before = engine.poll(now_ns + 450_000_000, &mut link);
    assert_eq!(engine.snapshot().lock_state, LockState::Locked);
    assert_eq!(res_before.next_deadline_ns, Some(now_ns + 500_000_000));

    // Poll at 501 ms silence: must transition to Unlocked and clear estimator buffers.
    let res_timeout = engine.poll(now_ns + 501_000_000, &mut link);
    let snap = engine.snapshot();
    assert_eq!(snap.lock_state, LockState::Unlocked);
    assert_eq!(snap.bpm_midi_long, None);
    assert_eq!(snap.bpm_midi_fast, None);
    assert_eq!(snap.phase_error_ms, 0.0);
    assert_eq!(res_timeout.next_deadline_ns, None);
    assert_eq!(link.tempo_commits.len(), commits_before_dropout);

    // Resume at 132 BPM after 2 seconds of silence.
    let resume_start_ns = now_ns + 2_000_000_000;
    let tick_132_ns = 60_000_000_000.0 / (24.0 * 132.0);
    for i in 0..160 {
        let ts = resume_start_ns + ((i as f64) * tick_132_ns).round() as i64;
        engine.on_midi_event(MidiEvent::Clock, ts, &mut link);
        if i == 0 {
            assert_eq!(engine.snapshot().lock_state, LockState::Acquiring);
        }
    }
    let recovered = engine.snapshot();
    assert_eq!(recovered.lock_state, LockState::Locked);
    assert!(
        (recovered.bpm_link - 132.0).abs() <= 0.05,
        "expected recovered BPM near 132.0, got {}",
        recovered.bpm_link
    );
}

#[test]
fn direction_switch_while_playing_emits_stop_and_resets_state() {
    let mut engine = SyncEngine::new(SyncConfig::default());
    let mut link = MockLinkSession::new(120.0);
    link.set_is_playing(true, 0);

    // First poll in LinkToMidi at t=0 starts transport at downbeat 0.
    let poll0 = engine.poll(0, &mut link);
    assert_eq!(
        poll0.midi_out.as_slice(),
        &[
            ScheduledMidiEvent {
                event: MidiEvent::Start,
                timestamp_ns: 0,
            },
            ScheduledMidiEvent {
                event: MidiEvent::Clock,
                timestamp_ns: 0,
            },
        ]
    );

    // Switch to MidiToLink while playing with start_stop_sync = true: must emit Stop cleanly.
    let switch_out = engine.apply_command(
        RtCommand::SetDirection(Direction::MidiToLink),
        250_000_000,
        &mut link,
    );
    assert_eq!(
        switch_out.as_slice(),
        &[ScheduledMidiEvent {
            event: MidiEvent::Stop,
            timestamp_ns: 250_000_000,
        }]
    );
    assert_eq!(engine.snapshot().direction, Direction::MidiToLink);
    assert_eq!(engine.snapshot().lock_state, LockState::Unlocked);

    // Feed partial Acquiring ticks, then switch back to LinkToMidi and verify clean reset.
    engine.on_midi_event(MidiEvent::Clock, 300_000_000, &mut link);
    assert_eq!(engine.snapshot().lock_state, LockState::Acquiring);

    let back_out = engine.apply_command(
        RtCommand::SetDirection(Direction::LinkToMidi),
        350_000_000,
        &mut link,
    );
    assert!(back_out.is_empty());
    assert_eq!(engine.snapshot().direction, Direction::LinkToMidi);
    assert_eq!(engine.snapshot().lock_state, LockState::Unlocked);
    assert_eq!(engine.snapshot().bpm_midi_long, None);
    assert_eq!(engine.snapshot().bpm_midi_fast, None);
}

#[test]
fn link_to_midi_24ppqn_zero_drift_under_wakeup_jitter_and_latency_offset() {
    let cfg = SyncConfig {
        direction: Direction::LinkToMidi,
        start_stop_sync: false,
        midi_offset_ms: -10.0,
        ..SyncConfig::default()
    };
    let mut engine = SyncEngine::new(cfg);
    let mut link = MockLinkSession::new(120.0); // 1 tick = 20,833.333... us
    let mut rng = DeterministicRng::new(0x24_9991);

    let mut emitted = Vec::new();

    // First poll at -20 ms captures tick 0 (scheduled at -10 ms) cleanly.
    let mut deadline_ns = -20_000_000_i64;
    while emitted.len() < 240 {
        let wakeup_ns = deadline_ns + rng.non_negative(2_000_000);
        let res = engine.poll(wakeup_ns, &mut link);
        emitted.extend_from_slice(res.midi_out.as_slice());
        deadline_ns = res.next_deadline_ns.expect("LinkToMidi sets deadline");
    }

    for (idx, ev) in emitted.iter().take(240).enumerate() {
        assert_eq!(ev.event, MidiEvent::Clock);
        let ideal_link_us = ((idx as f64) * (60_000_000.0 / (24.0 * 120.0))).round() as i64;
        let expected_ns = ideal_link_us * 1_000 - 10_000_000;
        assert_eq!(
            ev.timestamp_ns, expected_ns,
            "tick {idx} drifted from exact TIMER_ABSTIME grid"
        );
    }

    // Now test positive offset (+15.5 ms) dynamically.
    let _ = engine.apply_command(RtCommand::SetOffsetMs(15.5), deadline_ns, &mut link);
    let res = engine.poll(deadline_ns + 30_000_000, &mut link);
    let next_ev = res.midi_out.as_slice()[0];
    let tick_idx = 240;
    let ideal_link_us = ((tick_idx as f64) * (60_000_000.0 / (24.0 * 120.0))).round() as i64;
    assert_eq!(next_ev.timestamp_ns, ideal_link_us * 1_000 + 15_500_000);
}

#[test]
fn link_to_midi_transport_quantized_start_stop_resync_and_disabled_sync() {
    let mut engine = SyncEngine::new(SyncConfig::default()); // quantum = 4.0, start_stop_sync = true
    let mut link = MockLinkSession::new(120.0); // 1 beat = 500 ms, 1 quantum = 2.0 s

    // Poll while Link is stopped: continuous 0xF8 without 0xFA/0xFC.
    let mut now_ns = 0_i64;
    let res0 = engine.poll(now_ns, &mut link);
    assert_eq!(res0.midi_out.as_slice().len(), 1);
    assert_eq!(res0.midi_out.as_slice()[0].event, MidiEvent::Clock);

    // Link starts playing mid-bar at t = 750 ms (beat 1.5).
    // Next quantum boundary is beat 4.0 (t = 2.0 s, tick 96).
    link.set_is_playing(true, 750_000);
    let mut all_events = Vec::new();
    while now_ns <= 2_050_000_000 {
        now_ns += 10_000_000;
        let res = engine.poll(now_ns, &mut link);
        all_events.extend_from_slice(res.midi_out.as_slice());
    }

    let starts: Vec<ScheduledMidiEvent> = all_events
        .iter()
        .copied()
        .filter(|e| e.event == MidiEvent::Start)
        .collect();
    assert_eq!(starts.len(), 1);
    assert_eq!(starts[0].timestamp_ns, 2_000_000_000);

    // Start must appear right before the downbeat Clock at the exact same timestamp.
    let start_pos = all_events
        .iter()
        .position(|e| e.event == MidiEvent::Start)
        .unwrap();
    assert_eq!(all_events[start_pos + 1].event, MidiEvent::Clock);
    assert_eq!(all_events[start_pos + 1].timestamp_ns, 2_000_000_000);

    // Manual ResyncDownbeat while playing re-arms Start for next quantum boundary (beat 8.0 = 4.0 s).
    let _ = engine.apply_command(RtCommand::ResyncDownbeat, 2_500_000_000, &mut link);
    let mut resync_events = Vec::new();
    while now_ns <= 4_050_000_000 {
        now_ns += 10_000_000;
        let res = engine.poll(now_ns, &mut link);
        resync_events.extend_from_slice(res.midi_out.as_slice());
    }
    let resync_starts: Vec<i64> = resync_events
        .iter()
        .filter(|e| e.event == MidiEvent::Start)
        .map(|e| e.timestamp_ns)
        .collect();
    assert_eq!(resync_starts, vec![4_000_000_000]);

    // Link stops: Stop (0xFC) emitted immediately.
    link.set_is_playing(false, 4_100_000);
    let stop_res = engine.poll(4_100_000_000, &mut link);
    assert!(stop_res
        .midi_out
        .as_slice()
        .iter()
        .any(|e| e.event == MidiEvent::Stop));

    // Disable start_stop_sync: Link playing transitions must NOT emit Start or Stop.
    let _ = engine.apply_command(RtCommand::SetStartStopSync(false), 4_200_000_000, &mut link);
    link.set_is_playing(true, 4_250_000);
    let mut disabled_events = Vec::new();
    for step in 1..=200 {
        let res = engine.poll(4_200_000_000 + step * 10_000_000, &mut link);
        disabled_events.extend_from_slice(res.midi_out.as_slice());
    }
    assert!(disabled_events.iter().all(|e| e.event == MidiEvent::Clock));
}

#[test]
fn midi_to_link_transport_start_continue_stop_and_bounded_phase_convergence() {
    for phase_offset_ms in [25_i64, -25_i64] {
        let mut engine = SyncEngine::new(midi_to_link_config());
        let mut link = MockLinkSession::new(120.0);

        // With 1 peer connected, Start forces beat 0 and starts transport immediately.
        link.peers = 1;
        engine.on_midi_event(MidiEvent::Start, 0, &mut link);
        assert!(link.is_playing());
        assert!(link.phase_at_time(0, 4.0).abs() < 1e-6);

        // Inject an initial Link phase offset relative to incoming MIDI clock at t = 0.
        link.force_beat_at_time(0.0, -phase_offset_ms * 1_000, 4.0);
        link.phase_nudges_us.clear();

        let tick_ns = 60_000_000_000.0 / (24.0 * 120.0);
        // Feed 30 seconds (60 beats) so 25 ms error converges at <= 1.0 ms/beat into ±4.0 ms deadband.
        for i in 0..(30 * 48) {
            let ts = ((i as f64) * tick_ns).round() as i64;
            engine.on_midi_event(MidiEvent::Clock, ts, &mut link);

            // Verify Stop keeps tempo locked while 0xF8 continues, and Continue resumes.
            if i == 240 {
                engine.on_midi_event(MidiEvent::Stop, ts, &mut link);
                assert!(!link.is_playing());
            }
            if i == 264 {
                assert_eq!(engine.snapshot().lock_state, LockState::Locked);
                engine.on_midi_event(MidiEvent::Continue, ts, &mut link);
                assert!(link.is_playing());
            }
        }

        assert!(!link.phase_nudges_us.is_empty());
        for &(_, nudge_us) in &link.phase_nudges_us {
            assert!(
                nudge_us.abs() <= 1_000,
                "nudge exceeded 1 ms/beat limit: {nudge_us} us"
            );
        }
        assert!(
            engine.snapshot().phase_error_ms.abs() <= 4.0,
            "offset {phase_offset_ms} ms failed to converge within ±4.0 ms deadband: got {} ms",
            engine.snapshot().phase_error_ms
        );
    }
}

#[test]
fn link_to_midi_catchup_forward_and_backward_jumps() {
    let mut engine = SyncEngine::new(SyncConfig::default());
    let mut link = MockLinkSession::new(120.0); // 1 tick = 20,833.333 us

    // Part A: forward wake-up gap (> 100 ms) resyncs without bursting stale 0xF8 ticks.
    let res0 = engine.poll(0, &mut link);
    assert_eq!(res0.midi_out.len(), 1);
    let res_gap = engine.poll(250_000_000, &mut link);
    assert_eq!(res_gap.midi_out.len(), 1);
    assert_eq!(res_gap.midi_out.as_slice()[0].timestamp_ns, 250_000_000);
    assert_eq!(res_gap.next_deadline_ns, Some(270_833_000));

    // Part B: backward Link timeline jump while playing (beat 100 -> beat 0 at t = 50 s).
    let _ = engine.poll(50_000_000_000, &mut link);
    link.force_beat_at_time(0.0, 50_000_000, 4.0);
    let res_back = engine.poll(50_005_000_000, &mut link);
    assert!(
        res_back.next_deadline_ns.unwrap() < 50_050_000_000,
        "scheduler froze after backward beat jump: next_deadline_ns={:?}",
        res_back.next_deadline_ns
    );
    let res_tick = engine.poll(50_021_000_000, &mut link);
    assert!(!res_tick.midi_out.is_empty());

    // Part C: backward Link jump while ArmedForStart re-arms Start to new bar 1 (beat 4).
    let mut armed_engine = SyncEngine::new(SyncConfig::default());
    let mut armed_link = MockLinkSession::new(120.0);
    let _ = armed_engine.poll(48_500_000_000, &mut armed_link); // beat 97.0
    armed_link.set_is_playing(true, 48_500_000);
    let _ = armed_engine.poll(48_500_000_000, &mut armed_link); // armed for beat 100.0
    armed_link.force_beat_at_time(0.5, 51_000_000, 4.0); // peer jumps backward to beat 0.5
    let mut events = Vec::new();
    let mut t = 51_000_000_000_i64;
    while t <= 53_000_000_000 {
        events.extend_from_slice(armed_engine.poll(t, &mut armed_link).midi_out.as_slice());
        t += 10_000_000;
    }
    let start_ts: Vec<i64> = events
        .iter()
        .filter(|e| e.event == MidiEvent::Start)
        .map(|e| e.timestamp_ns)
        .collect();
    // 3.5 beats from beat 0.5 at 120 BPM = 1.75 s -> 51.0 s + 1.75 s = 52.75 s.
    assert_eq!(start_ts, vec![52_750_000_000]);
}

#[test]
fn midi_to_link_corrects_external_peer_tempo_override_and_tracks_sub_38_bpm() {
    let mut engine = SyncEngine::new(midi_to_link_config());
    let mut link = MockLinkSession::new(120.0);

    let tick_120_ns = 60_000_000_000.0 / (24.0 * 120.0);
    let mut now_ns = 0_i64;
    for i in 0..144 {
        now_ns = ((i as f64) * tick_120_ns).round() as i64;
        engine.on_midi_event(MidiEvent::Clock, now_ns, &mut link);
    }
    assert_eq!(engine.snapshot().lock_state, LockState::Locked);

    // External Link peer drags Link tempo from 120.0 to 125.0; Tactus must restore 120.0.
    link.set_tempo(125.0, now_ns / 1_000);
    for j in 1..=24 {
        now_ns = (((143 + j) as f64) * tick_120_ns).round() as i64;
        engine.on_midi_event(MidiEvent::Clock, now_ns, &mut link);
    }
    assert!(
        (link.tempo() - 120.0).abs() <= 0.05,
        "failed to restore tempo after external peer override: got {}",
        link.tempo()
    );

    // Step down to 35.0 BPM (< 38.4 BPM, so < 24 ticks fit in 1.5 s fast window).
    let tick_35_ns = 60_000_000_000.0 / (24.0 * 35.0);
    let seg_start = now_ns;
    for k in 1..=420 {
        let ts = seg_start + ((k as f64) * tick_35_ns).round() as i64;
        engine.on_midi_event(MidiEvent::Clock, ts, &mut link);
    }
    let snap = engine.snapshot();
    assert_eq!(snap.lock_state, LockState::Locked);
    assert!(snap.bpm_midi_fast.is_some());
    assert!(
        (snap.bpm_link - 35.0).abs() <= 0.05,
        "failed to track sub-38.4 BPM tempo: got {}",
        snap.bpm_link
    );
}

#[test]
fn commands_quantum_manual_tempo_resync_and_idempotent_start_stop_sync() {
    let mut engine = SyncEngine::new(SyncConfig::default());
    let mut link = MockLinkSession::new(120.0);

    let _ = engine.apply_command(RtCommand::SetQuantum(3.0), 0, &mut link);
    let _ = engine.apply_command(RtCommand::SetLinkTempoManual(135.0), 0, &mut link);
    assert_eq!(engine.snapshot().quantum, 3.0);
    assert!((link.tempo() - 135.0).abs() < 1e-9);

    // Start playing mid-bar at beat 1.125 (500 ms at 135 BPM); quantum=3.0 arms Start at beat 3.0.
    let _ = engine.poll(0, &mut link);
    link.set_is_playing(true, 500_000);
    let _ = engine.poll(500_000_000, &mut link);

    // Re-sending SetStartStopSync(true) while ArmedForStart must NOT drop the pending Start.
    let _ = engine.apply_command(RtCommand::SetStartStopSync(true), 600_000_000, &mut link);

    let mut start_beats = Vec::new();
    let mut t = 510_000_000_i64;
    while t <= 1_600_000_000 {
        for ev in engine.poll(t, &mut link).midi_out.as_slice() {
            if ev.event == MidiEvent::Start {
                start_beats.push(link.beat_at_time(ev.timestamp_ns / 1_000, 3.0));
            }
        }
        t += 10_000_000;
    }
    assert_eq!(start_beats.len(), 1);
    assert!(
        (start_beats[0] - 3.0).abs() < 1e-3,
        "expected Start quantized to beat 3.0, got {}",
        start_beats[0]
    );

    // Switch to MidiToLink: SetLinkTempoManual is ignored, ResyncDownbeat aligns beat 0.
    let _ = engine.apply_command(RtCommand::SetDirection(Direction::MidiToLink), t, &mut link);
    let _ = engine.apply_command(RtCommand::SetLinkTempoManual(150.0), t, &mut link);
    assert!((link.tempo() - 135.0).abs() < 1e-9);

    let tick_135_ns = 60_000_000_000.0 / (24.0 * 135.0);
    let mut clk_ns = t;
    for k in 0..96 {
        clk_ns = t + ((k as f64) * tick_135_ns).round() as i64;
        engine.on_midi_event(MidiEvent::Clock, clk_ns, &mut link);
    }
    assert_eq!(engine.snapshot().lock_state, LockState::Locked);

    link.peers = 1;
    let forced_before = link.forced_beats.len();
    let _ = engine.apply_command(RtCommand::ResyncDownbeat, clk_ns, &mut link);
    for k in 96..=125 {
        let ts = t + ((k as f64) * tick_135_ns).round() as i64;
        engine.on_midi_event(MidiEvent::Clock, ts, &mut link);
    }
    assert_eq!(link.forced_beats.len(), forced_before + 1);
    assert_eq!(link.forced_beats.last().unwrap().0, 0.0);
}

#[test]
fn midi_to_link_start_forces_downbeat_with_peers_without_preroll() {
    let mut engine = SyncEngine::new(midi_to_link_config());
    let mut link = MockLinkSession::new(120.0);
    link.peers = 2;

    // At 120 BPM (500_000 us/beat), t = 1_150_000 us is beat 2.3 (mid-bar, far from beat 4.0).
    let start_us = 1_150_000_i64;
    assert!((link.phase_at_time(start_us, 4.0) - 2.3).abs() < 1e-6);

    engine.on_midi_event(MidiEvent::Start, start_us * 1_000, &mut link);

    assert!(link.is_playing());
    assert_eq!(link.play_changes.last(), Some(&(true, start_us - 1_000)));
    assert!(
        link.phase_at_time(start_us, 4.0).abs() < 1e-6,
        "expected downbeat 0.0 forced at Start instant, got phase={}",
        link.phase_at_time(start_us, 4.0)
    );

    // Simulate a remote Link peer launching via requestBeatAtStartPlayingTime(0, 4.0) with +400 us
    // GhostXForm clock-rounding skew: because startStopTime is 1 ms before `start_us`, the peer's
    // nextPhaseMatch snaps cleanly to `start_us` instead of rolling forward by a full 4-beat bar.
    link.request_beat_at_time(0.0, (start_us - 1_000) + 400, 4.0);
    assert_eq!(link.time_at_beat(0.0, 4.0), start_us);
}

#[test]
fn midi_to_link_clock_thru_relay_and_clean_stop_on_disable() {
    let mut engine = SyncEngine::new(midi_to_link_config());
    let mut link = MockLinkSession::new(120.0);

    // 1. clock_thru = true, start_stop_sync = true: relays Start, Clock, Continue, Stop.
    assert!(engine.snapshot().clock_thru);
    let r_start = engine.on_midi_event(MidiEvent::Start, 1_000_000, &mut link);
    let r_clock = engine.on_midi_event(MidiEvent::Clock, 21_833_000, &mut link);
    let r_stop = engine.on_midi_event(MidiEvent::Stop, 40_000_000, &mut link);
    let r_cont = engine.on_midi_event(MidiEvent::Continue, 60_000_000, &mut link);

    assert_eq!(r_start.as_slice()[0].event, MidiEvent::Start);
    assert_eq!(r_clock.as_slice()[0].event, MidiEvent::Clock);
    assert_eq!(r_stop.as_slice()[0].event, MidiEvent::Stop);
    assert_eq!(r_cont.as_slice()[0].event, MidiEvent::Continue);

    // 2. Disabling clock_thru while playing emits a clean Stop (0xFC) to relayed outputs.
    let disable_batch = engine.apply_command(RtCommand::SetClockThru(false), 70_000_000, &mut link);
    assert_eq!(
        disable_batch.as_slice(),
        &[ScheduledMidiEvent {
            event: MidiEvent::Stop,
            timestamp_ns: 70_000_000,
        }]
    );
    assert!(!engine.snapshot().clock_thru);

    // 3. While clock_thru = false, incoming MIDI events relay nothing.
    assert!(engine
        .on_midi_event(MidiEvent::Start, 80_000_000, &mut link)
        .is_empty());
    assert!(engine
        .on_midi_event(MidiEvent::Clock, 100_000_000, &mut link)
        .is_empty());

    // 4. Re-enable clock_thru with start_stop_sync = false: relays Clock only, suppresses Start/Continue/Stop.
    assert!(engine
        .apply_command(RtCommand::SetClockThru(true), 110_000_000, &mut link)
        .is_empty());
    assert!(engine
        .apply_command(RtCommand::SetStartStopSync(false), 110_000_000, &mut link)
        .is_empty());
    assert!(engine
        .on_midi_event(MidiEvent::Start, 120_000_000, &mut link)
        .is_empty());
    assert_eq!(
        engine
            .on_midi_event(MidiEvent::Clock, 140_000_000, &mut link)
            .as_slice(),
        &[ScheduledMidiEvent {
            event: MidiEvent::Clock,
            timestamp_ns: 140_000_000,
        }]
    );
    assert!(engine
        .on_midi_event(MidiEvent::Continue, 160_000_000, &mut link)
        .is_empty());
    assert!(engine
        .on_midi_event(MidiEvent::Stop, 180_000_000, &mut link)
        .is_empty());
}
