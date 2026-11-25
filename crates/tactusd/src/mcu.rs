//! Optional `Arduino_RouterBridge` MessagePack-RPC Unix-socket client (`MILESTONES.md` M5).
//!
//! Enforces `AGENTS.md` Invariant #6: ZERO 24 PPQN MIDI Clock traffic crosses `/var/run/arduino-router.sock`.
//! Emits `tactus/state` only on connect and discrete state/`>= 0.1 BPM` changes, emits `tactus/downbeat`
//! on bar downbeat wraps while playing, emits `tactus/beat` on non-downbeat integer beat crossings
//! while playing, and forwards debounced MCU input commands to `ControlHandle`.

use std::time::Duration;

use tactus_protocol::{Direction, EngineCommand, LockState, McuBridgeConfig, TelemetrySnapshot};
use tactus_sync_core::{MAX_VALID_BPM, MIN_VALID_BPM};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::watch;
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

const METHOD_REGISTER: &str = "$/register";
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

// --- Session & Reconnect Loop ---

async fn run_mcu_bridge_loop(
    socket_path: String,
    handle: ControlHandle,
    shutdown_rx: watch::Receiver<bool>,
) {
    run_mcu_bridge_with_connector(
        &socket_path,
        || UnixStream::connect(&socket_path),
        handle,
        shutdown_rx,
    )
    .await;
}

async fn run_mcu_bridge_with_connector<F, Fut>(
    socket_label: &str,
    mut connect: F,
    handle: ControlHandle,
    mut shutdown_rx: watch::Receiver<bool>,
) where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = std::io::Result<UnixStream>>,
{
    let mut backoff = INITIAL_RECONNECT_BACKOFF;
    let mut connect_err_logged = false;

    loop {
        if *shutdown_rx.borrow_and_update() {
            break;
        }

        let connect_res = tokio::select! {
            biased;
            () = wait_shutdown(&mut shutdown_rx) => break,
            res = connect() => res,
        };

        match connect_res {
            Ok(stream) => {
                connect_err_logged = false;
                log_info!("MCU bridge connected to `{socket_label}`");
                let outcome = run_bridge_session_inner(
                    stream,
                    handle.clone(),
                    shutdown_rx.clone(),
                    DEFAULT_COMMAND_DEBOUNCE,
                )
                .await;
                match outcome {
                    SessionOutcome::Shutdown => break,
                    SessionOutcome::Disconnected { established } => {
                        if established {
                            backoff = INITIAL_RECONNECT_BACKOFF;
                        }
                    }
                }
            }
            Err(err) => {
                if !connect_err_logged {
                    log_warn!(
                        "MCU bridge failed connecting to `{socket_label}` ({err}); retrying with backoff"
                    );
                    connect_err_logged = true;
                }
            }
        }

        tokio::select! {
            biased;
            () = wait_shutdown(&mut shutdown_rx) => break,
            () = time::sleep(backoff) => {}
        }
        backoff = next_backoff(backoff);
    }

    log_info!("MCU bridge stopped");
}

