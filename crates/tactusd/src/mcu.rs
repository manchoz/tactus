//! Optional `Arduino_RouterBridge` MessagePack-RPC Unix-socket client (`MILESTONES.md` M5).
//!
//! Enforces the MCU bridge rate invariant (`AGENTS.md`): ZERO 24 PPQN MIDI Clock traffic crosses `/var/run/arduino-router.sock`.
//! Emits `tactus/state` only on connect and discrete state/`>= 0.1 BPM` changes, emits `tactus/downbeat`
//! on bar downbeat wraps while playing, emits `tactus/beat` on non-downbeat integer beat crossings
//! while playing, and forwards debounced MCU input commands to `ControlHandle`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use arduino_router::{
    Client, ClientConfig, CodecLimits, LifecycleEvent, ManagedClient, ReconnectPolicy, RpcError,
    Value,
};
use tactus_protocol::{Direction, EngineCommand, LockState, McuBridgeConfig, TelemetrySnapshot};
use tactus_sync_core::{MAX_VALID_BPM, MIN_VALID_BPM};
use tokio::net::UnixStream;
use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;
use tokio::time::{self, Instant};

use crate::control::ControlHandle;
use crate::log::{log_info, log_warn};

const INITIAL_RECONNECT_BACKOFF: Duration = Duration::from_millis(250);
const MAX_RECONNECT_BACKOFF: Duration = Duration::from_secs(5);
const DEFAULT_COMMAND_DEBOUNCE: Duration = Duration::from_millis(50);
const MAX_FRAME_BYTES: usize = 16 * 1024;
const MAX_DECODE_DEPTH: usize = 16;
const SOCKET_WRITE_TIMEOUT: Duration = Duration::from_secs(1);
const CMD_SEND_TIMEOUT: Duration = Duration::from_secs(1);
const BPM_CHANGE_THRESHOLD: f64 = 0.1 - 1e-9;

const METHOD_STATE: &str = "tactus/state";
const METHOD_DOWNBEAT: &str = "tactus/downbeat";
const METHOD_BEAT: &str = "tactus/beat";

const CMD_TOGGLE_DIRECTION: &str = "tactus/cmd/toggle_direction";
const CMD_SET_DIRECTION: &str = "tactus/cmd/set_direction";
const CMD_RESYNC_DOWNBEAT: &str = "tactus/cmd/resync_downbeat";
const CMD_TOGGLE_PLAY: &str = "tactus/cmd/toggle_play";
const CMD_SET_PLAYING: &str = "tactus/cmd/set_playing";
const CMD_ADJUST_BPM: &str = "tactus/cmd/adjust_bpm";
const CMD_SET_BPM: &str = "tactus/cmd/set_bpm";

const INCOMING_CMD_METHODS: [&str; 7] = [
    CMD_TOGGLE_DIRECTION,
    CMD_SET_DIRECTION,
    CMD_RESYNC_DOWNBEAT,
    CMD_TOGGLE_PLAY,
    CMD_SET_PLAYING,
    CMD_ADJUST_BPM,
    CMD_SET_BPM,
];

/// Spawn the optional `Arduino_RouterBridge` Unix-socket RPC task.
///
/// Returns `None` immediately when `cfg.enabled` is `false`.
pub fn spawn_mcu_bridge(
    cfg: &McuBridgeConfig,
    handle: ControlHandle,
    shutdown_rx: watch::Receiver<bool>,
) -> Option<JoinHandle<()>> {
    if !cfg.enabled {
        return None;
    }

    let socket_path = cfg.socket_path.clone();
    log_info!("MCU bridge enabled (socket=`{socket_path}`)");
    Some(tokio::spawn(run_mcu_bridge_loop(
        socket_path,
        handle,
        shutdown_rx,
    )))
}

fn mcu_client_config() -> ClientConfig {
    ClientConfig {
        limits: CodecLimits {
            max_frame_bytes: MAX_FRAME_BYTES,
            max_decode_depth: MAX_DECODE_DEPTH,
        },
        write_timeout: SOCKET_WRITE_TIMEOUT,
        max_concurrent_handlers: 1,
        ..ClientConfig::default()
    }
}

// --- Session & Reconnect Loop ---

async fn run_mcu_bridge_loop(
    socket_path: String,
    handle: ControlHandle,
    shutdown_rx: watch::Receiver<bool>,
) {
    let connect_path = socket_path.clone();
    run_mcu_bridge_with_connector(
        &socket_path,
        move || {
            let p = connect_path.clone();
            async move { UnixStream::connect(p).await }
        },
        handle,
        shutdown_rx,
        DEFAULT_COMMAND_DEBOUNCE,
    )
    .await;
}

async fn sync_initial_state(
    client: Option<Client>,
    state_rx: &mut watch::Receiver<tactus_protocol::AppState>,
    tracker: &mut TelemetryTracker,
) -> Option<Client> {
    let client = client?;
    let snap = state_rx.borrow_and_update().telemetry;
    *tracker = TelemetryTracker::new(snap);
    if let Err(err) = emit_state_notification(&client, &snap).await {
        log_warn!("MCU bridge initial state write failed: {err}");
        None
    } else {
        Some(client)
    }
}

