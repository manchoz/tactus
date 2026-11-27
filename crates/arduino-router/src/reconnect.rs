//! Auto-reconnecting supervisor (`ManagedClient`) with exponential backoff and
//! automatic `$/register` handler replay across router restarts.

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
#[cfg(unix)]
use tokio::net::UnixStream;
use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;
use tokio::time;

use crate::client::{wait_true, Client, ClientConfig};
use crate::error::{Error, Result};
use crate::handler::{erase_raw_handler, BoxFuture, ErasedHandler, Handler, IntoHandlerResult};
use crate::router_warn;
use crate::value::{FromValue, IntoParams, Value};

/// Marker trait for async bidirectional streams accepted by custom [`ManagedClient`] connectors.
pub trait AsyncStream: AsyncRead + AsyncWrite + Unpin + Send {}

impl<T: AsyncRead + AsyncWrite + Unpin + Send> AsyncStream for T {}

/// Type-erased stream returned by custom [`ManagedClientBuilder`] connectors.
pub type BoxStream = Box<dyn AsyncStream>;

type ConnectorFn = Arc<dyn Fn() -> BoxFuture<'static, std::io::Result<BoxStream>> + Send + Sync>;

/// Exponential reconnect backoff schedule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconnectPolicy {
    /// Delay after the first failed connect or unestablished disconnect (default: 250 ms).
    pub initial_backoff: Duration,
    /// Maximum backoff cap (default: 5 s).
    pub max_backoff: Duration,
    /// Exponential multiplier applied after consecutive failures (default: `2`).
    pub multiplier: u32,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            initial_backoff: Duration::from_millis(250),
            max_backoff: Duration::from_secs(5),
            multiplier: 2,
        }
    }
}

impl ReconnectPolicy {
    /// Compute the next backoff duration clamped to `max_backoff`.
    #[must_use]
    pub const fn next_backoff(&self, current: Duration) -> Duration {
        let factor = if self.multiplier == 0 {
            1
        } else {
            self.multiplier
        };
        let scaled = current.saturating_mul(factor);
        if scaled.as_millis() > self.max_backoff.as_millis() {
            self.max_backoff
        } else {
            scaled
        }
    }
}

/// Lifecycle events emitted by [`ManagedClient`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LifecycleEvent {
    /// A session connected and completed initial `$/register` replay.
    Connected,
    /// An established session disconnected.
    Disconnected {
        /// Human-readable disconnect reason.
        reason: String,
    },
    /// Connecting (or establishing the initial handshake) failed and will retry.
    ConnectFailed {
        /// Error description for the failed connect or handshake attempt.
        error: String,
        /// Backoff delay before the next reconnect attempt.
        next_retry: Duration,
    },
    /// Replaying `$/register` for a pre-registered method failed on connect.
    RegisterFailed {
        /// Method name whose `$/register` call failed.
        method: String,
        /// Error description returned by the router or transport.
        error: String,
    },
}

/// Target transport endpoint for [`ManagedClientBuilder`].
#[derive(Clone)]
pub enum Endpoint {
    /// Unix domain socket path (e.g. `/var/run/arduino-router.sock`).
    #[cfg(unix)]
    Unix(PathBuf),
    /// TCP host/port address (e.g. `127.0.0.1:8800`).
    Tcp(String),
    /// Custom async stream connector factory.
    Custom(ConnectorFn),
}

impl Endpoint {
    /// Construct a custom async connector endpoint.
    pub fn custom<F, Fut, S>(connector: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = std::io::Result<S>> + Send + 'static,
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let connector = Arc::new(connector);
        Self::Custom(Arc::new(move || {
            let connector = Arc::clone(&connector);
            Box::pin(async move {
                let stream = connector().await?;
                Ok(Box::new(stream) as BoxStream)
            })
        }))
    }
}

/// Builder for [`ManagedClient`].
pub struct ManagedClientBuilder {
    endpoint: Endpoint,
    client_config: ClientConfig,
    reconnect_policy: ReconnectPolicy,
    handlers: HashMap<String, ErasedHandler>,
    event_capacity: usize,
}

impl ManagedClientBuilder {
    /// Create a builder targeting `endpoint`.
    #[must_use]
    pub fn new(endpoint: Endpoint) -> Self {
        Self {
            endpoint,
            client_config: ClientConfig::default(),
            reconnect_policy: ReconnectPolicy::default(),
            handlers: HashMap::new(),
            event_capacity: 64,
        }
    }

    /// Override the underlying [`ClientConfig`] used for each session.
    #[must_use]
    pub fn client_config(mut self, config: ClientConfig) -> Self {
        self.client_config = config;
        self
    }