const fn next_backoff(current: Duration) -> Duration {
    let doubled = current.saturating_mul(2);
    if doubled.as_millis() > MAX_RECONNECT_BACKOFF.as_millis() {
        MAX_RECONNECT_BACKOFF
    } else {
        doubled
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionOutcome {
    Shutdown,
    Disconnected { established: bool },
}

/// Run a single connected MessagePack-RPC bridge session until disconnect or shutdown.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) async fn run_bridge_session(
    stream: UnixStream,
    handle: ControlHandle,
    shutdown_rx: watch::Receiver<bool>,
    debounce: Duration,
) {
    let _ = run_bridge_session_inner(stream, handle, shutdown_rx, debounce).await;
}

async fn run_bridge_session_inner(
    mut stream: UnixStream,
    handle: ControlHandle,
    mut shutdown_rx: watch::Receiver<bool>,
    debounce: Duration,
) -> SessionOutcome {
    if *shutdown_rx.borrow_and_update() {
        return SessionOutcome::Shutdown;
    }

    let mut state_rx = handle.state_rx.clone();
    let initial_snap = state_rx.borrow_and_update().telemetry;
    let mut tracker = TelemetryTracker::new(initial_snap);
    let mut debouncer = CommandDebouncer::new(debounce);

    let mut out_buf = Vec::with_capacity(512);
    for (idx, method) in INCOMING_CMD_METHODS.iter().enumerate() {
        let req = RpcMessage::Request {
            msgid: (idx as u32) + 1,
            method: METHOD_REGISTER.to_string(),
            params: vec![MsgPackValue::Str((*method).to_string())],
        };
        if let Err(err) = encode_rpc_message(&req, &mut out_buf) {
            log_warn!("MCU bridge failed encoding register request for `{method}`: {err}");
            return SessionOutcome::Disconnected { established: false };
        }
    }
    append_telemetry_notification(METHOD_STATE, &initial_snap, &mut out_buf);

    match write_all_or_shutdown(&mut stream, &out_buf, &mut shutdown_rx).await {
        WriteStatus::Ok => {}
        WriteStatus::Shutdown => return SessionOutcome::Shutdown,
        WriteStatus::Failed(err) => {
            log_warn!("MCU bridge initial handshake write failed: {err}");
            return SessionOutcome::Disconnected { established: false };
        }
    }

    let mut read_buf = Vec::with_capacity(1024);
    let mut tmp = [0u8; 4096];

    loop {
        let avail = MAX_FRAME_BYTES.saturating_sub(read_buf.len());
        if avail == 0 {
            log_warn!("MCU bridge frame exceeded {MAX_FRAME_BYTES} byte limit; closing session");
            return SessionOutcome::Disconnected { established: true };
        }
        let read_cap = avail.min(tmp.len());

        tokio::select! {
            biased;
            () = wait_shutdown(&mut shutdown_rx) => {
                return SessionOutcome::Shutdown;
            }
            changed = state_rx.changed() => {
                if changed.is_err() {
                    return SessionOutcome::Disconnected { established: true };
                }
                let snap = state_rx.borrow_and_update().telemetry;
                let emit = tracker.evaluate(snap);
                if emit.state || emit.beat.is_some() {
                    out_buf.clear();
                    if emit.state {
                        append_telemetry_notification(METHOD_STATE, &snap, &mut out_buf);
                    }
                    match emit.beat {
                        Some(BeatEvent::Downbeat) => {
                            append_telemetry_notification(METHOD_DOWNBEAT, &snap, &mut out_buf);
                        }
                        Some(BeatEvent::Beat(beat_idx)) => {
                            append_beat_notification(beat_idx, &snap, &mut out_buf);
                        }
                        None => {}
                    }
                    match write_all_or_shutdown(&mut stream, &out_buf, &mut shutdown_rx).await {
                        WriteStatus::Ok => {}
                        WriteStatus::Shutdown => return SessionOutcome::Shutdown,
                        WriteStatus::Failed(err) => {
                            log_warn!("MCU bridge telemetry write failed: {err}");
                            return SessionOutcome::Disconnected { established: true };
                        }
                    }
                }
            }
            read_res = stream.read(&mut tmp[..read_cap]) => {
                match read_res {
                    Ok(0) => {
                        log_info!("MCU bridge peer closed connection");
                        return SessionOutcome::Disconnected { established: true };
                    }
                    Ok(n) => {
                        read_buf.extend_from_slice(&tmp[..n]);
                        match process_read_buffer(
                            &mut read_buf,
                            &mut out_buf,
                            &mut stream,
                            &handle,
                            &mut shutdown_rx,
                            &mut debouncer,
                        ).await {
                            BufferAction::Continue => {}
                            BufferAction::Shutdown => return SessionOutcome::Shutdown,
                            BufferAction::Disconnect => {
                                return SessionOutcome::Disconnected { established: true };
                            }
                        }
                    }
                    Err(err) => {
                        log_warn!("MCU bridge socket read failed: {err}");
                        return SessionOutcome::Disconnected { established: true };
                    }
                }
            }
        }
    }
}

enum BufferAction {
    Continue,
    Shutdown,
    Disconnect,
}

async fn process_read_buffer(
    read_buf: &mut Vec<u8>,
    out_buf: &mut Vec<u8>,
    stream: &mut UnixStream,
    handle: &ControlHandle,
    shutdown_rx: &mut watch::Receiver<bool>,
    debouncer: &mut CommandDebouncer,
) -> BufferAction {
    loop {
        match decode_rpc_frame(read_buf) {
            Ok((msg, consumed)) => {
                read_buf.drain(..consumed);
                match handle_rpc_message(msg, out_buf, stream, handle, shutdown_rx, debouncer).await
                {
                    BufferAction::Continue => {}
                    other => return other,
                }
            }
            Err(DecodeError::Incomplete) => {
                if read_buf.len() >= MAX_FRAME_BYTES {
                    log_warn!(
                        "MCU bridge incomplete frame exceeded {MAX_FRAME_BYTES} byte limit; closing session"
                    );
                    return BufferAction::Disconnect;
                }
                return BufferAction::Continue;
            }
            Err(DecodeError::Oversized) => {
                log_warn!("MCU bridge received oversized frame; closing session");
                return BufferAction::Disconnect;
            }
            Err(DecodeError::Malformed(reason)) => {
                log_warn!("MCU bridge received malformed RPC frame ({reason}); closing session");
                return BufferAction::Disconnect;
            }
        }
    }
}

async fn handle_rpc_message(
    msg: RpcMessage,
    out_buf: &mut Vec<u8>,
    stream: &mut UnixStream,
    handle: &ControlHandle,
    shutdown_rx: &mut watch::Receiver<bool>,
    debouncer: &mut CommandDebouncer,
) -> BufferAction {
    match msg {
        RpcMessage::Response { msgid, error, .. } => {
            if !matches!(error, MsgPackValue::Nil) {
                log_warn!("MCU bridge RPC response error (msgid={msgid}): {error:?}");
            }
            BufferAction::Continue
        }
        RpcMessage::Notification { method, params } => {
            let snap = handle.state_rx.borrow().telemetry;
            match translate_mcu_command(&method, &params, &snap) {
                Ok((kind, cmd)) => {
                    if debouncer.allow(kind, Instant::now())
                        && !forward_engine_command(handle, cmd).await
                    {
                        return BufferAction::Disconnect;
                    }
                }
                Err(reason) => {
                    log_warn!("ignoring MCU notification `{method}`: {reason}");
                }
            }
            BufferAction::Continue
        }
        RpcMessage::Request {
            msgid,
            method,
            params,
        } => {
            let snap = handle.state_rx.borrow().telemetry;
            let resp = match translate_mcu_command(&method, &params, &snap) {
                Ok((kind, cmd)) => {
                    if debouncer.allow(kind, Instant::now())
                        && !forward_engine_command(handle, cmd).await
                    {
                        return BufferAction::Disconnect;
                    }
                    RpcMessage::Response {
                        msgid,
                        error: MsgPackValue::Nil,
                        result: MsgPackValue::Bool(true),
                    }
                }
                Err(reason) => {
                    log_warn!("rejecting MCU RPC request `{method}` (msgid={msgid}): {reason}");
                    RpcMessage::Response {
                        msgid,
                        error: MsgPackValue::Str(reason.to_string()),
                        result: MsgPackValue::Nil,
                    }
                }
            };

            out_buf.clear();
            if let Err(err) = encode_rpc_message(&resp, out_buf) {
                log_warn!("MCU bridge failed encoding RPC response: {err}");
                return BufferAction::Disconnect;
            }
            match write_all_or_shutdown(stream, out_buf, shutdown_rx).await {
                WriteStatus::Ok => BufferAction::Continue,
                WriteStatus::Shutdown => BufferAction::Shutdown,
                WriteStatus::Failed(err) => {
                    log_warn!("MCU bridge response write failed: {err}");
                    BufferAction::Disconnect
                }
            }
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

enum WriteStatus {
    Ok,
    Shutdown,
    Failed(std::io::Error),
}

async fn write_all_or_shutdown(
    stream: &mut UnixStream,
    bytes: &[u8],
    shutdown_rx: &mut watch::Receiver<bool>,
) -> WriteStatus {
    if *shutdown_rx.borrow_and_update() {
        return WriteStatus::Shutdown;
    }

    tokio::select! {
        biased;
        () = wait_shutdown(shutdown_rx) => WriteStatus::Shutdown,
        res = time::timeout(SOCKET_WRITE_TIMEOUT, stream.write_all(bytes)) => match res {
            Ok(Ok(())) => WriteStatus::Ok,
            Ok(Err(err)) => WriteStatus::Failed(err),
            Err(_) => WriteStatus::Failed(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "socket write timed out",
            )),
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

fn append_telemetry_notification(method: &str, snap: &TelemetrySnapshot, out: &mut Vec<u8>) {
    append_telemetry_notification_with_beat(method, None, snap, out);
}

fn append_beat_notification(beat_idx: u32, snap: &TelemetrySnapshot, out: &mut Vec<u8>) {
    append_telemetry_notification_with_beat(METHOD_BEAT, Some(beat_idx), snap, out);
}

fn append_telemetry_notification_with_beat(
    method: &str,
    beat_idx: Option<u32>,
    snap: &TelemetrySnapshot,
    out: &mut Vec<u8>,
) {
    let bpm_tenths = (snap.bpm_link * 10.0).round() as i32;
    let mut params = Vec::with_capacity(5);
    if let Some(idx) = beat_idx {
        params.push(MsgPackValue::Int(i64::from(idx)));
    }
    params.push(MsgPackValue::Int(i64::from(bpm_tenths)));
    params.push(MsgPackValue::Int(i64::from(direction_to_i32(
        snap.direction,
    ))));
    params.push(MsgPackValue::Int(i64::from(lock_state_to_i32(
        snap.lock_state,
    ))));
    params.push(MsgPackValue::Bool(snap.is_playing));
    let notification = RpcMessage::Notification {
        method: method.to_string(),
        params,
    };
    let _ = encode_rpc_message(&notification, out);
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
    params: &[MsgPackValue],
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
                MsgPackValue::Int(0) | MsgPackValue::Uint(0) => Direction::LinkToMidi,
                MsgPackValue::Int(1) | MsgPackValue::Uint(1) => Direction::MidiToLink,
                MsgPackValue::Str(s) if s == "link_to_midi" => Direction::LinkToMidi,
                MsgPackValue::Str(s) if s == "midi_to_link" => Direction::MidiToLink,
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
            let MsgPackValue::Bool(playing) = params[0] else {
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

// --- MessagePack-RPC Codec ---

#[derive(Debug, Clone, PartialEq)]
enum MsgPackValue {
    Nil,
    Bool(bool),
    Int(i64),
    Uint(u64),
    Float(f64),
    Str(String),
    Bin(Vec<u8>),
    Array(Vec<MsgPackValue>),
    Map(Vec<(MsgPackValue, MsgPackValue)>),
    Ext { kind: i8, data: Vec<u8> },
}

impl MsgPackValue {
    fn as_i64(&self) -> Option<i64> {
        match *self {
            Self::Int(i) => Some(i),
            Self::Uint(u) => i64::try_from(u).ok(),
            _ => None,
        }
    }

    fn as_u32(&self) -> Option<u32> {
        match *self {
            Self::Int(i) => u32::try_from(i).ok(),
            Self::Uint(u) => u32::try_from(u).ok(),
            _ => None,
        }
    }

    fn as_f64(&self) -> Option<f64> {
        match *self {
            Self::Float(f) => Some(f),
            Self::Int(i) => Some(i as f64),
            Self::Uint(u) => Some(u as f64),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum RpcMessage {
    Request {
        msgid: u32,
        method: String,
        params: Vec<MsgPackValue>,
    },
    Response {
        msgid: u32,
        error: MsgPackValue,
        result: MsgPackValue,
    },
    Notification {
        method: String,
        params: Vec<MsgPackValue>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DecodeError {
    Incomplete,
    Oversized,
    Malformed(&'static str),
}

struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    const fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn take_u8(&mut self) -> Result<u8, DecodeError> {
        if self.pos >= self.buf.len() {
            return Err(DecodeError::Incomplete);
        }
        let b = self.buf[self.pos];
        self.pos += 1;
        Ok(b)
    }

    fn take_bytes(&mut self, len: usize) -> Result<&'a [u8], DecodeError> {
        let end = self.pos.checked_add(len).ok_or(DecodeError::Oversized)?;
        if end > self.buf.len() {
            return Err(DecodeError::Incomplete);
        }
        let slice = &self.buf[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    fn take_u16_be(&mut self) -> Result<u16, DecodeError> {
        let bytes = self.take_bytes(2)?;
        Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    fn take_u32_be(&mut self) -> Result<u32, DecodeError> {
        let bytes = self.take_bytes(4)?;
        Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn take_u64_be(&mut self) -> Result<u64, DecodeError> {
        let bytes = self.take_bytes(8)?;
        Ok(u64::from_be_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]))
    }
}

fn decode_one(buf: &[u8]) -> Result<(MsgPackValue, usize), DecodeError> {
    let mut cursor = Cursor::new(buf);
    let value = decode_value(&mut cursor, 0)?;
    Ok((value, cursor.pos))
}

fn decode_value(cursor: &mut Cursor<'_>, depth: usize) -> Result<MsgPackValue, DecodeError> {
    if depth >= MAX_DECODE_DEPTH {
        return Err(DecodeError::Malformed("nesting depth exceeded"));
    }

    let tag = cursor.take_u8()?;
    match tag {
        0x00..=0x7f => Ok(MsgPackValue::Int(i64::from(tag))),
        0x80..=0x8f => decode_map(cursor, usize::from(tag & 0x0f), depth),
        0x90..=0x9f => decode_array(cursor, usize::from(tag & 0x0f), depth),
        0xa0..=0xbf => decode_str(cursor, usize::from(tag & 0x1f)),
        0xc0 => Ok(MsgPackValue::Nil),
        0xc1 => Err(DecodeError::Malformed("reserved tag 0xc1")),
        0xc2 => Ok(MsgPackValue::Bool(false)),
        0xc3 => Ok(MsgPackValue::Bool(true)),
        0xc4 => {
            let len = usize::from(cursor.take_u8()?);
            decode_bin(cursor, len)
        }
        0xc5 => {
            let len = usize::from(cursor.take_u16_be()?);
            decode_bin(cursor, len)
        }
        0xc6 => {
            let len = cursor.take_u32_be()? as usize;
            decode_bin(cursor, len)
        }
        0xc7 => {
            let len = usize::from(cursor.take_u8()?);
            decode_ext(cursor, len)
        }
        0xc8 => {
            let len = usize::from(cursor.take_u16_be()?);
            decode_ext(cursor, len)
        }
        0xc9 => {
            let len = cursor.take_u32_be()? as usize;
            decode_ext(cursor, len)
        }
        0xca => {
            let bytes = cursor.take_bytes(4)?;
            let bits = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
            Ok(MsgPackValue::Float(f64::from(f32::from_bits(bits))))
        }
        0xcb => {
            let bits = cursor.take_u64_be()?;
            Ok(MsgPackValue::Float(f64::from_bits(bits)))
        }
        0xcc => Ok(MsgPackValue::Int(i64::from(cursor.take_u8()?))),
        0xcd => Ok(MsgPackValue::Int(i64::from(cursor.take_u16_be()?))),
        0xce => Ok(MsgPackValue::Int(i64::from(cursor.take_u32_be()?))),
        0xcf => {
            let val = cursor.take_u64_be()?;
            Ok(match i64::try_from(val) {
                Ok(i) => MsgPackValue::Int(i),
                Err(_) => MsgPackValue::Uint(val),
            })
        }
        0xd0 => Ok(MsgPackValue::Int(i64::from(cursor.take_u8()? as i8))),
        0xd1 => {
            let bytes = cursor.take_bytes(2)?;
            Ok(MsgPackValue::Int(i64::from(i16::from_be_bytes([
                bytes[0], bytes[1],
            ]))))
        }
        0xd2 => {
            let bytes = cursor.take_bytes(4)?;
            Ok(MsgPackValue::Int(i64::from(i32::from_be_bytes([
                bytes[0], bytes[1], bytes[2], bytes[3],
            ]))))
        }
        0xd3 => {
            let bytes = cursor.take_bytes(8)?;
            Ok(MsgPackValue::Int(i64::from_be_bytes([
                bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
            ])))
        }
        0xd4 => decode_ext(cursor, 1),
        0xd5 => decode_ext(cursor, 2),
        0xd6 => decode_ext(cursor, 4),
        0xd7 => decode_ext(cursor, 8),
        0xd8 => decode_ext(cursor, 16),
        0xd9 => {
            let len = usize::from(cursor.take_u8()?);
            decode_str(cursor, len)
        }
        0xda => {
            let len = usize::from(cursor.take_u16_be()?);
            decode_str(cursor, len)
        }
        0xdb => {
            let len = cursor.take_u32_be()? as usize;
            decode_str(cursor, len)
        }
        0xdc => {
            let count = usize::from(cursor.take_u16_be()?);
            decode_array(cursor, count, depth)
        }
        0xdd => {
            let count = cursor.take_u32_be()? as usize;
            decode_array(cursor, count, depth)
        }
        0xde => {
            let count = usize::from(cursor.take_u16_be()?);
            decode_map(cursor, count, depth)
        }
        0xdf => {
            let count = cursor.take_u32_be()? as usize;
            decode_map(cursor, count, depth)
        }
        0xe0..=0xff => Ok(MsgPackValue::Int(i64::from(tag as i8))),
    }
}

fn decode_str(cursor: &mut Cursor<'_>, len: usize) -> Result<MsgPackValue, DecodeError> {
    if len > MAX_FRAME_BYTES {
        return Err(DecodeError::Oversized);
    }
    let bytes = cursor.take_bytes(len)?;
    let s = std::str::from_utf8(bytes)
        .map_err(|_| DecodeError::Malformed("invalid UTF-8 string"))?
        .to_string();
    Ok(MsgPackValue::Str(s))
}

fn decode_bin(cursor: &mut Cursor<'_>, len: usize) -> Result<MsgPackValue, DecodeError> {
    if len > MAX_FRAME_BYTES {
        return Err(DecodeError::Oversized);
    }
    let bytes = cursor.take_bytes(len)?;
    Ok(MsgPackValue::Bin(bytes.to_vec()))
}

fn decode_ext(cursor: &mut Cursor<'_>, len: usize) -> Result<MsgPackValue, DecodeError> {
    if len > MAX_FRAME_BYTES {
        return Err(DecodeError::Oversized);
    }
    let kind = cursor.take_u8()? as i8;
    let data = cursor.take_bytes(len)?;
    Ok(MsgPackValue::Ext {
        kind,
        data: data.to_vec(),
    })
}

fn decode_array(
    cursor: &mut Cursor<'_>,
    count: usize,
    depth: usize,
) -> Result<MsgPackValue, DecodeError> {
    if count > MAX_FRAME_BYTES {
        return Err(DecodeError::Oversized);
    }
    let mut items = Vec::with_capacity(count.min(64));
    for _ in 0..count {
        items.push(decode_value(cursor, depth + 1)?);
    }
    Ok(MsgPackValue::Array(items))
}

fn decode_map(
    cursor: &mut Cursor<'_>,
    count: usize,
    depth: usize,
) -> Result<MsgPackValue, DecodeError> {
    if count > MAX_FRAME_BYTES / 2 {
        return Err(DecodeError::Oversized);
    }
    let mut entries = Vec::with_capacity(count.min(64));
    for _ in 0..count {
        let key = decode_value(cursor, depth + 1)?;
        let val = decode_value(cursor, depth + 1)?;
        entries.push((key, val));
    }
    Ok(MsgPackValue::Map(entries))
}

fn decode_rpc_frame(buf: &[u8]) -> Result<(RpcMessage, usize), DecodeError> {
    let (value, consumed) = decode_one(buf)?;
    let MsgPackValue::Array(items) = value else {
        return Err(DecodeError::Malformed("RPC frame must be an array"));
    };
    let mut iter = items.into_iter();
    let Some(kind_val) = iter.next() else {
        return Err(DecodeError::Malformed("empty RPC frame"));
    };

    match kind_val.as_i64() {
        Some(0) => {
            let (Some(msgid_val), Some(method_val), Some(params_val), None) =
                (iter.next(), iter.next(), iter.next(), iter.next())
            else {
                return Err(DecodeError::Malformed("invalid RPC request arity"));
            };
            let msgid = msgid_val
                .as_u32()
                .ok_or(DecodeError::Malformed("invalid RPC request msgid"))?;
            let MsgPackValue::Str(method) = method_val else {
                return Err(DecodeError::Malformed("RPC request method must be string"));
            };
            let MsgPackValue::Array(params) = params_val else {
                return Err(DecodeError::Malformed("RPC request params must be array"));
            };
            Ok((
                RpcMessage::Request {
                    msgid,
                    method,
                    params,
                },
                consumed,
            ))
        }
        Some(1) => {
            let (Some(msgid_val), Some(error), Some(result), None) =
                (iter.next(), iter.next(), iter.next(), iter.next())
            else {
                return Err(DecodeError::Malformed("invalid RPC response arity"));
            };
            let msgid = msgid_val
                .as_u32()
                .ok_or(DecodeError::Malformed("invalid RPC response msgid"))?;
            Ok((
                RpcMessage::Response {
                    msgid,
                    error,
                    result,
                },
                consumed,
            ))
        }
        Some(2) => {
            let (Some(method_val), Some(params_val), None) =
                (iter.next(), iter.next(), iter.next())
            else {
                return Err(DecodeError::Malformed("invalid RPC notification arity"));
            };
            let MsgPackValue::Str(method) = method_val else {
                return Err(DecodeError::Malformed(
                    "RPC notification method must be string",
                ));
            };
            let MsgPackValue::Array(params) = params_val else {
                return Err(DecodeError::Malformed(
                    "RPC notification params must be array",
                ));
            };
            Ok((RpcMessage::Notification { method, params }, consumed))
        }
        _ => Err(DecodeError::Malformed("unknown RPC message type")),
    }
}

fn encode_rpc_message(msg: &RpcMessage, out: &mut Vec<u8>) -> Result<(), &'static str> {
    match msg {
        RpcMessage::Request {
            msgid,
            method,
            params,
        } => {
            out.push(0x94);
            out.push(0x00);
            encode_uint(u64::from(*msgid), out);
            encode_str(method, out)?;
            encode_array_header(params.len(), out)?;
            for item in params {
                encode_value(item, out)?;
            }
            Ok(())
        }
        RpcMessage::Response {
            msgid,
            error,
            result,
        } => {
            out.push(0x94);
            out.push(0x01);
            encode_uint(u64::from(*msgid), out);
            encode_value(error, out)?;
            encode_value(result, out)?;
            Ok(())
        }
        RpcMessage::Notification { method, params } => {
            out.push(0x93);
            out.push(0x02);
            encode_str(method, out)?;
            encode_array_header(params.len(), out)?;
            for item in params {
                encode_value(item, out)?;
            }
            Ok(())
        }
    }
}

fn encode_value(val: &MsgPackValue, out: &mut Vec<u8>) -> Result<(), &'static str> {
    match val {
        MsgPackValue::Nil => out.push(0xc0),
        MsgPackValue::Bool(b) => out.push(if *b { 0xc3 } else { 0xc2 }),
        MsgPackValue::Int(i) => encode_int(*i, out),
        MsgPackValue::Uint(u) => encode_uint(*u, out),
        MsgPackValue::Float(f) => {
            out.push(0xcb);
            out.extend_from_slice(&f.to_be_bytes());
        }
        MsgPackValue::Str(s) => encode_str(s, out)?,
        MsgPackValue::Bin(b) => encode_bin(b, out)?,
        MsgPackValue::Array(items) => {
            encode_array_header(items.len(), out)?;
            for item in items {
                encode_value(item, out)?;
            }
        }
        MsgPackValue::Map(entries) => {
            encode_map_header(entries.len(), out)?;
            for (k, v) in entries {
                encode_value(k, out)?;
                encode_value(v, out)?;
            }
        }
        MsgPackValue::Ext { kind, data } => encode_ext(*kind, data, out)?,
    }
    Ok(())
}

fn encode_uint(v: u64, out: &mut Vec<u8>) {
    if v <= 0x7f {
        out.push(v as u8);
    } else if v <= u64::from(u8::MAX) {
        out.push(0xcc);
        out.push(v as u8);
    } else if v <= u64::from(u16::MAX) {
        out.push(0xcd);
        out.extend_from_slice(&(v as u16).to_be_bytes());
    } else if v <= u64::from(u32::MAX) {
        out.push(0xce);
        out.extend_from_slice(&(v as u32).to_be_bytes());
    } else {
        out.push(0xcf);
        out.extend_from_slice(&v.to_be_bytes());
    }
}

fn encode_int(v: i64, out: &mut Vec<u8>) {
    if v >= 0 {
        encode_uint(v as u64, out);
    } else if v >= -32 {
        out.push((v as i8) as u8);
    } else if v >= i64::from(i8::MIN) {
        out.push(0xd0);
        out.push((v as i8) as u8);
    } else if v >= i64::from(i16::MIN) {
        out.push(0xd1);
        out.extend_from_slice(&(v as i16).to_be_bytes());
    } else if v >= i64::from(i32::MIN) {
        out.push(0xd2);
        out.extend_from_slice(&(v as i32).to_be_bytes());
    } else {
        out.push(0xd3);
        out.extend_from_slice(&v.to_be_bytes());
    }
}

fn encode_str(s: &str, out: &mut Vec<u8>) -> Result<(), &'static str> {
    let len = s.len();
    if len <= 31 {
        out.push(0xa0 | (len as u8));
    } else if let Ok(l) = u8::try_from(len) {
        out.push(0xd9);
        out.push(l);
    } else if let Ok(l) = u16::try_from(len) {
        out.push(0xda);
        out.extend_from_slice(&l.to_be_bytes());
    } else if let Ok(l) = u32::try_from(len) {
        out.push(0xdb);
        out.extend_from_slice(&l.to_be_bytes());
    } else {
        return Err("string exceeds MessagePack max length");
    }
    out.extend_from_slice(s.as_bytes());
    Ok(())
}

fn encode_bin(bytes: &[u8], out: &mut Vec<u8>) -> Result<(), &'static str> {
    let len = bytes.len();
    if let Ok(l) = u8::try_from(len) {
        out.push(0xc4);
        out.push(l);
    } else if let Ok(l) = u16::try_from(len) {
        out.push(0xc5);
        out.extend_from_slice(&l.to_be_bytes());
    } else if let Ok(l) = u32::try_from(len) {
        out.push(0xc6);
        out.extend_from_slice(&l.to_be_bytes());
    } else {
        return Err("binary exceeds MessagePack max length");
    }
    out.extend_from_slice(bytes);
    Ok(())
}

fn encode_array_header(len: usize, out: &mut Vec<u8>) -> Result<(), &'static str> {
    if len <= 15 {
        out.push(0x90 | (len as u8));
    } else if let Ok(l) = u16::try_from(len) {
        out.push(0xdc);
        out.extend_from_slice(&l.to_be_bytes());
    } else if let Ok(l) = u32::try_from(len) {
        out.push(0xdd);
        out.extend_from_slice(&l.to_be_bytes());
    } else {
        return Err("array exceeds MessagePack max length");
    }
    Ok(())
}

fn encode_map_header(len: usize, out: &mut Vec<u8>) -> Result<(), &'static str> {
    if len <= 15 {
        out.push(0x80 | (len as u8));
    } else if let Ok(l) = u16::try_from(len) {
        out.push(0xde);
        out.extend_from_slice(&l.to_be_bytes());
    } else if let Ok(l) = u32::try_from(len) {
        out.push(0xdf);
        out.extend_from_slice(&l.to_be_bytes());
    } else {
        return Err("map exceeds MessagePack max length");
    }
    Ok(())
}

fn encode_ext(kind: i8, data: &[u8], out: &mut Vec<u8>) -> Result<(), &'static str> {
    let len = data.len();
    match len {
        1 => out.push(0xd4),
        2 => out.push(0xd5),
        4 => out.push(0xd6),
        8 => out.push(0xd7),
        16 => out.push(0xd8),
        _ => {
            if let Ok(l) = u8::try_from(len) {
                out.push(0xc7);
                out.push(l);
            } else if let Ok(l) = u16::try_from(len) {
                out.push(0xc8);
                out.extend_from_slice(&l.to_be_bytes());
            } else if let Ok(l) = u32::try_from(len) {
                out.push(0xc9);
                out.extend_from_slice(&l.to_be_bytes());
            } else {
                return Err("ext exceeds MessagePack max length");
            }
        }
    }
    out.push(kind as u8);
    out.extend_from_slice(data);
    Ok(())
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use tactus_protocol::AppState;
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

        let handle = tokio::spawn(run_bridge_session(
            server,
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
        loop {
            match decode_rpc_frame(buf) {
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
        encode_rpc_message(msg, &mut encoded).expect("encode rpc message");
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
                    let MsgPackValue::Str(cmd_name) = &params[0] else {
                        panic!("expected string method parameter");
                    };
                    registered.push(cmd_name.clone());
                }
                other => panic!("expected $/register request, got {other:?}"),
            }
        }
        let initial_state = read_rpc_frame(&mut tb.peer, &mut tb.read_buf).await;
        (registered, initial_state)
    }

    #[test]
    fn msgpack_value_round_trips_all_primitives_and_partial_buffers() {
        let values = vec![
            MsgPackValue::Nil,
            MsgPackValue::Bool(false),
            MsgPackValue::Bool(true),
            MsgPackValue::Int(0),
            MsgPackValue::Int(127),
            MsgPackValue::Int(128),
            MsgPackValue::Int(255),
            MsgPackValue::Int(256),
            MsgPackValue::Int(65_535),
            MsgPackValue::Int(65_536),
            MsgPackValue::Int(i64::from(i32::MAX)),
            MsgPackValue::Int(i64::MAX),
            MsgPackValue::Uint(u64::MAX),
            MsgPackValue::Int(-1),
            MsgPackValue::Int(-32),
            MsgPackValue::Int(-33),
            MsgPackValue::Int(-128),
            MsgPackValue::Int(-129),
            MsgPackValue::Int(-32_768),
            MsgPackValue::Int(-32_769),
            MsgPackValue::Int(i64::from(i32::MIN)),
            MsgPackValue::Int(i64::MIN),
            MsgPackValue::Float(120.5),
            MsgPackValue::Float(-0.25),
            MsgPackValue::Str(String::new()),
            MsgPackValue::Str("tactus/state".to_string()),
            MsgPackValue::Str("x".repeat(40)),
            MsgPackValue::Str("y".repeat(300)),
            MsgPackValue::Bin(vec![0xF8, 0xFA, 0xFC]),
            MsgPackValue::Array(vec![
                MsgPackValue::Int(1200),
                MsgPackValue::Int(0),
                MsgPackValue::Int(2),
                MsgPackValue::Bool(true),
            ]),
            MsgPackValue::Map(vec![(
                MsgPackValue::Str("bpm".to_string()),
                MsgPackValue::Float(128.25),
            )]),
            MsgPackValue::Ext {
                kind: 7,
                data: vec![1, 2, 3, 4],
            },
        ];

        for val in values {
            let mut buf = Vec::new();
            encode_value(&val, &mut buf).expect("encode value");

            // Every strict prefix must report Incomplete.
            for prefix_len in 0..buf.len() {
                assert_eq!(
                    decode_one(&buf[..prefix_len]),
                    Err(DecodeError::Incomplete),
                    "prefix {prefix_len}/{} for {val:?}",
                    buf.len()
                );
            }

            // Extra trailing bytes stay unconsumed.
            let mut with_tail = buf.clone();
            with_tail.extend_from_slice(&[0x01, 0x02]);
            let (decoded, consumed) = decode_one(&with_tail).expect("decode full value");
            assert_eq!(consumed, buf.len());
            assert_eq!(decoded, val);
        }

        // float32 tag (0xca) decodes cleanly to Float(f64).
        let mut f32_bytes = vec![0xca];
        f32_bytes.extend_from_slice(&125.5_f32.to_be_bytes());
        assert_eq!(decode_one(&f32_bytes), Ok((MsgPackValue::Float(125.5), 5)));

        // Reserved tag 0xc1 and oversized declarations are rejected.
        assert!(matches!(
            decode_one(&[0xc1]),
            Err(DecodeError::Malformed(_))
        ));
        let mut huge_str = vec![0xdb];
        huge_str.extend_from_slice(&((MAX_FRAME_BYTES as u32) + 1).to_be_bytes());
        assert_eq!(decode_one(&huge_str), Err(DecodeError::Oversized));

        let mut huge_arr = vec![0xdd];
        huge_arr.extend_from_slice(&((MAX_FRAME_BYTES as u32) + 1).to_be_bytes());
        assert_eq!(decode_one(&huge_arr), Err(DecodeError::Oversized));
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
        let expected: Vec<String> = INCOMING_CMD_METHODS
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        assert_eq!(registered, expected);
        assert_eq!(
            initial_state,
            RpcMessage::Notification {
                method: METHOD_STATE.to_string(),
                params: vec![
                    MsgPackValue::Int(1246),
                    MsgPackValue::Int(1),
                    MsgPackValue::Int(1),
                    MsgPackValue::Bool(true),
                ],
            }
        );

        // $/register responses are cleanly ignored.
        send_rpc(
            &mut tb.peer,
            &RpcMessage::Response {
                msgid: 1,
                error: MsgPackValue::Nil,
                result: MsgPackValue::Bool(true),
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
                        MsgPackValue::Int(i64::from(beat)),
                        MsgPackValue::Int(1200),
                        MsgPackValue::Int(0),
                        MsgPackValue::Int(2),
                        MsgPackValue::Bool(true),
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
                    MsgPackValue::Int(1200),
                    MsgPackValue::Int(0),
                    MsgPackValue::Int(2),
                    MsgPackValue::Bool(true),
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
                    MsgPackValue::Int(1201),
                    MsgPackValue::Int(0),
                    MsgPackValue::Int(0),
                    MsgPackValue::Bool(false),
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
                    MsgPackValue::Int(1201),
                    MsgPackValue::Int(0),
                    MsgPackValue::Int(0),
                    MsgPackValue::Bool(true),
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
                    MsgPackValue::Int(1201),
                    MsgPackValue::Int(1),
                    MsgPackValue::Int(0),
                    MsgPackValue::Bool(true),
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
                    MsgPackValue::Int(1201),
                    MsgPackValue::Int(1),
                    MsgPackValue::Int(2),
                    MsgPackValue::Bool(true),
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
                error: MsgPackValue::Nil,
                result: MsgPackValue::Bool(true),
            }
        );

        // Wait past debounce and send remaining command types (split across partial writes!).
        time::sleep(Duration::from_millis(90)).await;

        let mut partial = Vec::new();
        encode_rpc_message(
            &RpcMessage::Request {
                msgid: 43,
                method: CMD_SET_DIRECTION.to_string(),
                params: vec![MsgPackValue::Str("link_to_midi".to_string())],
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
                error: MsgPackValue::Nil,
                result: MsgPackValue::Bool(true),
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
                params: vec![MsgPackValue::Bool(false)],
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
                params: vec![MsgPackValue::Int(-150)],
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
                params: vec![MsgPackValue::Float(132.5)],
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
                error: MsgPackValue::Nil,
                result: MsgPackValue::Bool(true),
            }
        );

        // Out-of-bounds set_bpm request gets an error response and emits no EngineCommand,
        // and does not poison debounce for a subsequent valid set_bpm.
        time::sleep(Duration::from_millis(90)).await;
        send_rpc(
            &mut tb.peer,
            &RpcMessage::Request {
                msgid: 45,
                method: CMD_SET_BPM.to_string(),
                params: vec![MsgPackValue::Float(1000.0)],
            },
        )
        .await;
        match read_rpc_frame(&mut tb.peer, &mut tb.read_buf).await {
            RpcMessage::Response {
                msgid: 45,
                error: MsgPackValue::Str(msg),
                result: MsgPackValue::Nil,
            } => assert!(msg.contains("range")),
            other => panic!("expected error response, got {other:?}"),
        }
        assert!(tb.cmd_rx.try_recv().is_err());

        send_rpc(
            &mut tb.peer,
            &RpcMessage::Notification {
                method: CMD_SET_BPM.to_string(),
                params: vec![MsgPackValue::Int(140)],
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
        time::timeout(Duration::from_secs(2), tb.handle)
            .await
            .expect("session exits on malformed frame")
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
        time::timeout(Duration::from_secs(2), tb2.handle)
            .await
            .expect("session exits on oversized frame")
            .expect("session join");
    }

    #[tokio::test]
    async fn exits_cleanly_on_peer_eof_backoff_reconnect_and_spawn_enabled_flag() {
        // Verify backoff schedule progression: 250ms -> 500ms -> 1s -> 2s -> 4s -> 5s cap.
        let mut b = INITIAL_RECONNECT_BACKOFF;
        assert_eq!(b, Duration::from_millis(250));
        b = next_backoff(b);
        assert_eq!(b, Duration::from_millis(500));
        b = next_backoff(b);
        assert_eq!(b, Duration::from_secs(1));
        b = next_backoff(b);
        assert_eq!(b, Duration::from_secs(2));
        b = next_backoff(b);
        assert_eq!(b, Duration::from_secs(4));
        b = next_backoff(b);
        assert_eq!(b, Duration::from_secs(5));
        assert_eq!(next_backoff(b), Duration::from_secs(5));

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

        // Peer EOF stops run_bridge_session cleanly.
        let tb = spawn_test_bridge(TelemetrySnapshot::default(), Duration::ZERO).await;
        drop(tb.peer);
        time::timeout(Duration::from_secs(2), tb.handle)
            .await
            .expect("session exits promptly on EOF")
            .expect("session join");

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
