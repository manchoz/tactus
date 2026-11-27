//! Async Tokio MessagePack-RPC client conforming to `arduino-router`,
//! `Arduino_RouterBridge`, and `Arduino_RPClite`.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
#[cfg(unix)]
use tokio::net::UnixStream;
use tokio::net::{TcpStream, ToSocketAddrs};
use tokio::sync::{mpsc, oneshot, watch, Semaphore};
use tokio::time;

use crate::codec::{decode_frame, encode_frame, encode_frame_checked, CodecLimits, RpcMessage};
use crate::error::{DecodeError, Error, Result, RpcError};
use crate::handler::{erase_raw_handler, ErasedHandler, Handler, IntoHandlerResult};
use crate::value::{Bytes, FromValue, IntoParams, Value};
use crate::{
    router_debug, router_warn, METHOD_MON_CONNECTED, METHOD_MON_READ, METHOD_MON_RESET,
    METHOD_MON_WRITE, METHOD_REGISTER, METHOD_RESET, METHOD_SERIAL_CLOSE, METHOD_SERIAL_OPEN,
    METHOD_SET_MAX_MSG_SIZE, METHOD_UNREGISTER, METHOD_VERSION,
};

/// Default Unix domain socket path used by `arduino-router` on Linux.
pub const DEFAULT_UNIX_SOCKET: &str = "/var/run/arduino-router.sock";
/// Environment variable overriding [`DEFAULT_UNIX_SOCKET`] (matching `arduino-router`).
pub const UNIX_SOCKET_ENV: &str = "ARDUINO_ROUTER_SOCKET";

/// Minimum permitted `$/setMaxMsgSize` threshold enforced by upstream `arduino-router`.
pub const MIN_MAX_MSG_SIZE: usize = 128;

/// Resolve the default Unix socket path, honoring `ARDUINO_ROUTER_SOCKET` if non-empty.
#[must_use]
pub fn default_unix_socket_path() -> PathBuf {
    match std::env::var_os(UNIX_SOCKET_ENV) {
        Some(val) if !val.is_empty() => PathBuf::from(val),
        _ => PathBuf::from(DEFAULT_UNIX_SOCKET),
    }
}

/// Configuration for a single [`Client`] connection session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientConfig {
    /// Frame size and nesting depth limits enforced by the decoder.
    pub limits: CodecLimits,
    /// Timeout applied to outbound `call*` operations (default: 10 seconds).
    pub call_timeout: Duration,
    /// Timeout applied to socket/stream writes (default: 5 seconds).
    pub write_timeout: Duration,
    /// Maximum concurrent incoming RPC handlers (`<= 1` enforces strict FIFO sequential dispatch;
    /// default: `32`).
    pub max_concurrent_handlers: usize,
    /// Capacity of the bounded outbound write queue (default: `128`).
    pub outbound_queue_capacity: usize,
    /// Optional local cap on outgoing frame byte size (default: `None`).
    pub max_outgoing_msg_size: Option<usize>,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            limits: CodecLimits::default(),
            call_timeout: Duration::from_secs(10),
            write_timeout: Duration::from_secs(5),
            max_concurrent_handlers: 32,
            outbound_queue_capacity: 128,
            max_outgoing_msg_size: None,
        }
    }
}

type PendingSender = oneshot::Sender<std::result::Result<Value, RpcError>>;

struct ClientInner {
    config: ClientConfig,
    next_msgid: AtomicU32,
    outbound_tx: mpsc::Sender<Vec<u8>>,
    pending: Mutex<HashMap<u32, PendingSender>>,
    handlers: RwLock<HashMap<String, ErasedHandler>>,
    closed: AtomicBool,
    closed_tx: watch::Sender<bool>,
    _closed_rx: watch::Receiver<bool>,
    shutdown_tx: watch::Sender<bool>,
    _shutdown_rx: watch::Receiver<bool>,
}