async fn run_mcu_bridge_with_connector<F, Fut>(
    socket_label: &str,
    connect: F,
    handle: ControlHandle,
    mut shutdown_rx: watch::Receiver<bool>,
    debounce: Duration,
) where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = std::io::Result<UnixStream>> + Send + 'static,
{
    if *shutdown_rx.borrow_and_update() {
        log_info!("MCU bridge stopped");
        return;
    }

    let debouncer = Arc::new(Mutex::new(CommandDebouncer::new(debounce)));
    let mut builder = ManagedClient::builder_with_connector(connect)
        .client_config(mcu_client_config())
        .reconnect_policy(ReconnectPolicy {
            initial_backoff: INITIAL_RECONNECT_BACKOFF,
            max_backoff: MAX_RECONNECT_BACKOFF,
            multiplier: 2,
        });

    for &method in &INCOMING_CMD_METHODS {
        let h = handle.clone();
        let d = Arc::clone(&debouncer);
        builder = builder.provide_raw(method, move |params| {
            let h = h.clone();
            let d = Arc::clone(&d);
            async move { handle_mcu_rpc(method, params, &h, &d).await }
        });
    }

    let managed = builder.spawn();
    let mut events = managed.subscribe_events();
    let mut state_rx = handle.state_rx.clone();
    let mut tracker = TelemetryTracker::new(state_rx.borrow_and_update().telemetry);
    let mut active_client: Option<Client> = None;
    let mut connect_err_logged = false;

    loop {
        if *shutdown_rx.borrow_and_update() {
            break;
        }

        tokio::select! {
            biased;
            () = wait_shutdown(&mut shutdown_rx) => break,
            ev = events.recv() => {
                match ev {
                    Ok(LifecycleEvent::Connected) => {
                        connect_err_logged = false;
                        log_info!("MCU bridge connected to `{socket_label}`");
                        active_client = sync_initial_state(
                            managed.current_client(),
                            &mut state_rx,
                            &mut tracker,
                        )
                        .await;
                    }
                    Ok(LifecycleEvent::Disconnected { .. }) => {
                        active_client = None;
                        log_info!("MCU bridge peer closed connection");
                    }
                    Ok(LifecycleEvent::ConnectFailed { error, .. }) => {
                        active_client = None;
                        if !connect_err_logged {
                            log_warn!(
                                "MCU bridge failed connecting to `{socket_label}` ({error}); retrying with backoff"
                            );
                            connect_err_logged = true;
                        }
                    }
                    Ok(LifecycleEvent::RegisterFailed { method, error }) => {
                        log_warn!("MCU bridge $/register `{method}` failed: {error}");
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        active_client = sync_initial_state(
                            managed.current_client(),
                            &mut state_rx,
                            &mut tracker,
                        )
                        .await;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            changed = state_rx.changed(), if active_client.is_some() => {
                if changed.is_err() {
                    break;
                }
                if let Some(ref client) = active_client {
                    let snap = state_rx.borrow_and_update().telemetry;
                    let emit = tracker.evaluate(snap);
                    if let Err(err) = emit_telemetry_events(client, emit, &snap).await {
                        log_warn!("MCU bridge telemetry write failed: {err}");
                        active_client = None;
                    }
                }
            }
        }
    }

    managed.shutdown().await;
    log_info!("MCU bridge stopped");
}

async fn handle_mcu_rpc(
    method: &'static str,
    params: Vec<Value>,
    handle: &ControlHandle,
    debouncer: &Mutex<CommandDebouncer>,
) -> Result<Value, RpcError> {
    let snap = handle.state_rx.borrow().telemetry;
    match translate_mcu_command(method, &params, &snap) {
        Ok((kind, cmd)) => {
            let allowed = {
                let mut guard = debouncer.lock().expect("debouncer mutex poisoned");
                guard.allow(kind, Instant::now())
            };
            if allowed && !forward_engine_command(handle, cmd).await {
                return Err(RpcError::generic("control plane closed"));
            }
            Ok(Value::Bool(true))
        }
        Err(reason) => {
            log_warn!("rejecting MCU RPC `{method}`: {reason}");
            Err(RpcError::malformed_call(reason))
        }
    }
}

async fn forward_engine_command(handle: &ControlHandle, cmd: EngineCommand) -> bool {
    match time::timeout(CMD_SEND_TIMEOUT, handle.cmd_tx.send(cmd)).await {
        Ok(Ok(())) => true,
        Ok(Err(_)) => false,
        Err(_) => {
            log_warn!("dropping MCU command: control channel saturated");
            true
        }
    }
}

async fn wait_shutdown(rx: &mut watch::Receiver<bool>) {
    if *rx.borrow_and_update() {
        return;
    }
    while rx.changed().await.is_ok() {
        if *rx.borrow_and_update() {
            return;
        }
    }
}

// --- Telemetry Diffing & Beat / Downbeat Detection ---

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BeatEvent {
    Downbeat,
    Beat(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TelemetryEmit {
    state: bool,
    beat: Option<BeatEvent>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct TelemetryTracker {
    last_sent_bpm: f64,
    last_sent_direction: Direction,
    last_sent_lock_state: LockState,
    last_sent_playing: bool,
    prev_beat: Option<u32>,
    prev_phase: Option<f64>,
}

impl TelemetryTracker {
    fn new(initial: TelemetrySnapshot) -> Self {
        Self {
            last_sent_bpm: initial.bpm_link,
            last_sent_direction: initial.direction,
            last_sent_lock_state: initial.lock_state,
            last_sent_playing: initial.is_playing,
            prev_beat: Some(beat_index(initial.beat_phase)),
            prev_phase: Some(initial.beat_phase),
        }
    }

    /// Evaluate a telemetry snapshot and return discrete state / beat RPC events to emit.
    fn evaluate(&mut self, snap: TelemetrySnapshot) -> TelemetryEmit {
        let state_changed = snap.is_playing != self.last_sent_playing
            || snap.direction != self.last_sent_direction
            || snap.lock_state != self.last_sent_lock_state
            || (snap.bpm_link - self.last_sent_bpm).abs() >= BPM_CHANGE_THRESHOLD;

        let cur_beat = beat_index(snap.beat_phase);
        let downbeat_wrapped = snap.is_playing
            && cur_beat == 0
            && (self.prev_beat != Some(0)
                || self.prev_phase.is_some_and(|p| snap.beat_phase + 0.5 < p));
        let beat = if downbeat_wrapped {
            Some(BeatEvent::Downbeat)
        } else if snap.is_playing && self.prev_beat.is_some_and(|prev| cur_beat != prev) {
            Some(BeatEvent::Beat(cur_beat))
        } else {
            None
        };

        self.prev_beat = Some(cur_beat);
        self.prev_phase = Some(snap.beat_phase);

        if state_changed {
            self.last_sent_bpm = snap.bpm_link;
            self.last_sent_direction = snap.direction;
            self.last_sent_lock_state = snap.lock_state;
            self.last_sent_playing = snap.is_playing;
        }

        TelemetryEmit {
            state: state_changed,
            beat,
        }
    }
}

#[inline]
fn beat_index(beat_phase: f64) -> u32 {
    beat_phase.max(0.0).floor() as u32
}

const fn direction_to_i32(dir: Direction) -> i32 {
    match dir {
        Direction::LinkToMidi => 0,
        Direction::MidiToLink => 1,
    }
}

const fn lock_state_to_i32(lock: LockState) -> i32 {
    match lock {
        LockState::Unlocked => 0,
        LockState::Acquiring => 1,
        LockState::Locked => 2,
    }
}

fn build_telemetry_params(beat_idx: Option<u32>, snap: &TelemetrySnapshot) -> Vec<Value> {
    let bpm_tenths = (snap.bpm_link * 10.0).round() as i32;
    let mut params = Vec::with_capacity(5);
    if let Some(idx) = beat_idx {
        params.push(Value::Int(i64::from(idx)));
    }
    params.push(Value::Int(i64::from(bpm_tenths)));
    params.push(Value::Int(i64::from(direction_to_i32(snap.direction))));
    params.push(Value::Int(i64::from(lock_state_to_i32(snap.lock_state))));
    params.push(Value::Bool(snap.is_playing));
    params
}

async fn emit_state_notification(
    client: &Client,
    snap: &TelemetrySnapshot,
) -> arduino_router::Result<()> {
    client
        .notify_raw(METHOD_STATE, build_telemetry_params(None, snap))
        .await
}

async fn emit_telemetry_events(
    client: &Client,
    emit: TelemetryEmit,
    snap: &TelemetrySnapshot,
) -> arduino_router::Result<()> {
    if emit.state {
        emit_state_notification(client, snap).await?;
    }
    match emit.beat {
        Some(BeatEvent::Downbeat) => {
            client
                .notify_raw(METHOD_DOWNBEAT, build_telemetry_params(None, snap))
                .await?;
        }
        Some(BeatEvent::Beat(beat_idx)) => {
            client
                .notify_raw(METHOD_BEAT, build_telemetry_params(Some(beat_idx), snap))
                .await?;
        }
        None => {}
    }
    Ok(())
}

// --- Inbound MCU Command Debouncing & Translation ---

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum McuCommandKind {
    ToggleDirection = 0,
    SetDirection = 1,
    ResyncDownbeat = 2,
    TogglePlay = 3,
    SetPlaying = 4,
    AdjustBpm = 5,
    SetBpm = 6,
}

struct CommandDebouncer {
    debounce: Duration,
    last_accepted: [Option<Instant>; 7],
}

impl CommandDebouncer {
    const fn new(debounce: Duration) -> Self {
        Self {
            debounce,
            last_accepted: [None; 7],
        }
    }

    fn allow(&mut self, kind: McuCommandKind, now: Instant) -> bool {
        if self.debounce.is_zero() {
            return true;
        }
        let slot = &mut self.last_accepted[kind as usize];
        if let Some(prev) = *slot {
            if now.saturating_duration_since(prev) < self.debounce {
                return false;
            }
        }
        *slot = Some(now);
        true
    }
}

fn translate_mcu_command(
    method: &str,
    params: &[Value],
    snap: &TelemetrySnapshot,
) -> Result<(McuCommandKind, EngineCommand), &'static str> {
    match method {
        CMD_TOGGLE_DIRECTION => {
            if !params.is_empty() {
                return Err("toggle_direction takes no parameters");
            }
            let next = match snap.direction {
                Direction::LinkToMidi => Direction::MidiToLink,
                Direction::MidiToLink => Direction::LinkToMidi,
            };
            Ok((
                McuCommandKind::ToggleDirection,
                EngineCommand::SetDirection(next),
            ))
        }
        CMD_SET_DIRECTION => {
            if params.len() != 1 {
                return Err("set_direction expects 1 parameter");
            }
            let dir = match &params[0] {
                Value::Int(0) | Value::Uint(0) => Direction::LinkToMidi,
                Value::Int(1) | Value::Uint(1) => Direction::MidiToLink,
                Value::Str(s) if s == "link_to_midi" => Direction::LinkToMidi,
                Value::Str(s) if s == "midi_to_link" => Direction::MidiToLink,
                _ => return Err("invalid direction parameter"),
            };
            Ok((
                McuCommandKind::SetDirection,
                EngineCommand::SetDirection(dir),
            ))
        }
        CMD_RESYNC_DOWNBEAT => {
            if !params.is_empty() {
                return Err("resync_downbeat takes no parameters");
            }
            Ok((
                McuCommandKind::ResyncDownbeat,
                EngineCommand::ResyncDownbeat,
            ))
        }
        CMD_TOGGLE_PLAY => {
            if !params.is_empty() {
                return Err("toggle_play takes no parameters");
            }
            Ok((
                McuCommandKind::TogglePlay,
                EngineCommand::SetLinkPlaying(!snap.is_playing),
            ))
        }
        CMD_SET_PLAYING => {
            if params.len() != 1 {
                return Err("set_playing expects 1 boolean parameter");
            }
            let Value::Bool(playing) = params[0] else {
                return Err("invalid playing parameter");
            };
            Ok((
                McuCommandKind::SetPlaying,
                EngineCommand::SetLinkPlaying(playing),
            ))
        }
        CMD_ADJUST_BPM => {
            if params.len() != 1 {
                return Err("adjust_bpm expects 1 numeric parameter");
            }
            let Some(delta) = params[0].as_f64() else {
                return Err("invalid bpm delta parameter");
            };
            if !delta.is_finite() {
                return Err("non-finite bpm delta");
            }
            if !snap.bpm_link.is_finite() {
                return Err("current bpm is non-finite");
            }
            let next = (snap.bpm_link + delta).clamp(MIN_VALID_BPM, MAX_VALID_BPM);
            Ok((
                McuCommandKind::AdjustBpm,
                EngineCommand::SetLinkTempoManual(next),
            ))
        }
        CMD_SET_BPM => {
            if params.len() != 1 {
                return Err("set_bpm expects 1 numeric parameter");
            }
            let Some(bpm) = params[0].as_f64() else {
                return Err("invalid bpm parameter");
            };
            if !bpm.is_finite() || !(MIN_VALID_BPM..=MAX_VALID_BPM).contains(&bpm) {
                return Err("bpm out of valid range");
            }
            Ok((
                McuCommandKind::SetBpm,
                EngineCommand::SetLinkTempoManual(bpm),
            ))
        }
        _ => Err("unknown RPC method"),
    }
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use arduino_router::{
        decode_frame, encode_frame, DecodeError, ErrorCode, ReconnectPolicy, RpcMessage,
        METHOD_REGISTER,
    };
    use tactus_protocol::AppState;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::mpsc;

    use super::*;

    struct TestBridge {
        peer: UnixStream,
        read_buf: Vec<u8>,
        state_tx: watch::Sender<AppState>,
        cmd_rx: mpsc::Receiver<EngineCommand>,
        shutdown_tx: watch::Sender<bool>,
        handle: JoinHandle<()>,
    }

    async fn spawn_test_bridge(initial_snap: TelemetrySnapshot, debounce: Duration) -> TestBridge {
        let (server, peer) = UnixStream::pair().expect("create unix stream pair");
        let initial_state = AppState {
            telemetry: initial_snap,
            ..AppState::default()
        };
        let (state_tx, state_rx) = watch::channel(initial_state);
        let (cmd_tx, cmd_rx) = mpsc::channel(64);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let handle_arg = ControlHandle { state_rx, cmd_tx };

        let server_slot = Arc::new(Mutex::new(Some(server)));
        let handle = tokio::spawn(run_mcu_bridge_with_connector(
            "test-pair",
            move || {
                let slot = Arc::clone(&server_slot);
                async move {
                    slot.lock()
                        .expect("server slot poisoned")
                        .take()
                        .ok_or_else(|| {
                            std::io::Error::new(
                                std::io::ErrorKind::NotConnected,
                                "test stream already consumed",
                            )
                        })
                }
            },
            handle_arg,
            shutdown_rx,
            debounce,
        ));

        TestBridge {
            peer,
            read_buf: Vec::new(),
            state_tx,
            cmd_rx,
            shutdown_tx,
            handle,
        }
    }

    async fn read_rpc_frame(peer: &mut UnixStream, buf: &mut Vec<u8>) -> RpcMessage {
        let mut tmp = [0u8; 1024];
        let limits = CodecLimits {
            max_frame_bytes: MAX_FRAME_BYTES,
            max_decode_depth: MAX_DECODE_DEPTH,
        };
        loop {
            match decode_frame(buf, limits) {
                Ok((msg, consumed)) => {
                    buf.drain(..consumed);
                    return msg;
                }
                Err(DecodeError::Incomplete) => {}
                Err(err) => panic!("unexpected decode error: {err:?}"),
            }

            let n = time::timeout(Duration::from_secs(2), peer.read(&mut tmp))
                .await
                .expect("timed out waiting for RPC frame")
                .expect("read peer stream");
            assert!(n > 0, "unexpected EOF while waiting for RPC frame");
            buf.extend_from_slice(&tmp[..n]);
        }
    }

    async fn send_rpc(peer: &mut UnixStream, msg: &RpcMessage) {
        let mut encoded = Vec::new();
        encode_frame(msg, &mut encoded).expect("encode rpc message");
        peer.write_all(&encoded).await.expect("write rpc message");
    }

    async fn consume_handshake(tb: &mut TestBridge) -> (Vec<String>, RpcMessage) {
        let mut registered = Vec::with_capacity(INCOMING_CMD_METHODS.len());
        for expected_idx in 0..INCOMING_CMD_METHODS.len() {
            let msg = read_rpc_frame(&mut tb.peer, &mut tb.read_buf).await;
            match msg {
                RpcMessage::Request {
                    msgid,
                    method,
                    params,
                } => {
                    assert_eq!(msgid, (expected_idx as u32) + 1);
                    assert_eq!(method, METHOD_REGISTER);
                    assert_eq!(params.len(), 1);
                    let Value::Str(cmd_name) = &params[0] else {
                        panic!("expected string method parameter");
                    };
                    registered.push(cmd_name.clone());
                    send_rpc(
                        &mut tb.peer,
                        &RpcMessage::Response {
                            msgid,
                            error: Value::Nil,
                            result: Value::Bool(true),
                        },
                    )
                    .await;
                }
                other => panic!("expected $/register request, got {other:?}"),
            }
        }
        let initial_state = read_rpc_frame(&mut tb.peer, &mut tb.read_buf).await;
        (registered, initial_state)
    }

    #[test]
    fn telemetry_tracker_enforces_rate_boundary_and_quantum_one_wrap() {
        let init = TelemetrySnapshot {
            bpm_link: 120.0,
            direction: Direction::LinkToMidi,
            lock_state: LockState::Unlocked,
            is_playing: true,
            beat_phase: 0.0,
            quantum: 4.0,
            ..TelemetrySnapshot::default()
        };
        let mut tracker = TelemetryTracker::new(init);

        // 24 PPQN sub-beat ticks emit nothing; only integer beat crossings (1, 2, 3) emit tactus/beat.
        for step in 1..96 {
            let phase = (f64::from(step) / 24.0) % 4.0;
            let mut snap = init;
            snap.beat_phase = phase;
            let expected_beat = if step % 24 == 0 {
                Some(BeatEvent::Beat(step / 24))
            } else {
                None
            };
            assert_eq!(
                tracker.evaluate(snap),
                TelemetryEmit {
                    state: false,
                    beat: expected_beat,
                },
                "step={step} phase={phase}"
            );
        }

        // Downbeat wrap from 3.958 -> 0.0 triggers downbeat only.
        let mut wrap = init;
        wrap.beat_phase = 0.0;
        assert_eq!(
            tracker.evaluate(wrap),
            TelemetryEmit {
                state: false,
                beat: Some(BeatEvent::Downbeat),
            }
        );

        // Sub-beat jitter < 0.1 BPM emits nothing; exact 0.1 BPM step emits state.
        let mut jitter = wrap;
        jitter.beat_phase = 0.25;
        jitter.bpm_link = 120.09;
        assert_eq!(
            tracker.evaluate(jitter),
            TelemetryEmit {
                state: false,
                beat: None,
            }
        );

        let mut step_up = jitter;
        step_up.beat_phase = 0.50;
        step_up.bpm_link = 120.1;
        assert_eq!(
            tracker.evaluate(step_up),
            TelemetryEmit {
                state: true,
                beat: None,
            }
        );

        // quantum == 1.0 wrap triggers via phase drop even though cur_beat stays 0.
        let q1_init = TelemetrySnapshot {
            quantum: 1.0,
            is_playing: true,
            beat_phase: 0.90,
            ..init
        };
        let mut q1_tracker = TelemetryTracker::new(q1_init);
        let q1_wrap = TelemetrySnapshot {
            beat_phase: 0.02,
            ..q1_init
        };
        assert_eq!(
            q1_tracker.evaluate(q1_wrap),
            TelemetryEmit {
                state: false,
                beat: Some(BeatEvent::Downbeat),
            }
        );

        // Shrinking quantum mid-bar (e.g. 6/4 -> 4/4, phase 5.2 -> 1.2) drops phase
        // without landing on beat 0, so it must emit tactus/beat(1) instead of downbeat.
        let shrink_init = TelemetrySnapshot {
            quantum: 6.0,
            is_playing: true,
            beat_phase: 5.2,
            ..init
        };
        let mut shrink_tracker = TelemetryTracker::new(shrink_init);
        assert_eq!(
            shrink_tracker.evaluate(TelemetrySnapshot {
                quantum: 4.0,
                beat_phase: 1.2,
                ..shrink_init
            }),
            TelemetryEmit {
                state: false,
                beat: Some(BeatEvent::Beat(1)),
            }
        );

        // Stopped transport never triggers beat or downbeat.
        let stopped_init = TelemetrySnapshot {
            is_playing: false,
            beat_phase: 0.9,
            ..init
        };
        let mut stopped_tracker = TelemetryTracker::new(stopped_init);
        assert_eq!(
            stopped_tracker.evaluate(TelemetrySnapshot {
                beat_phase: 1.0,
                ..stopped_init
            }),
            TelemetryEmit {
                state: false,
                beat: None,
            }
        );
        assert_eq!(
            stopped_tracker.evaluate(TelemetrySnapshot {
                beat_phase: 3.9,
                ..stopped_init
            }),
            TelemetryEmit {
                state: false,
                beat: None,
            }
        );
        assert_eq!(
            stopped_tracker.evaluate(TelemetrySnapshot {
                beat_phase: 0.0,
                ..stopped_init
            }),
            TelemetryEmit {
                state: false,
                beat: None,
            }
        );

        // Simultaneous beat crossing + state change emits both.
        let both_beat_init = TelemetrySnapshot {
            is_playing: true,
            beat_phase: 0.9,
            bpm_link: 120.0,
            lock_state: LockState::Acquiring,
            ..init
        };
        let mut both_beat_tracker = TelemetryTracker::new(both_beat_init);
        assert_eq!(
            both_beat_tracker.evaluate(TelemetrySnapshot {
                beat_phase: 1.02,
                bpm_link: 120.2,
                lock_state: LockState::Locked,
                ..both_beat_init
            }),
            TelemetryEmit {
                state: true,
                beat: Some(BeatEvent::Beat(1)),
            }
        );

        // Simultaneous downbeat wrap + state change updates last_sent and emits both.
        let both_init = TelemetrySnapshot {
            is_playing: true,
            beat_phase: 3.9,
            bpm_link: 120.0,
            lock_state: LockState::Acquiring,
            ..init
        };
        let mut both_tracker = TelemetryTracker::new(both_init);
        assert_eq!(
            both_tracker.evaluate(TelemetrySnapshot {
                beat_phase: 0.02,
                bpm_link: 120.5,
                lock_state: LockState::Locked,
                ..both_init
            }),
            TelemetryEmit {
                state: true,
                beat: Some(BeatEvent::Downbeat),
            }
        );
        // Following sub-beat tick with identical updated state emits zero frames.
        assert_eq!(
            both_tracker.evaluate(TelemetrySnapshot {
                beat_phase: 0.10,
                bpm_link: 120.52,
                lock_state: LockState::Locked,
                ..both_init
            }),
            TelemetryEmit {
                state: false,
                beat: None,
            }
        );
    }

    #[tokio::test]
    async fn connect_registers_methods_and_emits_initial_state() {
        let init = TelemetrySnapshot {
            bpm_link: 124.6,
            direction: Direction::MidiToLink,
            lock_state: LockState::Acquiring,
            is_playing: true,
            ..TelemetrySnapshot::default()
        };
        let mut tb = spawn_test_bridge(init, Duration::ZERO).await;

        let (registered, initial_state) = consume_handshake(&mut tb).await;
        let mut expected: Vec<String> = INCOMING_CMD_METHODS
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        expected.sort();
        assert_eq!(registered, expected);
        assert_eq!(
            initial_state,
            RpcMessage::Notification {
                method: METHOD_STATE.to_string(),
                params: vec![
                    Value::Int(1246),
                    Value::Int(1),
                    Value::Int(1),
                    Value::Bool(true),
                ],
            }
        );

        // Unmatched response frames are cleanly ignored.
        send_rpc(
            &mut tb.peer,
            &RpcMessage::Response {
                msgid: 1,
                error: Value::Nil,
                result: Value::Bool(true),
            },
        )
        .await;

        let _ = tb.shutdown_tx.send(true);
        tb.handle.await.expect("session join");
    }

    #[tokio::test]
    async fn subbeat_ticks_emit_zero_frames_and_beat_and_downbeat_emit_once() {
        let init = TelemetrySnapshot {
            bpm_link: 120.0,
            direction: Direction::LinkToMidi,
            lock_state: LockState::Locked,
            is_playing: true,
            beat_phase: 0.0,
            quantum: 4.0,
            ..TelemetrySnapshot::default()
        };
        let mut tb = spawn_test_bridge(init, Duration::ZERO).await;
        let _ = consume_handshake(&mut tb).await;

        // Simulate 24 sub-beat ticks across beat 0 (0.04..0.96) -> zero RPC frames.
        for tick in 1..24 {
            let phase = f64::from(tick) / 24.0;
            tb.state_tx.send_modify(|s| s.telemetry.beat_phase = phase);
            time::sleep(Duration::from_millis(2)).await;
        }

        // Cross beats 1, 2, 3: each emits exactly one tactus/beat, while sub-beats emit nothing.
        for beat in 1..4 {
            tb.state_tx
                .send_modify(|s| s.telemetry.beat_phase = f64::from(beat));
            let frame = read_rpc_frame(&mut tb.peer, &mut tb.read_buf).await;
            assert_eq!(
                frame,
                RpcMessage::Notification {
                    method: METHOD_BEAT.to_string(),
                    params: vec![
                        Value::Int(i64::from(beat)),
                        Value::Int(1200),
                        Value::Int(0),
                        Value::Int(2),
                        Value::Bool(true),
                    ],
                }
            );
            for sub in [0.25, 0.5, 0.75, 0.96] {
                tb.state_tx
                    .send_modify(|s| s.telemetry.beat_phase = f64::from(beat) + sub);
                time::sleep(Duration::from_millis(2)).await;
            }
        }

        // Now wrap back to downbeat 0.02: the very next RPC frame MUST be tactus/downbeat.
        tb.state_tx.send_modify(|s| s.telemetry.beat_phase = 0.02);
        let frame = read_rpc_frame(&mut tb.peer, &mut tb.read_buf).await;
        assert_eq!(
            frame,
            RpcMessage::Notification {
                method: METHOD_DOWNBEAT.to_string(),
                params: vec![
                    Value::Int(1200),
                    Value::Int(0),
                    Value::Int(2),
                    Value::Bool(true),
                ],
            }
        );

        let _ = tb.shutdown_tx.send(true);
        tb.handle.await.expect("session join");
    }

    #[tokio::test]
    async fn state_changes_emit_while_sub_tenth_bpm_jitter_is_suppressed() {
        let init = TelemetrySnapshot {
            bpm_link: 120.0,
            direction: Direction::LinkToMidi,
            lock_state: LockState::Unlocked,
            is_playing: false,
            beat_phase: 0.0,
            ..TelemetrySnapshot::default()
        };
        let mut tb = spawn_test_bridge(init, Duration::ZERO).await;
        let _ = consume_handshake(&mut tb).await;

        // < 0.1 BPM jitter should emit nothing.
        tb.state_tx.send_modify(|s| s.telemetry.bpm_link = 120.04);
        time::sleep(Duration::from_millis(5)).await;
        tb.state_tx.send_modify(|s| s.telemetry.bpm_link = 120.09);
        time::sleep(Duration::from_millis(5)).await;

        // >= 0.1 BPM change emits tactus/state.
        tb.state_tx.send_modify(|s| s.telemetry.bpm_link = 120.1);
        assert_eq!(
            read_rpc_frame(&mut tb.peer, &mut tb.read_buf).await,
            RpcMessage::Notification {
                method: METHOD_STATE.to_string(),
                params: vec![
                    Value::Int(1201),
                    Value::Int(0),
                    Value::Int(0),
                    Value::Bool(false),
                ],
            }
        );

        // Play/stop transition emits tactus/state.
        tb.state_tx.send_modify(|s| s.telemetry.is_playing = true);
        assert_eq!(
            read_rpc_frame(&mut tb.peer, &mut tb.read_buf).await,
            RpcMessage::Notification {
                method: METHOD_STATE.to_string(),
                params: vec![
                    Value::Int(1201),
                    Value::Int(0),
                    Value::Int(0),
                    Value::Bool(true),
                ],
            }
        );

        // Direction transition emits tactus/state.
        tb.state_tx
            .send_modify(|s| s.telemetry.direction = Direction::MidiToLink);
        assert_eq!(
            read_rpc_frame(&mut tb.peer, &mut tb.read_buf).await,
            RpcMessage::Notification {
                method: METHOD_STATE.to_string(),
                params: vec![
                    Value::Int(1201),
                    Value::Int(1),
                    Value::Int(0),
                    Value::Bool(true),
                ],
            }
        );

        // Lock-state transition emits tactus/state.
        tb.state_tx
            .send_modify(|s| s.telemetry.lock_state = LockState::Locked);
        assert_eq!(
            read_rpc_frame(&mut tb.peer, &mut tb.read_buf).await,
            RpcMessage::Notification {
                method: METHOD_STATE.to_string(),
                params: vec![
                    Value::Int(1201),
                    Value::Int(1),
                    Value::Int(2),
                    Value::Bool(true),
                ],
            }
        );

        let _ = tb.shutdown_tx.send(true);
        tb.handle.await.expect("session join");
    }

    #[tokio::test]
    async fn incoming_notifications_requests_and_debounce_work_end_to_end() {
        let init = TelemetrySnapshot {
            bpm_link: 120.0,
            direction: Direction::LinkToMidi,
            is_playing: false,
            ..TelemetrySnapshot::default()
        };
        let mut tb = spawn_test_bridge(init, Duration::from_millis(80)).await;
        let _ = consume_handshake(&mut tb).await;

        // 1. Notification: toggle_direction -> MidiToLink.
        send_rpc(
            &mut tb.peer,
            &RpcMessage::Notification {
                method: CMD_TOGGLE_DIRECTION.to_string(),
                params: vec![],
            },
        )
        .await;
        assert_eq!(
            tb.cmd_rx.recv().await,
            Some(EngineCommand::SetDirection(Direction::MidiToLink))
        );

        // Duplicate toggle_direction within 80ms debounce is suppressed.
        send_rpc(
            &mut tb.peer,
            &RpcMessage::Notification {
                method: CMD_TOGGLE_DIRECTION.to_string(),
                params: vec![],
            },
        )
        .await;

        // Different command kind (resync_downbeat) as Request passes immediately and replies [1, msgid, nil, true].
        send_rpc(
            &mut tb.peer,
            &RpcMessage::Request {
                msgid: 42,
                method: CMD_RESYNC_DOWNBEAT.to_string(),
                params: vec![],
            },
        )
        .await;
        assert_eq!(tb.cmd_rx.recv().await, Some(EngineCommand::ResyncDownbeat));
        assert_eq!(
            read_rpc_frame(&mut tb.peer, &mut tb.read_buf).await,
            RpcMessage::Response {
                msgid: 42,
                error: Value::Nil,
                result: Value::Bool(true),
            }
        );

        // Wait past debounce and send remaining command types (split across partial writes!).
        time::sleep(Duration::from_millis(90)).await;

        let mut partial = Vec::new();
        encode_frame(
            &RpcMessage::Request {
                msgid: 43,
                method: CMD_SET_DIRECTION.to_string(),
                params: vec![Value::Str("link_to_midi".to_string())],
            },
            &mut partial,
        )
        .expect("encode set_direction");
        tb.peer.write_all(&partial[..3]).await.expect("part 1");
        time::sleep(Duration::from_millis(5)).await;
        tb.peer.write_all(&partial[3..]).await.expect("part 2");
        assert_eq!(
            tb.cmd_rx.recv().await,
            Some(EngineCommand::SetDirection(Direction::LinkToMidi))
        );
        assert_eq!(
            read_rpc_frame(&mut tb.peer, &mut tb.read_buf).await,
            RpcMessage::Response {
                msgid: 43,
                error: Value::Nil,
                result: Value::Bool(true),
            }
        );

        // toggle_play, set_playing, adjust_bpm, set_bpm
        send_rpc(
            &mut tb.peer,
            &RpcMessage::Notification {
                method: CMD_TOGGLE_PLAY.to_string(),
                params: vec![],
            },
        )
        .await;
        assert_eq!(
            tb.cmd_rx.recv().await,
            Some(EngineCommand::SetLinkPlaying(true))
        );

        send_rpc(
            &mut tb.peer,
            &RpcMessage::Notification {
                method: CMD_SET_PLAYING.to_string(),
                params: vec![Value::Bool(false)],
            },
        )
        .await;
        assert_eq!(
            tb.cmd_rx.recv().await,
            Some(EngineCommand::SetLinkPlaying(false))
        );

        send_rpc(
            &mut tb.peer,
            &RpcMessage::Notification {
                method: CMD_ADJUST_BPM.to_string(),
                params: vec![Value::Int(-150)],
            },
        )
        .await;
        assert_eq!(
            tb.cmd_rx.recv().await,
            Some(EngineCommand::SetLinkTempoManual(MIN_VALID_BPM))
        );

        send_rpc(
            &mut tb.peer,
            &RpcMessage::Request {
                msgid: 44,
                method: CMD_SET_BPM.to_string(),
                params: vec![Value::F64(132.5)],
            },
        )
        .await;
        assert_eq!(
            tb.cmd_rx.recv().await,
            Some(EngineCommand::SetLinkTempoManual(132.5))
        );
        assert_eq!(
            read_rpc_frame(&mut tb.peer, &mut tb.read_buf).await,
            RpcMessage::Response {
                msgid: 44,
                error: Value::Nil,
                result: Value::Bool(true),
            }
        );

        // Out-of-bounds set_bpm request gets a conformant [253, msg] error response and emits no
        // EngineCommand, without poisoning debounce for a subsequent valid set_bpm.
        time::sleep(Duration::from_millis(90)).await;
        send_rpc(
            &mut tb.peer,
            &RpcMessage::Request {
                msgid: 45,
                method: CMD_SET_BPM.to_string(),
                params: vec![Value::F64(1000.0)],
            },
        )
        .await;
        match read_rpc_frame(&mut tb.peer, &mut tb.read_buf).await {
            RpcMessage::Response {
                msgid: 45,
                error,
                result: Value::Nil,
            } => {
                let rpc_err = RpcError::from_wire(error);
                assert_eq!(rpc_err.code, ErrorCode::MalformedCall);
                assert!(rpc_err.message.contains("range"));
            }
            other => panic!("expected error response, got {other:?}"),
        }
        assert!(tb.cmd_rx.try_recv().is_err());

        send_rpc(
            &mut tb.peer,
            &RpcMessage::Notification {
                method: CMD_SET_BPM.to_string(),
                params: vec![Value::Int(140)],
            },
        )
        .await;
        assert_eq!(
            tb.cmd_rx.recv().await,
            Some(EngineCommand::SetLinkTempoManual(140.0))
        );

        let _ = tb.shutdown_tx.send(true);
        tb.handle.await.expect("session join");
    }

    #[tokio::test]
    async fn closes_session_on_malformed_and_oversized_frames() {
        // Malformed reserved tag 0xc1 closes session cleanly.
        let mut tb = spawn_test_bridge(TelemetrySnapshot::default(), Duration::ZERO).await;
        let _ = consume_handshake(&mut tb).await;
        tb.peer.write_all(&[0xc1]).await.expect("write malformed");
        let mut tmp = [0u8; 8];
        let eof1 = time::timeout(Duration::from_secs(2), tb.peer.read(&mut tmp))
            .await
            .expect("peer sees EOF on malformed frame")
            .expect("read eof1");
        assert_eq!(eof1, 0);
        let _ = tb.shutdown_tx.send(true);
        time::timeout(Duration::from_secs(2), tb.handle)
            .await
            .expect("bridge exits on shutdown")
            .expect("session join");

        // Incomplete frame larger than MAX_FRAME_BYTES (16 KiB) closes session cleanly.
        let mut tb2 = spawn_test_bridge(TelemetrySnapshot::default(), Duration::ZERO).await;
        let _ = consume_handshake(&mut tb2).await;
        // 0xdb + len(20000) triggers immediate Oversized reject.
        let mut oversized_hdr = vec![0xdb];
        oversized_hdr.extend_from_slice(&20_000_u32.to_be_bytes());
        tb2.peer
            .write_all(&oversized_hdr)
            .await
            .expect("write oversized header");
        let eof2 = time::timeout(Duration::from_secs(2), tb2.peer.read(&mut tmp))
            .await
            .expect("peer sees EOF on oversized frame")
            .expect("read eof2");
        assert_eq!(eof2, 0);
        let _ = tb2.shutdown_tx.send(true);
        time::timeout(Duration::from_secs(2), tb2.handle)
            .await
            .expect("bridge exits on shutdown")
            .expect("session join");
    }

    #[tokio::test]
    async fn exits_cleanly_on_peer_eof_backoff_reconnect_and_spawn_enabled_flag() {
        // Verify backoff schedule progression: 250ms -> 500ms -> 1s -> 2s -> 4s -> 5s cap.
        let policy = ReconnectPolicy {
            initial_backoff: INITIAL_RECONNECT_BACKOFF,
            max_backoff: MAX_RECONNECT_BACKOFF,
            multiplier: 2,
        };
        let mut b = policy.initial_backoff;
        assert_eq!(b, Duration::from_millis(250));
        b = policy.next_backoff(b);
        assert_eq!(b, Duration::from_millis(500));
        b = policy.next_backoff(b);
        assert_eq!(b, Duration::from_secs(1));
        b = policy.next_backoff(b);
        assert_eq!(b, Duration::from_secs(2));
        b = policy.next_backoff(b);
        assert_eq!(b, Duration::from_secs(4));
        b = policy.next_backoff(b);
        assert_eq!(b, Duration::from_secs(5));
        assert_eq!(policy.next_backoff(b), Duration::from_secs(5));

        let (_state_tx, state_rx) = watch::channel(AppState::default());
        let (cmd_tx, _cmd_rx) = mpsc::channel(8);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let control = ControlHandle { state_rx, cmd_tx };

        // Disabled config returns None immediately.
        assert!(spawn_mcu_bridge(
            &McuBridgeConfig::default(),
            control.clone(),
            shutdown_rx.clone()
        )
        .is_none());

        // Enabled bridge spawns and shuts down promptly even if socket does not exist.
        let disabled_sock_cfg = McuBridgeConfig {
            enabled: true,
            socket_path: "/nonexistent/tactus-mcu-bridge-test.sock".to_string(),
        };
        let spawned = spawn_mcu_bridge(&disabled_sock_cfg, control.clone(), shutdown_rx.clone())
            .expect("spawn enabled bridge");
        let _ = shutdown_tx.send(true);
        time::timeout(Duration::from_secs(2), spawned)
            .await
            .expect("spawned bridge stops promptly on shutdown")
            .expect("join spawned bridge");

        // Reconnect loop handles initial connect failure, connects, survives peer EOF, reconnects, and shuts down.
        let (shutdown_tx2, shutdown_rx2) = watch::channel(false);
        let attempts = Arc::new(AtomicUsize::new(0));
        let (peer_tx, mut peer_rx) = mpsc::unbounded_channel();
        let attempts_c = Arc::clone(&attempts);
        let bridge_handle = tokio::spawn(run_mcu_bridge_with_connector(
            "test-pair",
            move || {
                let idx = attempts_c.fetch_add(1, Ordering::AcqRel);
                let peer_tx = peer_tx.clone();
                async move {
                    if idx == 0 {
                        Err(std::io::Error::new(
                            std::io::ErrorKind::NotFound,
                            "router not ready yet",
                        ))
                    } else {
                        let (server, peer) = UnixStream::pair()?;
                        let _ = peer_tx.send(peer);
                        Ok(server)
                    }
                }
            },
            control,
            shutdown_rx2,
            DEFAULT_COMMAND_DEBOUNCE,
        ));

        // First successful session after initial ENOENT backoff.
        let mut peer1 = time::timeout(Duration::from_secs(2), peer_rx.recv())
            .await
            .expect("timed out waiting for session 1")
            .expect("peer 1");
        let mut buf1 = Vec::new();
        let frame1 = read_rpc_frame(&mut peer1, &mut buf1).await;
        assert!(matches!(
            frame1,
            RpcMessage::Request {
                msgid: 1,
                ref method,
                ..
            } if method == METHOD_REGISTER
        ));

        // Drop session 1 -> bridge resets backoff and reconnects as session 2.
        drop(peer1);
        let mut peer2 = time::timeout(Duration::from_secs(2), peer_rx.recv())
            .await
            .expect("timed out waiting for session 2")
            .expect("peer 2");
        let mut buf2 = Vec::new();
        let frame2 = read_rpc_frame(&mut peer2, &mut buf2).await;
        assert!(matches!(
            frame2,
            RpcMessage::Request {
                msgid: 1,
                ref method,
                ..
            } if method == METHOD_REGISTER
        ));

        let _ = shutdown_tx2.send(true);
        time::timeout(Duration::from_secs(2), bridge_handle)
            .await
            .expect("timed out waiting for bridge loop shutdown")
            .expect("join bridge loop");
        assert!(attempts.load(Ordering::Acquire) >= 3);
    }
}