    /// Override the [`ReconnectPolicy`].
    #[must_use]
    pub fn reconnect_policy(mut self, policy: ReconnectPolicy) -> Self {
        self.reconnect_policy = policy;
        self
    }

    /// Set the capacity of the [`LifecycleEvent`] broadcast channel (default: `64`).
    #[must_use]
    pub fn event_capacity(mut self, capacity: usize) -> Self {
        self.event_capacity = capacity.max(1);
        self
    }

    /// Register a typed RPC callback to be re-registered via `$/register` on every connect.
    #[must_use]
    pub fn provide<H, Args>(mut self, method: impl Into<String>, handler: H) -> Self
    where
        H: Handler<Args>,
    {
        self.handlers.insert(method.into(), handler.into_erased());
        self
    }

    /// Register a raw RPC callback to be re-registered via `$/register` on every connect.
    #[must_use]
    pub fn provide_raw<F, Fut, R>(mut self, method: impl Into<String>, handler: F) -> Self
    where
        F: Fn(Vec<Value>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = R> + Send + 'static,
        R: IntoHandlerResult,
    {
        self.handlers
            .insert(method.into(), erase_raw_handler(handler));
        self
    }

    /// Spawn the background reconnect supervisor task and return a [`ManagedClient`].
    #[must_use]
    pub fn spawn(self) -> ManagedClient {
        let (active_tx, active_rx) = watch::channel(None::<Client>);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (events_tx, _) = broadcast::channel(self.event_capacity);

        let inner = Arc::new(ManagedClientInner {
            handlers: RwLock::new(self.handlers),
            active_tx,
            _active_rx: active_rx,
            shutdown_tx,
            _shutdown_rx: shutdown_rx.clone(),
            events_tx,
        });

        let task = tokio::spawn(supervisor_loop(
            self.endpoint,
            self.client_config,
            self.reconnect_policy,
            Arc::clone(&inner),
            shutdown_rx,
        ));

        ManagedClient {
            _drop_guard: Arc::new(ManagedClientDropGuard(Arc::clone(&inner))),
            inner,
            task: Arc::new(tokio::sync::Mutex::new(Some(task))),
        }
    }
}

struct ManagedClientDropGuard(Arc<ManagedClientInner>);

impl Drop for ManagedClientDropGuard {
    fn drop(&mut self) {
        let _ = self.0.shutdown_tx.send_replace(true);
        if let Some(client) = self.0.active_tx.borrow().clone() {
            client.close_now();
        }
    }
}

struct ManagedClientInner {
    handlers: RwLock<HashMap<String, ErasedHandler>>,
    active_tx: watch::Sender<Option<Client>>,
    _active_rx: watch::Receiver<Option<Client>>,
    shutdown_tx: watch::Sender<bool>,
    _shutdown_rx: watch::Receiver<bool>,
    events_tx: broadcast::Sender<LifecycleEvent>,
}

/// Auto-reconnecting supervisor wrapping [`Client`] with automatic `$/register` replay.
///
/// Any `$/register` failure during connect aborts the handshake and retries the session
/// according to [`ReconnectPolicy`]. Dropping the last [`ManagedClient`] clone automatically
/// stops the background supervisor task and closes any active session; call
/// [`ManagedClient::shutdown`] when you want to await deterministic teardown.
#[derive(Clone)]
pub struct ManagedClient {
    inner: Arc<ManagedClientInner>,
    task: Arc<tokio::sync::Mutex<Option<JoinHandle<()>>>>,
    _drop_guard: Arc<ManagedClientDropGuard>,
}

impl ManagedClient {
    /// Start building a [`ManagedClient`] targeting a Unix socket path.
    #[cfg(unix)]
    #[must_use]
    pub fn builder_unix(path: impl Into<PathBuf>) -> ManagedClientBuilder {
        ManagedClientBuilder::new(Endpoint::Unix(path.into()))
    }

    /// Start building a [`ManagedClient`] targeting a TCP address.
    #[must_use]
    pub fn builder_tcp(addr: impl Into<String>) -> ManagedClientBuilder {
        ManagedClientBuilder::new(Endpoint::Tcp(addr.into()))
    }

    /// Start building a [`ManagedClient`] using a custom async stream connector.
    #[must_use]
    pub fn builder_with_connector<F, Fut, S>(connector: F) -> ManagedClientBuilder
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = std::io::Result<S>> + Send + 'static,
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        ManagedClientBuilder::new(Endpoint::custom(connector))
    }