impl ClientInner {
    fn next_message_id(&self) -> u32 {
        loop {
            let id = self.next_msgid.fetch_add(1, Ordering::Relaxed);
            if id != 0 {
                return id;
            }
        }
    }

    fn encode_outbound(&self, msg: &RpcMessage) -> Result<Vec<u8>> {
        let mut buf = Vec::with_capacity(64);
        if let Some(max_out) = self.config.max_outgoing_msg_size {
            encode_frame_checked(msg, &mut buf, max_out)?;
        } else {
            encode_frame(msg, &mut buf)?;
        }
        Ok(buf)
    }

    fn mark_closed(&self) {
        if !self.closed.swap(true, Ordering::AcqRel) {
            let _ = self.closed_tx.send_replace(true);
            let _ = self.shutdown_tx.send_replace(true);
            let waiters = {
                let mut map = self.pending.lock().expect("pending mutex poisoned");
                map.drain().map(|(_, tx)| tx).collect::<Vec<_>>()
            };
            drop(waiters);
        }
    }
}

struct PendingGuard<'a> {
    pending: &'a Mutex<HashMap<u32, PendingSender>>,
    msgid: u32,
    armed: bool,
}

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            let mut map = self.pending.lock().expect("pending mutex poisoned");
            map.remove(&self.msgid);
        }
    }
}

enum IncomingDispatch {
    Request {
        msgid: u32,
        method: String,
        params: Vec<Value>,
    },
    Notification {
        method: String,
        params: Vec<Value>,
    },
}

struct ClientDropGuard(Arc<ClientInner>);

impl Drop for ClientDropGuard {
    fn drop(&mut self) {
        self.0.mark_closed();
    }
}

/// Async MessagePack-RPC client connected to an `arduino-router` instance or compatible peer.
///
/// `Client` is cheaply cloneable and multiplexes concurrent outbound `call` requests,
/// `notify` calls, and incoming `provide` callbacks over a single full-duplex stream.
/// Dropping the last `Client` clone automatically closes the session and stops its
/// background reader/writer tasks; call [`Client::close`] for explicit teardown.
#[derive(Clone)]
pub struct Client {
    inner: Arc<ClientInner>,
    _drop_guard: Arc<ClientDropGuard>,
}

impl Client {
    /// Connect to the default `arduino-router` Unix domain socket (`ARDUINO_ROUTER_SOCKET`
    /// or `/var/run/arduino-router.sock`).
    #[cfg(unix)]
    pub async fn connect_unix_default() -> Result<Self> {
        Self::connect_unix(default_unix_socket_path()).await
    }

    /// Connect to an `arduino-router` Unix domain socket at `path`.
    #[cfg(unix)]
    pub async fn connect_unix(path: impl AsRef<Path>) -> Result<Self> {
        Self::connect_unix_with_config(path, ClientConfig::default()).await
    }

    /// Connect to an `arduino-router` Unix domain socket at `path` with custom [`ClientConfig`].
    #[cfg(unix)]
    pub async fn connect_unix_with_config(
        path: impl AsRef<Path>,
        config: ClientConfig,
    ) -> Result<Self> {
        let stream = UnixStream::connect(path).await?;
        Ok(Self::from_stream(stream, config))
    }

    /// Connect to an `arduino-router` TCP listener at `addr`.
    pub async fn connect_tcp(addr: impl ToSocketAddrs) -> Result<Self> {
        Self::connect_tcp_with_config(addr, ClientConfig::default()).await
    }

    /// Connect to an `arduino-router` TCP listener at `addr` with custom [`ClientConfig`].
    pub async fn connect_tcp_with_config(
        addr: impl ToSocketAddrs,
        config: ClientConfig,
    ) -> Result<Self> {
        let stream = TcpStream::connect(addr).await?;
        let _ = stream.set_nodelay(true);
        Ok(Self::from_stream(stream, config))
    }

