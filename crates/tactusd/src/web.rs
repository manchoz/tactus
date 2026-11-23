//! Embedded mobile Web UI HTTP & WebSocket server (`axum` on `tokio`).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use tactus_protocol::EngineCommand;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use crate::control::ControlHandle;
use crate::log::{log_error, log_info, log_warn};

const MAX_WS_CLIENTS: usize = 16;
const MAX_WS_MSG_BYTES: usize = 4096;
const WS_SEND_TIMEOUT: Duration = Duration::from_secs(5);
const WS_CMD_TIMEOUT: Duration = Duration::from_secs(1);
const WS_PING_INTERVAL: Duration = Duration::from_secs(15);

const INDEX_HTML: &str = include_str!("../web/index.html");
const STYLE_CSS: &str = include_str!("../web/style.css");
const APP_JS: &str = include_str!("../web/app.js");
const MANIFEST_JSON: &str = include_str!("../web/manifest.webmanifest");
const ICON_SVG: &str = include_str!("../web/icon.svg");
const APPLE_TOUCH_ICON_PNG: &[u8] = include_bytes!("../web/apple-touch-icon.png");

#[derive(Clone)]
struct WebAppState {
    control: ControlHandle,
    shutdown_rx: watch::Receiver<bool>,
    active_ws_clients: Arc<AtomicUsize>,
}

struct WsClientGuard {
    counter: Arc<AtomicUsize>,
}

impl Drop for WsClientGuard {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::AcqRel);
    }
}

fn try_acquire_ws_slot(counter: &Arc<AtomicUsize>) -> Option<WsClientGuard> {
    let mut curr = counter.load(Ordering::Acquire);
    while curr < MAX_WS_CLIENTS {
        match counter.compare_exchange_weak(curr, curr + 1, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => {
                return Some(WsClientGuard {
                    counter: Arc::clone(counter),
                });
            }
            Err(actual) => curr = actual,
        }
    }
    None
}

fn static_asset(
    content_type: &'static str,
    cache_control: &'static str,
    body: &'static [u8],
) -> Response {
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, cache_control),
        ],
        body,
    )
        .into_response()
}

/// Construct the `axum` router serving the embedded mobile UI, REST state, and `/ws`.
pub fn build_router(control: ControlHandle, shutdown_rx: watch::Receiver<bool>) -> Router {
    let state = WebAppState {
        control,
        shutdown_rx,
        active_ws_clients: Arc::new(AtomicUsize::new(0)),
    };

    Router::new()
        .route(
            "/",
            get(|| async {
                static_asset(
                    "text/html; charset=utf-8",
                    "no-cache",
                    INDEX_HTML.as_bytes(),
                )
            }),
        )
        .route(
            "/style.css",
            get(|| async {
                static_asset("text/css; charset=utf-8", "no-cache", STYLE_CSS.as_bytes())
            }),
        )
        .route(
            "/app.js",
            get(|| async {
                static_asset(
                    "application/javascript; charset=utf-8",
                    "no-cache",
                    APP_JS.as_bytes(),
                )
            }),
        )
        .route(
            "/manifest.webmanifest",
            get(|| async {
                static_asset(
                    "application/manifest+json; charset=utf-8",
                    "no-cache",
                    MANIFEST_JSON.as_bytes(),
                )
            }),
        )
        .route(
            "/icon.svg",
            get(|| async {
                static_asset(
                    "image/svg+xml",
                    "public, max-age=86400",
                    ICON_SVG.as_bytes(),
                )
            }),
        )
        .route(
            "/apple-touch-icon.png",
            get(|| async {
                static_asset("image/png", "public, max-age=86400", APPLE_TOUCH_ICON_PNG)
            }),
        )
        .route("/api/state", get(api_state))
        .route("/healthz", get(healthz))
        .route("/ws", get(ws_upgrade))
        .with_state(state)
}

