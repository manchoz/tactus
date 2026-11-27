//! Async Rust client, streaming MessagePack-RPC codec, and virtual serial monitor
//! for [`arduino-router`](https://github.com/arduino/arduino-router),
//! [`Arduino_RouterBridge`](https://github.com/arduino-libraries/Arduino_RouterBridge), and
//! [`Arduino_RPClite`](https://github.com/arduino-libraries/Arduino_RPClite).
//!
//! ## Features
//!
//! - `tokio` *(default)*: enables `Client`, `ManagedClient`, `Handler`, and `MonitorStream`.
//!   Disable default features (`default-features = false`) for the pure runtime-agnostic
//!   [`codec`], [`value`], and [`error`] modules.
//! - `serde`: adds `Serde<T>`, `to_value`, and `from_value` for direct `serde` conversion
//!   without pulling in external `rmp` crates.
//! - `tracing`: emits structured connection and RPC diagnostics via `tracing`.
//!
//! ## Choosing `Client` vs `ManagedClient`
//!
//! - Use `Client` for short-lived CLI tools or single-session connections (`connect_unix_default`,
//!   `connect_tcp`, or `from_stream`).
//! - Use `ManagedClient` for long-running daemons: it maintains exponential reconnect backoff,
//!   replays `$/register` handlers on every connect, retries the session if any `$/register` call
//!   fails, and broadcasts `LifecycleEvent` updates.
//! - Dropping the last `Client` or `ManagedClient` clone automatically closes the session and
//!   stops background tasks (`close().await` / `shutdown().await` are available for explicit
//!   deterministic teardown).
//!
//! ## Wire & Type Rules (`Arduino_RPClite` / `Arduino_RouterBridge`)
//!
//! - **Positional tuple parameters:** `()` sends `0` args, `(arg,)` *(trailing comma)* sends
//!   `1` arg, tuples up to 8 elements are supported, and `Vec<Value>` handles dynamic arities.
//! - **`Bytes` vs `Vec<u8>`:** `Vec<u8>` encodes as a MessagePack `Array` of `u8` (matching
//!   `Arduino_RPClite`), whereas [`Bytes`] encodes as MessagePack `Bin` (required by
//!   `mon/write` and binary RPCs).
//! - **Distinct `f32` (`0xca`) vs `f64` (`0xcb`):** [`Value::F32`] and [`Value::F64`] preserve
//!   exact wire tags because C++ `Arduino_RPClite` rejects `f64` when expecting `float`.
//! - **Handler concurrency & dispatch:** `ClientConfig::max_concurrent_handlers` defaults to
//!   `32`; set `<= 1` for strict FIFO dispatch (re-entrant outbound `call`s inside a handler never
//!   deadlock). Both incoming RPC requests and notifications invoke registered handlers.
//! - **Outbound `Serde<T>` errors:** `Client::call`, `Client::notify`, and `provide` handler
//!   returns propagate serialization failures (for example `i128`/`u128` outside 64-bit bounds)
//!   as `Error::Conversion` or [`ErrorCode::GenericError`].
//!
//! ## Examples
//!
//! ### Async Client (`tokio`, Unix)
//!
//! ```no_run
//! #[cfg(all(feature = "tokio", unix))]
//! async fn example() -> arduino_router::Result<()> {
//!     use arduino_router::Client;
//!
//!     let client = Client::connect_unix_default().await?;
//!     let version = client.version().await?;
//!     let sum: i32 = client.call("add", (20, 22)).await?;
//!     client.notify("led/blink", (true,)).await?;
//!     println!("router={version} sum={sum}");
//!     client.close().await;
//!     Ok(())
//! }
//! ```
//!
//! ### Runtime-Agnostic Codec (`default-features = false`)
//!
//! ```
//! use arduino_router::{decode_frame, encode_frame, CodecLimits, RpcMessage, Value};
//!
//! let frame = RpcMessage::Request {
//!     msgid: 1,
//!     method: "ping".to_string(),
//!     params: vec![Value::Int(42)],
//! };
//! let mut bytes = Vec::new();
//! encode_frame(&frame, &mut bytes).unwrap();
//! let (decoded, consumed) = decode_frame(&bytes, CodecLimits::default()).unwrap();
//! assert_eq!(consumed, bytes.len());
//! assert_eq!(decoded, frame);
//! ```
//!
//! ### `serde` Structs (`features = ["serde"]`)
//!
//! ```
//! #[cfg(feature = "serde")]
//! fn serde_round_trip() {
//!     use arduino_router::{from_value, to_value, Serde};
//!     use serde::{Deserialize, Serialize};
//!
//!     #[derive(Debug, PartialEq, Serialize, Deserialize)]
//!     struct Tempo { bpm: f64, playing: bool }
//!
//!     let val = to_value(&Tempo { bpm: 120.0, playing: true }).unwrap();
//!     let back: Tempo = from_value(val).unwrap();
//!     assert_eq!(back, Tempo { bpm: 120.0, playing: true });
//!     let _param = Serde(back);
//! }
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod codec;
pub mod error;
pub mod value;