    /// Create a [`Client`] over any `AsyncRead + AsyncWrite` stream (such as `tokio::io::duplex`
    /// or `UnixStream::pair` in tests).
    pub fn from_stream<S>(stream: S, config: ClientConfig) -> Self
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let queue_cap = config.outbound_queue_capacity.max(1);
        let (outbound_tx, outbound_rx) = mpsc::channel(queue_cap);
        let (closed_tx, closed_rx) = watch::channel(false);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let inner = Arc::new(ClientInner {
            config,
            next_msgid: AtomicU32::new(1),
            outbound_tx,
            pending: Mutex::new(HashMap::new()),
            handlers: RwLock::new(HashMap::new()),
            closed: AtomicBool::new(false),
            closed_tx,
            _closed_rx: closed_rx,
            shutdown_tx,
            _shutdown_rx: shutdown_rx.clone(),
        });

        let (read_half, write_half) = tokio::io::split(stream);
        tokio::spawn(writer_loop(
            write_half,
            outbound_rx,
            Arc::clone(&inner),
            shutdown_rx.clone(),
        ));
        tokio::spawn(reader_loop(read_half, Arc::clone(&inner), shutdown_rx));

        Self {
            _drop_guard: Arc::new(ClientDropGuard(Arc::clone(&inner))),
            inner,
        }
    }

    /// Invoke a remote RPC method and deserialize its return value.
    pub async fn call<P, R>(&self, method: &str, params: P) -> Result<R>
    where
        P: IntoParams,
        R: FromValue,
    {
        self.call_with_timeout(method, params, self.inner.config.call_timeout)
            .await
    }

    /// Invoke a remote RPC method with an explicit per-call timeout.
    pub async fn call_with_timeout<P, R>(
        &self,
        method: &str,
        params: P,
        timeout: Duration,
    ) -> Result<R>
    where
        P: IntoParams,
        R: FromValue,
    {
        let value = self
            .call_raw_with_timeout(method, params.into_params()?, timeout)
            .await?;
        R::from_value(value).map_err(Error::from)
    }

    /// Invoke a remote RPC method using raw [`Value`] parameters and return value.
    pub async fn call_raw(&self, method: &str, params: Vec<Value>) -> Result<Value> {
        self.call_raw_with_timeout(method, params, self.inner.config.call_timeout)
            .await
    }

    /// Invoke a remote RPC method with raw [`Value`] types and an explicit timeout.
    pub async fn call_raw_with_timeout(
        &self,
        method: &str,
        params: Vec<Value>,
        timeout: Duration,
    ) -> Result<Value> {
        if self.is_closed() {
            return Err(Error::Closed);
        }

        let msgid = self.inner.next_message_id();
        let bytes = self.inner.encode_outbound(&RpcMessage::Request {
            msgid,
            method: method.to_string(),
            params,
        })?;

        let (tx, rx) = oneshot::channel();
        {
            let mut map = self.inner.pending.lock().expect("pending mutex poisoned");
            if self.is_closed() {
                return Err(Error::Closed);
            }
            map.insert(msgid, tx);
        }

        let mut guard = PendingGuard {
            pending: &self.inner.pending,
            msgid,
            armed: true,
        };

        if self.inner.outbound_tx.send(bytes).await.is_err() {
            return Err(Error::Closed);
        }

        let response = match time::timeout(timeout, rx).await {
            Ok(Ok(res)) => {
                guard.armed = false;
                res
            }
            Ok(Err(_)) => {
                guard.armed = false;
                return Err(Error::Closed);
            }
            Err(_) => {
                drop(guard);
                return Err(Error::Timeout);
            }
        };

        response.map_err(Error::Rpc)
    }

    /// Send a fire-and-forget MessagePack-RPC notification with typed positional arguments.
    pub async fn notify<P: IntoParams>(&self, method: &str, params: P) -> Result<()> {
        self.notify_raw(method, params.into_params()?).await
    }

    /// Send a fire-and-forget MessagePack-RPC notification with raw [`Value`] arguments.
    pub async fn notify_raw(&self, method: &str, params: Vec<Value>) -> Result<()> {
        if self.is_closed() {
            return Err(Error::Closed);
        }

        let bytes = self.inner.encode_outbound(&RpcMessage::Notification {
            method: method.to_string(),
            params,
        })?;

        self.inner
            .outbound_tx
            .send(bytes)
            .await
            .map_err(|_| Error::Closed)
    }

    /// Register a typed async callback with the router (`$/register`).
    ///
    /// Both incoming RPC requests and notifications for `method` are dispatched to `handler`.
    /// If `$/register` fails on the router (for example [`crate::ErrorCode::RouteAlreadyExists`]),
    /// local handler state is rolled back cleanly.
    pub async fn provide<H, Args>(&self, method: impl Into<String>, handler: H) -> Result<()>
    where
        H: Handler<Args>,
    {
        self.provide_erased(method.into(), handler.into_erased())
            .await
    }

    /// Register a raw async callback receiving `Vec<Value>` and returning `Result<R, RpcError>`.
    pub async fn provide_raw<F, Fut, R>(&self, method: impl Into<String>, handler: F) -> Result<()>
    where
        F: Fn(Vec<Value>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = R> + Send + 'static,
        R: IntoHandlerResult,
    {
        self.provide_erased(method.into(), erase_raw_handler(handler))
            .await
    }

    pub(crate) async fn provide_erased(
        &self,
        method: String,
        handler: ErasedHandler,
    ) -> Result<()> {
        let prev = {
            let mut map = self.inner.handlers.write().expect("handlers lock poisoned");
            map.insert(method.clone(), handler)
        };

        match self.register(&method).await {
            Ok(true) => Ok(()),
            Ok(false) => {
                self.restore_handler(&method, prev);
                Err(Error::Rpc(RpcError::generic(format!(
                    "router rejected registration of `{method}`"
                ))))
            }
            Err(err) => {
                self.restore_handler(&method, prev);
                Err(err)
            }
        }
    }

    fn restore_handler(&self, method: &str, prev: Option<ErasedHandler>) {
        let mut map = self.inner.handlers.write().expect("handlers lock poisoned");
        match prev {
            Some(h) => {
                map.insert(method.to_string(), h);
            }
            None => {
                map.remove(method);
            }
        }
    }

    pub(crate) fn install_handlers_snapshot(&self, snapshot: &HashMap<String, ErasedHandler>) {
        let mut map = self.inner.handlers.write().expect("handlers lock poisoned");
        for (k, v) in snapshot {
            map.insert(k.clone(), Arc::clone(v));
        }
    }

    /// Unregister a previously provided method from the router (`$/unregister`) and remove
    /// its local handler.
    pub async fn unprovide(&self, method: &str) -> Result<bool> {
        let removed = self.unregister(method).await?;
        if removed {
            let mut map = self.inner.handlers.write().expect("handlers lock poisoned");
            map.remove(method);
        }
        Ok(removed)
    }

    /// Invoke `$/register` on the router directly.
    pub async fn register(&self, method: &str) -> Result<bool> {
        self.call(METHOD_REGISTER, (method,)).await
    }

    /// Invoke `$/unregister` on the router directly.
    pub async fn unregister(&self, method: &str) -> Result<bool> {
        self.call(METHOD_UNREGISTER, (method,)).await
    }

    /// Drop all routes registered by this connection on the router (`$/reset`) and clear
    /// local handlers.
    pub async fn reset(&self) -> Result<bool> {
        let ok: bool = self.call(METHOD_RESET, ()).await?;
        if ok {
            let mut map = self.inner.handlers.write().expect("handlers lock poisoned");
            map.clear();
        }
        Ok(ok)
    }

    /// Query the running `arduino-router` daemon version (`$/version`).
    pub async fn version(&self) -> Result<String> {
        self.call(METHOD_VERSION, ()).await
    }

    /// Set the router's maximum forwarded message size for this connection (`$/setMaxMsgSize`).
    ///
    /// Upstream `arduino-router` requires `max_bytes > 127`.
    pub async fn set_max_msg_size(&self, max_bytes: usize) -> Result<bool> {
        if max_bytes < MIN_MAX_MSG_SIZE {
            return Err(Error::InvalidArgument(format!(
                "max_bytes must be >= {MIN_MAX_MSG_SIZE}, got {max_bytes}"
            )));
        }
        self.call(METHOD_SET_MAX_MSG_SIZE, (max_bytes,)).await
    }

    /// Set the router's maximum forwarded message size using a fire-and-forget notification
    /// (`$/setMaxMsgSize`), matching `Arduino_RouterBridge::begin()`.
    ///
    /// Upstream `arduino-router` requires `max_bytes > 127`.
    pub async fn notify_max_msg_size(&self, max_bytes: usize) -> Result<()> {
        if max_bytes < MIN_MAX_MSG_SIZE {
            return Err(Error::InvalidArgument(format!(
                "max_bytes must be >= {MIN_MAX_MSG_SIZE}, got {max_bytes}"
            )));
        }
        self.notify(METHOD_SET_MAX_MSG_SIZE, (max_bytes,)).await
    }

    /// Ask `arduino-router` to open its configured serial link to the MCU (`$/serial/open`).
    pub async fn serial_open(&self, port: &str) -> Result<bool> {
        self.call(METHOD_SERIAL_OPEN, (port,)).await
    }

    /// Ask `arduino-router` to close its configured serial link to the MCU (`$/serial/close`).
    pub async fn serial_close(&self, port: &str) -> Result<bool> {
        self.call(METHOD_SERIAL_CLOSE, (port,)).await
    }

    /// Check whether at least one monitor client is connected to the router's TCP monitor proxy
    /// (`mon/connected`).
    pub async fn monitor_connected(&self) -> Result<bool> {
        self.call(METHOD_MON_CONNECTED, ()).await
    }

    /// Close all active monitor TCP connections on the router (`mon/reset`).
    pub async fn monitor_reset(&self) -> Result<bool> {
        self.call(METHOD_MON_RESET, ()).await
    }

    /// Read up to `max_bytes` buffered monitor bytes sent by TCP monitor clients (`mon/read`).
    pub async fn monitor_read(&self, max_bytes: usize) -> Result<Vec<u8>> {
        let bytes: Bytes = self.call(METHOD_MON_READ, (max_bytes,)).await?;
        Ok(bytes.into_vec())
    }

    /// Write raw bytes to connected TCP monitor clients (`mon/write` request) and return the
    /// byte count reported by the router.
    pub async fn monitor_write(&self, data: &[u8]) -> Result<usize> {
        self.call(METHOD_MON_WRITE, (Bytes(data.to_vec()),)).await
    }

    /// Write raw bytes to connected TCP monitor clients using a fire-and-forget notification
    /// (`mon/write` notification).
    pub async fn monitor_write_notify(&self, data: &[u8]) -> Result<()> {
        self.notify(METHOD_MON_WRITE, (Bytes(data.to_vec()),)).await
    }

    /// Returns `true` if the underlying session has closed or been shut down.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::Acquire)
    }

    /// Completes when the underlying client session closes.
    pub async fn closed(&self) {
        let mut rx = self.inner.closed_tx.subscribe();
        wait_true(&mut rx).await;
    }

    /// Close the client session and stop background reader/writer tasks.
    pub async fn close(&self) {
        self.inner.mark_closed();
    }

    pub(crate) fn close_now(&self) {
        self.inner.mark_closed();
    }

    pub(crate) fn subscribe_closed(&self) -> watch::Receiver<bool> {
        self.inner.closed_tx.subscribe()
    }
}

