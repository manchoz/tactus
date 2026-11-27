# arduino-router

Async Rust client, streaming MessagePack-RPC codec, and virtual serial monitor stream conforming to:

- [`arduino/arduino-router`](https://github.com/arduino/arduino-router) (star-topology MessagePack-RPC daemon, `$/` router methods, `ErrCode` codes, Unix/TCP transports, monitor proxy)
- [`arduino-libraries/Arduino_RouterBridge`](https://github.com/arduino-libraries/Arduino_RouterBridge) (`Bridge` / `Monitor` RPC & TCP monitor semantics)
- [`arduino-libraries/Arduino_RPClite`](https://github.com/arduino-libraries/Arduino_RPClite) (MessagePack-RPC framing, `[code, traceback]` wire errors, `0x00` / `0xFB..=0xFF` error codes, parameter type validation)

> **Disclaimer:** This is an unofficial community crate and is not affiliated with or endorsed by Arduino s.r.l.

## Installation

```sh
cargo add arduino-router
```

Or in `Cargo.toml`:

```toml
# Default (async Tokio client + codec + monitor)
arduino-router = "0.1"

# With serde and structured tracing diagnostics
arduino-router = { version = "0.1", features = ["serde", "tracing"] }

# Pure runtime-agnostic codec/value/error only (no Tokio)
arduino-router = { version = "0.1", default-features = false }
```

## Choosing `Client` vs `ManagedClient`

| Type | Best For | Behavior |
| :--- | :--- | :--- |
| `Client` | CLI tools, one-off scripts, tests (`from_stream`) | Single session over Unix (`/var/run/arduino-router.sock` or `$ARDUINO_ROUTER_SOCKET`) or TCP. |
| `ManagedClient` | Daemons, long-lived services | Auto-reconnects with exponential backoff (`ReconnectPolicy`), replays `$/register` on every connect, retries the session if any `$/register` fails, and emits `LifecycleEvent`. |

Dropping the last `Client` or `ManagedClient` clone automatically closes the session and stops background tasks (`close().await` / `shutdown().await` are optional for explicit teardown).

## Quick Start

```rust
use arduino_router::{Client, Result};

#[tokio::main]
async fn main() -> Result<()> {
    // Connects to ARDUINO_ROUTER_SOCKET or /var/run/arduino-router.sock
    let client = Client::connect_unix_default().await?;

    let version = client.version().await?;
    println!("arduino-router version: {version}");

    let sum: i32 = client.call("add", (20, 22)).await?;
    println!("20 + 22 = {sum}");

    client.notify("led/blink", (true,)).await?;
    client.close().await;
    Ok(())
}
```

### Exposing Typed Callbacks (`$/register`)

```rust
use arduino_router::{Client, Result, RpcError};

async fn expose_handlers(client: &Client) -> Result<()> {
    client
        .provide("math/div", |a: f64, b: f64| async move {
            if b == 0.0 {
                Err(RpcError::invalid_params("division by zero"))
            } else {
                Ok(a / b)
            }
        })
        .await?;
    Ok(())
}
```

### Auto-Reconnecting Client (`ManagedClient`)

```rust
use arduino_router::{LifecycleEvent, ManagedClient, ReconnectPolicy};

async fn run_managed() {
    let managed = ManagedClient::builder_unix("/var/run/arduino-router.sock")
        .reconnect_policy(ReconnectPolicy::default())
        .provide("host/ping", || async { "pong" })
        .spawn();

    let mut events = managed.subscribe_events();
    tokio::spawn(async move {
        while let Ok(ev) = events.recv().await {
            if let LifecycleEvent::Connected = ev {
                println!("Connected and handlers registered!");
            }
        }
    });

    if let Ok(client) = managed.wait_connected().await {
        let _ = client.notify("host/ready", ()).await;
    }

    managed.shutdown().await;
}
```

### Virtual Serial Monitor (`MonitorStream` + `mon/*`)

```rust
use arduino_router::{Client, MonitorStream};

async fn monitor_example(client: &Client) -> std::io::Result<()> {
    let mut mon = MonitorStream::connect_default().await?; // 127.0.0.1:7500
    mon.write_str("hello mcu\n").await?;

    let mut buf = [0u8; 256];
    let n = mon.read_bytes(&mut buf).await?;
    println!("MCU printed: {:?}", &buf[..n]);

    let _connected = client.monitor_connected().await;
    Ok(())
}
```

### Using `Serde<T>` (`features = ["serde"]`)

```rust
use arduino_router::{Client, Result, Serde};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
struct Tempo {
    bpm: f64,
    playing: bool,
}

async fn send_tempo(client: &Client) -> Result<()> {
    // Pass 1-arg Serde<T> as a 1-tuple `(Serde(value),)`.
    // Serialization errors (e.g., u128 > u64::MAX) return Err(Error::Conversion(_)).
    let ack: Serde<Tempo> = client
        .call("mcu/set_tempo", (Serde(Tempo { bpm: 120.0, playing: true }),))
        .await?;
    println!("MCU tempo: {:?}", ack.0);
    Ok(())
}
```

### Runtime-Agnostic Codec (`default-features = false`)

```rust
use arduino_router::{decode_frame, encode_frame, CodecLimits, RpcMessage, Value};

let frame = RpcMessage::Notification {
    method: "ping".to_string(),
    params: vec![Value::Int(1)],
};
let mut buf = Vec::new();
encode_frame(&frame, &mut buf).unwrap();
let (decoded, consumed) = decode_frame(&buf, CodecLimits::default()).unwrap();
assert_eq!(consumed, buf.len());
```

## Wire & Type Gotchas (Humans & AI Agents)

- **1-argument tuples need a trailing comma:** use `()` for 0 args, `(arg,)` for 1 arg, up to 8-tuples, or `Vec<Value>` for dynamic/more args.
- **`Bytes` vs `Vec<u8>`:** `Vec<u8>` encodes as a MessagePack `Array` of `u8` (for `Arduino_RPClite`), whereas `Bytes` encodes as MessagePack `Bin` (required by `mon/write` and binary RPCs).
- **Strict `f32` (`0xca`) vs `f64` (`0xcb`):** C++ `Arduino_RPClite` rejects `f64` when a sketch method expects `float` with `ErrorCode::MalformedCall` (`253`). Pass `f32` / `Value::F32` when targeting C++ `float`.
- **Handler concurrency & re-entrancy:** `ClientConfig::max_concurrent_handlers` defaults to `32`; set `<= 1` for strict FIFO order (re-entrant `client.call(...)` inside a handler never deadlocks). Both requests and notifications invoke registered handlers.
- **MCU UART bandwidth:** Keep high-rate clocks or sub-millisecond telemetry off the router socket—the underlying MCU link is typically a UART serial bridge.

## Feature Flags

| Feature | Default | Description |
| :--- | :--- | :--- |
| `tokio` | Yes | Async `Client`, `ManagedClient`, `Handler`, and `MonitorStream`. Disable default features for the runtime-agnostic `codec`, `value`, and `error` modules only. |
| `serde` | No | Zero-extra-dependency `serde` conversion via `Serde<T>`, `to_value`, and `from_value`. |
| `tracing` | No | Emits structured connection/RPC diagnostics through the `tracing` crate. |

## Running Examples

```sh
cargo run -p arduino-router --example basic_client
cargo run -p arduino-router --example provide_handler
cargo run -p arduino-router --example monitor
```

## Protocol Conformance Summary

- **MessagePack-RPC Frames:** Request `[0, msgid, method, params]`, Response `[1, msgid, error, result]`, Notification `[2, method, params]`, compact integer encoding, distinct `Value::F32` (`0xca`) and `Value::F64` (`0xcb`) wire fidelity.
- **Router Control Methods (`$/`):** `$/register`, `$/unregister`, `$/reset`, `$/version`, `$/setMaxMsgSize` (both request and notification, enforcing `>= 128`), `$/serial/open`, `$/serial/close`.
- **Error Codes (`[code, message]`):**
  - `1..=6`: `InvalidParams`, `MethodNotAvailable`, `FailedToSendRequest`, `RouterGenericError`, `RouteAlreadyExists`, `BufferLimitExceeded`.
  - `0x00` & `0xFB..=0xFF`: `NoError` (`0`), `Timeout` (`251`), `ParsingError` (`252`), `MalformedCall` (`253`), `FunctionNotFound` (`254`), `GenericError` (`255`).
- **Monitor API:** `MonitorStream` (`127.0.0.1:7500`) plus `mon/connected`, `mon/read`, `mon/write`, `mon/reset`.

## License

Dual-licensed under either of:

- MIT License ([LICENSE-MIT](https://github.com/manchoz/tactus/blob/main/crates/arduino-router/LICENSE-MIT))
- Apache License, Version 2.0 ([LICENSE-APACHE](https://github.com/manchoz/tactus/blob/main/crates/arduino-router/LICENSE-APACHE))

at your option.