    /// Return the currently active [`Client`], or `None` while disconnected.
    #[must_use]
    pub fn current_client(&self) -> Option<Client> {
        self.inner.active_tx.borrow().clone()
    }

    /// Returns `true` when an active connected session is established.
    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.inner.active_tx.borrow().is_some()
    }

    /// Subscribe to [`LifecycleEvent`] notifications.
    #[must_use]
    pub fn subscribe_events(&self) -> broadcast::Receiver<LifecycleEvent> {
        self.inner.events_tx.subscribe()
    }

    /// Wait until a session is connected and all `$/register` handlers have been replayed,
    /// returning the active [`Client`]. Returns [`Error::Closed`] if [`ManagedClient::shutdown`]
    /// was called.
    pub async fn wait_connected(&self) -> Result<Client> {
        let mut active_rx = self.inner.active_tx.subscribe();
        let mut shutdown_rx = self.inner.shutdown_tx.subscribe();

        loop {
            if let Some(client) = active_rx.borrow_and_update().clone() {
                return Ok(client);
            }
            if *shutdown_rx.borrow_and_update() {
                return Err(Error::Closed);
            }
            tokio::select! {
                biased;
                () = wait_true(&mut shutdown_rx) => return Err(Error::Closed),
                res = active_rx.changed() => {
                    if res.is_err() {
                        return Err(Error::Closed);
                    }
                }
            }
        }
    }

    /// Invoke a remote RPC method on the active session (fails immediately with
    /// [`Error::Closed`] if disconnected).
    pub async fn call<P, R>(&self, method: &str, params: P) -> Result<R>
    where
        P: IntoParams,
        R: FromValue,
    {
        let client = self.current_client().ok_or(Error::Closed)?;
        client.call(method, params).await
    }

    /// Invoke a remote RPC method with an explicit timeout on the active session.
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
        let client = self.current_client().ok_or(Error::Closed)?;
        client.call_with_timeout(method, params, timeout).await
    }

    /// Invoke a raw RPC method on the active session.
    pub async fn call_raw(&self, method: &str, params: Vec<Value>) -> Result<Value> {
        let client = self.current_client().ok_or(Error::Closed)?;
        client.call_raw(method, params).await
    }

    /// Send a fire-and-forget notification on the active session.
    pub async fn notify<P: IntoParams>(&self, method: &str, params: P) -> Result<()> {
        let client = self.current_client().ok_or(Error::Closed)?;
        client.notify(method, params).await
    }

    /// Send a raw fire-and-forget notification on the active session.
    pub async fn notify_raw(&self, method: &str, params: Vec<Value>) -> Result<()> {
        let client = self.current_client().ok_or(Error::Closed)?;
        client.notify_raw(method, params).await
    }

    /// Register a typed callback for the current session (if connected) and future reconnects.
    pub async fn provide<H, Args>(&self, method: impl Into<String>, handler: H) -> Result<()>
    where
        H: Handler<Args>,
    {
        self.provide_erased(method.into(), handler.into_erased())
            .await
    }

    /// Register a raw callback for the current session (if connected) and future reconnects.
    pub async fn provide_raw<F, Fut, R>(&self, method: impl Into<String>, handler: F) -> Result<()>
    where
        F: Fn(Vec<Value>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = R> + Send + 'static,
        R: IntoHandlerResult,
    {
        self.provide_erased(method.into(), erase_raw_handler(handler))
            .await
    }

    async fn provide_erased(&self, method: String, handler: ErasedHandler) -> Result<()> {
        let prev = {
            let mut map = self.inner.handlers.write().expect("handlers lock poisoned");
            map.insert(method.clone(), Arc::clone(&handler))
        };

        if let Some(client) = self.current_client() {
            if let Err(err) = client.provide_erased(method.clone(), handler).await {
                let mut map = self.inner.handlers.write().expect("handlers lock poisoned");
                match prev {
                    Some(h) => {
                        map.insert(method, h);
                    }
                    None => {
                        map.remove(&method);
                    }
                }
                return Err(err);
            }
        }
        Ok(())
    }

    /// Unregister a handler from future reconnects and (if connected) from the active session.
    pub async fn unprovide(&self, method: &str) -> Result<bool> {
        {
            let mut map = self.inner.handlers.write().expect("handlers lock poisoned");
            map.remove(method);
        }
        if let Some(client) = self.current_client() {
            client.unprovide(method).await
        } else {
            Ok(false)
        }
    }

    /// Stop the reconnect supervisor and close any active session.
    pub async fn shutdown(&self) {
        let _ = self.inner.shutdown_tx.send_replace(true);
        if let Some(client) = self.current_client() {
            client.close().await;
        }
        let handle = {
            let mut guard = self.task.lock().await;
            guard.take()
        };
        if let Some(h) = handle {
            let _ = h.await;
        }
    }
}