async fn writer_loop<W>(
    mut writer: W,
    mut outbound_rx: mpsc::Receiver<Vec<u8>>,
    inner: Arc<ClientInner>,
    mut shutdown_rx: watch::Receiver<bool>,
) where
    W: AsyncWrite + Unpin + Send,
{
    let write_timeout = inner.config.write_timeout;

    loop {
        let bytes = tokio::select! {
            biased;
            () = wait_true(&mut shutdown_rx) => break,
            maybe_bytes = outbound_rx.recv() => match maybe_bytes {
                Some(b) => b,
                None => break,
            }
        };

        let write_fut = async {
            writer.write_all(&bytes).await?;
            writer.flush().await
        };

        let res = tokio::select! {
            biased;
            () = wait_true(&mut shutdown_rx) => break,
            res = time::timeout(write_timeout, write_fut) => res,
        };

        match res {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                router_warn!("arduino-router socket write failed: {err}");
                break;
            }
            Err(_) => {
                router_warn!("arduino-router socket write timed out");
                break;
            }
        }
    }

    let _ = writer.shutdown().await;
    inner.mark_closed();
}

async fn reader_loop<R>(
    mut reader: R,
    inner: Arc<ClientInner>,
    mut shutdown_rx: watch::Receiver<bool>,
) where
    R: AsyncRead + Unpin + Send,
{
    let max_frame_bytes = inner.config.limits.max_frame_bytes.max(1);
    let max_concurrent = inner.config.max_concurrent_handlers;

    let fifo_tx = if max_concurrent <= 1 {
        let cap = inner.config.outbound_queue_capacity.max(16);
        let (tx, mut rx) = mpsc::channel::<IncomingDispatch>(cap);
        let inner_c = Arc::clone(&inner);
        let mut shutdown_c = shutdown_rx.clone();
        tokio::spawn(async move {
            loop {
                let item = tokio::select! {
                    biased;
                    () = wait_true(&mut shutdown_c) => break,
                    item = rx.recv() => match item {
                        Some(msg) => msg,
                        None => break,
                    }
                };
                dispatch_one(&inner_c, item).await;
            }
        });
        Some(tx)
    } else {
        None
    };

    let semaphore = if max_concurrent > 1 {
        Some(Arc::new(Semaphore::new(max_concurrent)))
    } else {
        None
    };

    let mut read_buf = Vec::with_capacity(4096.min(max_frame_bytes));
    let mut tmp = [0u8; 4096];

    'outer: loop {
        let avail = max_frame_bytes.saturating_sub(read_buf.len());
        if avail == 0 {
            router_warn!(
                "arduino-router incoming frame exceeded {max_frame_bytes} byte limit; closing"
            );
            break;
        }
        let read_cap = avail.min(tmp.len());

        let n = tokio::select! {
            biased;
            () = wait_true(&mut shutdown_rx) => break,
            res = reader.read(&mut tmp[..read_cap]) => match res {
                Ok(0) => {
                    router_debug!("arduino-router peer closed connection (EOF)");
                    break;
                }
                Ok(n) => n,
                Err(err) => {
                    router_warn!("arduino-router socket read failed: {err}");
                    break;
                }
            }
        };

        read_buf.extend_from_slice(&tmp[..n]);

        loop {
            match decode_frame(&read_buf, inner.config.limits) {
                Ok((msg, consumed)) => {
                    read_buf.drain(..consumed);
                    match msg {
                        RpcMessage::Response {
                            msgid,
                            error,
                            result,
                        } => {
                            let sender = {
                                let mut map = inner.pending.lock().expect("pending mutex poisoned");
                                map.remove(&msgid)
                            };
                            if let Some(tx) = sender {
                                let outcome = if error.is_nil() {
                                    Ok(result)
                                } else {
                                    Err(RpcError::from_wire(error))
                                };
                                let _ = tx.send(outcome);
                            } else {
                                router_debug!(
                                    "received response for unknown or cancelled msgid={msgid}"
                                );
                            }
                        }
                        RpcMessage::Request {
                            msgid,
                            method,
                            params,
                        } => {
                            let dispatch = IncomingDispatch::Request {
                                msgid,
                                method,
                                params,
                            };
                            if !enqueue_dispatch(
                                dispatch,
                                &inner,
                                fifo_tx.as_ref(),
                                semaphore.as_ref(),
                                &mut shutdown_rx,
                            )
                            .await
                            {
                                break 'outer;
                            }
                        }
                        RpcMessage::Notification { method, params } => {
                            let dispatch = IncomingDispatch::Notification { method, params };
                            if !enqueue_dispatch(
                                dispatch,
                                &inner,
                                fifo_tx.as_ref(),
                                semaphore.as_ref(),
                                &mut shutdown_rx,
                            )
                            .await
                            {
                                break 'outer;
                            }
                        }
                    }
                }
                Err(DecodeError::Incomplete) => {
                    if read_buf.len() >= max_frame_bytes {
                        router_warn!(
                            "arduino-router incomplete frame exceeded {max_frame_bytes} byte limit"
                        );
                        break 'outer;
                    }
                    break;
                }
                Err(DecodeError::Oversized) => {
                    router_warn!("arduino-router received oversized frame; closing");
                    break 'outer;
                }
                Err(DecodeError::Malformed(reason)) => {
                    router_warn!("arduino-router received malformed frame ({reason}); closing");
                    break 'outer;
                }
            }
        }
    }

    inner.mark_closed();
}