/// Bind and spawn the embedded Web UI HTTP/WebSocket server on the `tokio` runtime.
///
/// Binding failure is logged and returns `None` so headless RT sync continues unaffected.
pub async fn spawn_web_server(
    listen_addr: &str,
    control: ControlHandle,
    shutdown_rx: watch::Receiver<bool>,
) -> Option<JoinHandle<()>> {
    let listener = match TcpListener::bind(listen_addr).await {
        Ok(l) => l,
        Err(err) => {
            log_error!("failed to bind web UI on {listen_addr}: {err}; continuing headless");
            return None;
        }
    };

    let bound_addr = listener
        .local_addr()
        .map_or_else(|_| listen_addr.to_string(), |a| a.to_string());
    log_info!("web UI listening on http://{bound_addr}");

    let router = build_router(control, shutdown_rx.clone());
    let mut graceful_rx = shutdown_rx;
    let handle = tokio::spawn(async move {
        let server = axum::serve(listener, router).with_graceful_shutdown(async move {
            wait_shutdown(&mut graceful_rx).await;
        });
        if let Err(err) = server.await {
            log_error!("web UI server stopped with error: {err}");
        }
    });

    Some(handle)
}

async fn api_state(State(state): State<WebAppState>) -> Response {
    let snapshot = state.control.state_rx.borrow().clone();
    ([(header::CACHE_CONTROL, "no-store")], Json(snapshot)).into_response()
}

async fn healthz() -> Response {
    (
        [(header::CACHE_CONTROL, "no-store")],
        Json(serde_json::json!({
            "status": "ok",
            "version": env!("CARGO_PKG_VERSION"),
        })),
    )
        .into_response()
}

async fn ws_upgrade(ws: WebSocketUpgrade, State(state): State<WebAppState>) -> Response {
    let Some(guard) = try_acquire_ws_slot(&state.active_ws_clients) else {
        log_warn!("rejecting /ws upgrade: client limit ({MAX_WS_CLIENTS}) reached");
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "too many WebSocket clients",
        )
            .into_response();
    };

    ws.max_message_size(MAX_WS_MSG_BYTES)
        .max_frame_size(MAX_WS_MSG_BYTES)
        .on_upgrade(move |socket| handle_ws_socket(socket, state, guard))
}

async fn handle_ws_socket(mut socket: WebSocket, mut state: WebAppState, _guard: WsClientGuard) {
    let mut state_rx = state.control.state_rx.clone();

    let initial_json = {
        let initial = state_rx.borrow_and_update().clone();
        match serde_json::to_string(&initial) {
            Ok(json) => json,
            Err(err) => {
                log_error!("failed to serialize initial WebSocket state: {err}");
                return;
            }
        }
    };
    if send_text_with_timeout(&mut socket, initial_json)
        .await
        .is_err()
    {
        return;
    }

    let mut ping_tick = tokio::time::interval(WS_PING_INTERVAL);
    ping_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    ping_tick.tick().await;

    loop {
        tokio::select! {
            changed = state_rx.changed() => {
                if changed.is_err() {
                    break;
                }
                let json = {
                    let latest = state_rx.borrow_and_update().clone();
                    match serde_json::to_string(&latest) {
                        Ok(s) => s,
                        Err(err) => {
                            log_error!("failed to serialize WebSocket state update: {err}");
                            continue;
                        }
                    }
                };
                if send_text_with_timeout(&mut socket, json).await.is_err() {
                    break;
                }
            }
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<EngineCommand>(text.as_str()) {
                            Ok(cmd) => {
                                match tokio::time::timeout(
                                    WS_CMD_TIMEOUT,
                                    state.control.cmd_tx.send(cmd),
                                ).await {
                                    Ok(Ok(())) => {}
                                    Ok(Err(_)) => break,
                                    Err(_) => {
                                        log_warn!("dropping WebSocket command: control channel saturated");
                                    }
                                }
                            }
                            Err(_) => {
                                if send_text_with_timeout(
                                    &mut socket,
                                    r#"{"error":"invalid command"}"#.to_string(),
                                ).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Binary(_))) => {}
                }
            }
            _ = ping_tick.tick() => {
                if tokio::time::timeout(WS_SEND_TIMEOUT, socket.send(Message::Ping(Bytes::new())))
                    .await
                    .is_err()
                {
                    break;
                }
            }
            _ = wait_shutdown(&mut state.shutdown_rx) => {
                let _ = tokio::time::timeout(
                    Duration::from_millis(250),
                    socket.send(Message::Close(None)),
                ).await;
                break;
            }
        }
    }
}