async fn connect_endpoint(endpoint: &Endpoint, config: &ClientConfig) -> std::io::Result<Client> {
    match endpoint {
        #[cfg(unix)]
        Endpoint::Unix(path) => {
            let stream = UnixStream::connect(path).await?;
            Ok(Client::from_stream(stream, config.clone()))
        }
        Endpoint::Tcp(addr) => {
            let stream = TcpStream::connect(addr.as_str()).await?;
            let _ = stream.set_nodelay(true);
            Ok(Client::from_stream(stream, config.clone()))
        }
        Endpoint::Custom(factory) => {
            let stream = factory().await?;
            Ok(Client::from_stream(stream, config.clone()))
        }
    }
}

async fn supervisor_loop(
    endpoint: Endpoint,
    client_config: ClientConfig,
    policy: ReconnectPolicy,
    inner: Arc<ManagedClientInner>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    let mut backoff = policy.initial_backoff;

    loop {
        if *shutdown_rx.borrow_and_update() {
            break;
        }

        let connect_res = tokio::select! {
            biased;
            () = wait_true(&mut shutdown_rx) => break,
            res = connect_endpoint(&endpoint, &client_config) => res,
        };

        let mut established = false;
        let mut disconnect_reason = None;

        match connect_res {
            Ok(client) => {
                let snapshot = {
                    let map = inner.handlers.read().expect("handlers lock poisoned");
                    map.clone()
                };
                client.install_handlers_snapshot(&snapshot);

                let mut methods: Vec<String> = snapshot.keys().cloned().collect();
                methods.sort();

                let mut handshake_ok = true;
                for method in &methods {
                    let reg_res = tokio::select! {
                        biased;
                        () = wait_true(&mut shutdown_rx) => {
                            client.close().await;
                            return;
                        }
                        res = client.register(method) => res,
                    };
                    match reg_res {
                        Ok(true) => {}
                        Ok(false) => {
                            let err_msg = format!("router rejected $/register for `{method}`");
                            let _ = inner.events_tx.send(LifecycleEvent::RegisterFailed {
                                method: method.clone(),
                                error: err_msg.clone(),
                            });
                            router_warn!("arduino-router ManagedClient: {err_msg}");
                            handshake_ok = false;
                            disconnect_reason = Some(err_msg);
                            break;
                        }
                        Err(err) => {
                            let err_msg = err.to_string();
                            let _ = inner.events_tx.send(LifecycleEvent::RegisterFailed {
                                method: method.clone(),
                                error: err_msg.clone(),
                            });
                            router_warn!(
                                "arduino-router ManagedClient $/register `{method}` failed: {err_msg}"
                            );
                            handshake_ok = false;
                            disconnect_reason = Some(err_msg);
                            break;
                        }
                    }
                }

                if handshake_ok && !client.is_closed() {
                    established = true;
                    backoff = policy.initial_backoff;
                    let _ = inner.active_tx.send_replace(Some(client.clone()));
                    let _ = inner.events_tx.send(LifecycleEvent::Connected);

                    let mut closed_rx = client.subscribe_closed();
                    tokio::select! {
                        biased;
                        () = wait_true(&mut shutdown_rx) => {
                            let _ = inner.active_tx.send_replace(None);
                            client.close().await;
                            break;
                        }
                        () = wait_true(&mut closed_rx) => {
                            let _ = inner.active_tx.send_replace(None);
                            disconnect_reason = Some("session closed by peer".to_string());
                        }
                    }
                } else {
                    client.close().await;
                    let _ = inner.active_tx.send_replace(None);
                }
            }
            Err(err) => {
                let next_retry = backoff;
                let _ = inner.events_tx.send(LifecycleEvent::ConnectFailed {
                    error: err.to_string(),
                    next_retry,
                });
            }
        }

        if let Some(reason) = disconnect_reason {
            if established {
                let _ = inner
                    .events_tx
                    .send(LifecycleEvent::Disconnected { reason });
            } else {
                let _ = inner.events_tx.send(LifecycleEvent::ConnectFailed {
                    error: reason,
                    next_retry: backoff,
                });
            }
        }

        tokio::select! {
            biased;
            () = wait_true(&mut shutdown_rx) => break,
            () = time::sleep(backoff) => {}
        }
        backoff = policy.next_backoff(backoff);
    }

    let _ = inner.active_tx.send_replace(None);
}
