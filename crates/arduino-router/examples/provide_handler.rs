use std::time::Duration;

use arduino_router::{LifecycleEvent, ManagedClient, ReconnectPolicy, RpcError};

#[tokio::main]
async fn main() {
    #[cfg(unix)]
    let builder = ManagedClient::builder_unix("/var/run/arduino-router.sock");
    #[cfg(not(unix))]
    let builder = ManagedClient::builder_tcp("127.0.0.1:8800");

    let managed = builder
        .reconnect_policy(ReconnectPolicy::default())
        .provide("math/add", |a: i32, b: i32| async move { a + b })
        .provide("math/div", |a: f64, b: f64| async move {
            if b == 0.0 {
                Err(RpcError::invalid_params("division by zero"))
            } else {
                Ok(a / b)
            }
        })
        .spawn();

    let mut events = managed.subscribe_events();
    let monitor = tokio::spawn(async move {
        while let Ok(ev) = events.recv().await {
            match ev {
                LifecycleEvent::Connected => println!("Bridge connected and routes registered"),
                LifecycleEvent::Disconnected { reason } => println!("Disconnected: {reason}"),
                LifecycleEvent::ConnectFailed { error, next_retry } => {
                    println!("Connect failed ({error}); retrying in {next_retry:?}");
                }
                LifecycleEvent::RegisterFailed { method, error } => {
                    eprintln!("Failed registering `{method}`: {error}");
                }
            }
        }
    });

    tokio::time::sleep(Duration::from_secs(1)).await;
    managed.shutdown().await;
    let _ = monitor.await;
}