async fn enqueue_dispatch(
    item: IncomingDispatch,
    inner: &Arc<ClientInner>,
    fifo_tx: Option<&mpsc::Sender<IncomingDispatch>>,
    semaphore: Option<&Arc<Semaphore>>,
    shutdown_rx: &mut watch::Receiver<bool>,
) -> bool {
    if let Some(tx) = fifo_tx {
        tokio::select! {
            biased;
            () = wait_true(shutdown_rx) => false,
            res = tx.send(item) => res.is_ok(),
        }
    } else if let Some(sem) = semaphore {
        let permit = tokio::select! {
            biased;
            () = wait_true(shutdown_rx) => return false,
            res = Arc::clone(sem).acquire_owned() => match res {
                Ok(p) => p,
                Err(_) => return false,
            }
        };
        let inner_c = Arc::clone(inner);
        tokio::spawn(async move {
            let _permit = permit;
            dispatch_one(&inner_c, item).await;
        });
        true
    } else {
        false
    }
}

async fn dispatch_one(inner: &Arc<ClientInner>, item: IncomingDispatch) {
    match item {
        IncomingDispatch::Request {
            msgid,
            method,
            params,
        } => {
            let handler = {
                let map = inner.handlers.read().expect("handlers lock poisoned");
                map.get(&method).cloned()
            };

            let resp = match handler {
                Some(h) => match h(params).await {
                    Ok(result) => RpcMessage::Response {
                        msgid,
                        error: Value::Nil,
                        result,
                    },
                    Err(rpc_err) => RpcMessage::Response {
                        msgid,
                        error: rpc_err.to_wire(),
                        result: Value::Nil,
                    },
                },
                None => RpcMessage::Response {
                    msgid,
                    error: RpcError::function_not_found(format!("method not found: {method}"))
                        .to_wire(),
                    result: Value::Nil,
                },
            };

            match inner.encode_outbound(&resp) {
                Ok(bytes) => {
                    let _ = inner.outbound_tx.send(bytes).await;
                }
                Err(err) => {
                    router_warn!("failed encoding response for `{method}` (msgid={msgid}): {err}");
                }
            }
        }
        IncomingDispatch::Notification { method, params } => {
            let handler = {
                let map = inner.handlers.read().expect("handlers lock poisoned");
                map.get(&method).cloned()
            };
            if let Some(h) = handler {
                if let Err(err) = h(params).await {
                    router_warn!("handler error on notification `{method}`: {err}");
                }
            }
        }
    }
}

pub(crate) async fn wait_true(rx: &mut watch::Receiver<bool>) {
    if *rx.borrow_and_update() {
        return;
    }
    while rx.changed().await.is_ok() {
        if *rx.borrow_and_update() {
            return;
        }
    }
}