#[cfg(feature = "serde")]
pub mod serde_support;

#[cfg(feature = "tokio")]
pub mod client;
#[cfg(feature = "tokio")]
pub mod handler;
#[cfg(feature = "tokio")]
pub mod monitor;
#[cfg(feature = "tokio")]
pub mod reconnect;

#[cfg(feature = "tokio")]
pub(crate) mod log_shim {
    #[cfg(feature = "tracing")]
    macro_rules! router_debug {
        ($($arg:tt)*) => {
            ::tracing::debug!($($arg)*)
        };
    }

    #[cfg(not(feature = "tracing"))]
    macro_rules! router_debug {
        ($($arg:tt)*) => {
            if false {
                let _ = format_args!($($arg)*);
            }
        };
    }

    #[cfg(feature = "tracing")]
    macro_rules! router_warn {
        ($($arg:tt)*) => {
            ::tracing::warn!($($arg)*)
        };
    }

    #[cfg(not(feature = "tracing"))]
    macro_rules! router_warn {
        ($($arg:tt)*) => {
            if false {
                let _ = format_args!($($arg)*);
            }
        };
    }

    pub(crate) use {router_debug, router_warn};
}

#[cfg(feature = "tokio")]
pub(crate) use log_shim::{router_debug, router_warn};

pub use codec::{
    decode_frame, decode_value, encode_frame, encode_frame_checked, encode_value, CodecLimits,
    RpcMessage, DEFAULT_MAX_DECODE_DEPTH, DEFAULT_MAX_FRAME_BYTES,
};
pub use error::{DecodeError, EncodeError, Error, ErrorCode, Result, RpcError};
pub use value::{Bytes, ConversionError, FromParams, FromValue, IntoParams, IntoValue, Value};

#[cfg(feature = "serde")]
pub use serde_support::{from_value, to_value, Serde};

#[cfg(feature = "tokio")]
pub use client::{
    default_unix_socket_path, Client, ClientConfig, DEFAULT_UNIX_SOCKET, MIN_MAX_MSG_SIZE,
    UNIX_SOCKET_ENV,
};
#[cfg(feature = "tokio")]
pub use handler::{BoxFuture, ErasedHandler, Handler, IntoHandlerResult};
#[cfg(feature = "tokio")]
pub use monitor::{MonitorStream, DEFAULT_MONITOR_ADDR};
#[cfg(feature = "tokio")]
pub use reconnect::{
    AsyncStream, BoxStream, Endpoint, LifecycleEvent, ManagedClient, ManagedClientBuilder,
    ReconnectPolicy,
};

/// Built-in router method: register an RPC route (`$/register`).
pub const METHOD_REGISTER: &str = "$/register";
/// Built-in router method: unregister a single RPC route (`$/unregister`).
pub const METHOD_UNREGISTER: &str = "$/unregister";
/// Built-in router method: reset all routes owned by this connection (`$/reset`).
pub const METHOD_RESET: &str = "$/reset";
/// Built-in router method: query router version string (`$/version`).
pub const METHOD_VERSION: &str = "$/version";
/// Built-in router method: set maximum forwarded message size (`$/setMaxMsgSize`).
pub const METHOD_SET_MAX_MSG_SIZE: &str = "$/setMaxMsgSize";
/// Built-in router method: open router serial port (`$/serial/open`).
pub const METHOD_SERIAL_OPEN: &str = "$/serial/open";
/// Built-in router method: close router serial port (`$/serial/close`).
pub const METHOD_SERIAL_CLOSE: &str = "$/serial/close";
/// Built-in monitor method: check if a TCP monitor client is connected (`mon/connected`).
pub const METHOD_MON_CONNECTED: &str = "mon/connected";
/// Built-in monitor method: reset active TCP monitor connections (`mon/reset`).
pub const METHOD_MON_RESET: &str = "mon/reset";
/// Built-in monitor method: read buffered bytes from monitor clients (`mon/read`).
pub const METHOD_MON_READ: &str = "mon/read";
/// Built-in monitor method: write bytes to connected monitor clients (`mon/write`).
pub const METHOD_MON_WRITE: &str = "mon/write";