async fn send_text_with_timeout(socket: &mut WebSocket, json: String) -> Result<(), ()> {
    match tokio::time::timeout(WS_SEND_TIMEOUT, socket.send(Message::Text(json.into()))).await {
        Ok(Ok(())) => Ok(()),
        _ => Err(()),
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

#[cfg(test)]
mod tests {
    use tactus_protocol::{AppState, Direction, LockState, TelemetrySnapshot};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixStream;
    use tokio::sync::mpsc;

    use super::*;

    struct PairListener {
        rx: mpsc::UnboundedReceiver<UnixStream>,
    }

    impl axum::serve::Listener for PairListener {
        type Io = UnixStream;
        type Addr = ();

        async fn accept(&mut self) -> (Self::Io, Self::Addr) {
            match self.rx.recv().await {
                Some(stream) => (stream, ()),
                None => std::future::pending().await,
            }
        }

        fn local_addr(&self) -> std::io::Result<Self::Addr> {
            Ok(())
        }
    }

    struct TestServer {
        conn_tx: mpsc::UnboundedSender<UnixStream>,
        state_tx: watch::Sender<AppState>,
        cmd_rx: mpsc::Receiver<EngineCommand>,
        shutdown_tx: watch::Sender<bool>,
        handle: JoinHandle<()>,
    }

    impl TestServer {
        fn connect(&self) -> UnixStream {
            let (client, server) = UnixStream::pair().expect("create unix stream pair");
            self.conn_tx.send(server).expect("send accepted stream");
            client
        }
    }

    async fn spawn_test_server() -> TestServer {
        let initial = AppState {
            telemetry: TelemetrySnapshot {
                direction: Direction::LinkToMidi,
                bpm_link: 124.0,
                quantum: 4.0,
                ..TelemetrySnapshot::default()
            },
            midi_input_match: "WIDI".to_string(),
            midi_output_matches: vec!["WIDI".to_string()],
            ..AppState::default()
        };
        let (state_tx, state_rx) = watch::channel(initial);
        let (cmd_tx, cmd_rx) = mpsc::channel(16);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (conn_tx, conn_rx) = mpsc::unbounded_channel();

        let control = ControlHandle { state_rx, cmd_tx };
        let listener = PairListener { rx: conn_rx };
        let router = build_router(control, shutdown_rx.clone());
        let mut graceful_rx = shutdown_rx;
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, router)
                .with_graceful_shutdown(async move {
                    wait_shutdown(&mut graceful_rx).await;
                })
                .await;
        });

        TestServer {
            conn_tx,
            state_tx,
            cmd_rx,
            shutdown_tx,
            handle,
        }
    }

    async fn http_get(srv: &TestServer, path: &str) -> (String, Vec<u8>) {
        let mut stream = srv.connect();
        let req = format!("GET {path} HTTP/1.1\r\nHost: tactus.local\r\nConnection: close\r\n\r\n");
        stream.write_all(req.as_bytes()).await.expect("write req");
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).await.expect("read resp");

        let split = raw
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .expect("HTTP header separator");
        let headers = String::from_utf8_lossy(&raw[..split]).to_string();
        let body = raw[split + 4..].to_vec();
        (headers, body)
    }

    async fn ws_connect(srv: &TestServer) -> (UnixStream, Vec<u8>) {
        let mut stream = srv.connect();
        let req = "GET /ws HTTP/1.1\r\n\
             Host: tactus.local\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
             Sec-WebSocket-Version: 13\r\n\r\n";
        stream
            .write_all(req.as_bytes())
            .await
            .expect("write ws req");

        let mut buf = Vec::new();
        let mut tmp = [0u8; 1024];
        let header_end = loop {
            let n = stream.read(&mut tmp).await.expect("read ws handshake");
            assert!(n > 0, "unexpected EOF waiting for 101 handshake");
            buf.extend_from_slice(&tmp[..n]);
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break pos;
            }
        };
        let headers = String::from_utf8_lossy(&buf[..header_end]);
        assert!(
            headers.starts_with("HTTP/1.1 101"),
            "expected 101 Switching Protocols, got: {headers}"
        );
        let leftover = buf[header_end + 4..].to_vec();
        (stream, leftover)
    }

    async fn ws_read_exact(stream: &mut UnixStream, pending: &mut Vec<u8>, len: usize) -> Vec<u8> {
        let mut tmp = [0u8; 1024];
        while pending.len() < len {
            let n = stream.read(&mut tmp).await.expect("read ws bytes");
            assert!(n > 0, "unexpected EOF on WebSocket stream");
            pending.extend_from_slice(&tmp[..n]);
        }
        pending.drain(..len).collect()
    }

    async fn ws_read_next_text(stream: &mut UnixStream, pending: &mut Vec<u8>) -> String {
        loop {
            let hdr = ws_read_exact(stream, pending, 2).await;
            let opcode = hdr[0] & 0x0F;
            let masked = (hdr[1] & 0x80) != 0;
            let mut len = (hdr[1] & 0x7F) as usize;
            if len == 126 {
                let ext = ws_read_exact(stream, pending, 2).await;
                len = u16::from_be_bytes([ext[0], ext[1]]) as usize;
            } else if len == 127 {
                let ext = ws_read_exact(stream, pending, 8).await;
                len = u64::from_be_bytes(ext.try_into().expect("8-byte len")) as usize;
            }
            let mask_key = if masked {
                Some(ws_read_exact(stream, pending, 4).await)
            } else {
                None
            };
            let mut payload = ws_read_exact(stream, pending, len).await;
            if let Some(mask) = mask_key {
                for (i, b) in payload.iter_mut().enumerate() {
                    *b ^= mask[i % 4];
                }
            }
            if opcode == 0x1 {
                return String::from_utf8(payload).expect("utf8 text frame");
            }
        }
    }

    async fn ws_send_client_text(stream: &mut UnixStream, text: &str) {
        let bytes = text.as_bytes();
        let mask = [0x12u8, 0x34, 0x56, 0x78];
        let mut frame = Vec::with_capacity(6 + bytes.len());
        frame.push(0x81);
        assert!(bytes.len() < 126);
        frame.push(0x80 | (bytes.len() as u8));
        frame.extend_from_slice(&mask);
        for (i, &b) in bytes.iter().enumerate() {
            frame.push(b ^ mask[i % 4]);
        }
        stream
            .write_all(&frame)
            .await
            .expect("write client ws frame");
    }

    #[tokio::test]
    async fn serves_embedded_mobile_assets_and_rest_endpoints() {
        let srv = spawn_test_server().await;

        let (hdr, body) = http_get(&srv, "/").await;
        assert!(hdr.starts_with("HTTP/1.1 200"));
        let html = String::from_utf8(body).expect("utf8 index");
        assert!(html.contains("apple-mobile-web-app-capable"));
        assert!(html.contains("/apple-touch-icon.png"));
        assert!(html.contains("clock-thru-toggle"));
        assert!(html.contains("midi-out-list"));

        for (path, ctype_sub) in [
            ("/style.css", "text/css"),
            ("/app.js", "application/javascript"),
            ("/manifest.webmanifest", "application/manifest+json"),
            ("/icon.svg", "image/svg+xml"),
        ] {
            let (h, b) = http_get(&srv, path).await;
            assert!(h.starts_with("HTTP/1.1 200"), "{path} status: {h}");
            assert!(
                h.to_ascii_lowercase().contains(ctype_sub),
                "{path} type: {h}"
            );
            assert!(!b.is_empty(), "{path} empty");
        }

        let (png_hdr, png_body) = http_get(&srv, "/apple-touch-icon.png").await;
        assert!(png_hdr.starts_with("HTTP/1.1 200"));
        assert!(png_body.starts_with(b"\x89PNG\r\n\x1a\n"));

        let (state_hdr, state_body) = http_get(&srv, "/api/state").await;
        assert!(state_hdr.starts_with("HTTP/1.1 200"));
        let parsed_state: AppState = serde_json::from_slice(&state_body).expect("api state json");
        assert_eq!(parsed_state.telemetry.bpm_link, 124.0);
        assert_eq!(parsed_state.midi_input_match, "WIDI");

        let (hz_hdr, hz_body) = http_get(&srv, "/healthz").await;
        assert!(hz_hdr.starts_with("HTTP/1.1 200"));
        let hz_str = String::from_utf8(hz_body).expect("utf8 healthz");
        assert!(hz_str.contains(r#""status":"ok""#));

        let _ = srv.shutdown_tx.send(true);
        srv.handle.await.expect("server shutdown join");
    }

    #[tokio::test]
    async fn websocket_hydrates_pushes_updates_and_forwards_commands() {
        let mut srv = spawn_test_server().await;
        let (mut ws, mut pending) = ws_connect(&srv).await;

        let first = ws_read_next_text(&mut ws, &mut pending).await;
        let hydrated: AppState = serde_json::from_str(&first).expect("initial state json");
        assert_eq!(hydrated.telemetry.bpm_link, 124.0);
        assert_eq!(hydrated.telemetry.lock_state, LockState::Unlocked);

        srv.state_tx.send_modify(|s| {
            s.telemetry.bpm_link = 132.5;
            s.telemetry.lock_state = LockState::Locked;
        });
        let updated_raw = ws_read_next_text(&mut ws, &mut pending).await;
        let updated: AppState = serde_json::from_str(&updated_raw).expect("updated state json");
        assert_eq!(updated.telemetry.bpm_link, 132.5);
        assert_eq!(updated.telemetry.lock_state, LockState::Locked);

        ws_send_client_text(&mut ws, r#"{"set_direction":"midi_to_link"}"#).await;
        ws_send_client_text(&mut ws, r#""resync_downbeat""#).await;
        ws_send_client_text(&mut ws, r#"{"set_offset_ms":-4.5}"#).await;

        assert_eq!(
            srv.cmd_rx.recv().await,
            Some(EngineCommand::SetDirection(Direction::MidiToLink))
        );
        assert_eq!(srv.cmd_rx.recv().await, Some(EngineCommand::ResyncDownbeat));
        assert_eq!(
            srv.cmd_rx.recv().await,
            Some(EngineCommand::SetOffsetMs(-4.5))
        );

        // Malformed command returns structured error without disconnecting.
        ws_send_client_text(&mut ws, r#"{"bogus":123}"#).await;
        let err_frame = ws_read_next_text(&mut ws, &mut pending).await;
        assert!(err_frame.contains(r#""error""#));

        ws_send_client_text(&mut ws, r#"{"set_quantum":3.0}"#).await;
        assert_eq!(
            srv.cmd_rx.recv().await,
            Some(EngineCommand::SetQuantum(3.0))
        );

        ws_send_client_text(&mut ws, r#"{"set_clock_thru":false}"#).await;
        assert_eq!(
            srv.cmd_rx.recv().await,
            Some(EngineCommand::SetClockThru(false))
        );

        ws_send_client_text(
            &mut ws,
            r#"{"select_midi_ports":{"input":"OP-XY MIDI 1","outputs":["WIDI Uhost MIDI 1","OP-XY MIDI 1"]}}"#,
        )
        .await;
        assert_eq!(
            srv.cmd_rx.recv().await,
            Some(EngineCommand::SelectMidiPorts {
                input: "OP-XY MIDI 1".to_string(),
                outputs: vec!["WIDI Uhost MIDI 1".to_string(), "OP-XY MIDI 1".to_string()],
            })
        );

        drop(ws);
        let _ = srv.shutdown_tx.send(true);
        srv.handle.await.expect("server shutdown join");
    }
}
