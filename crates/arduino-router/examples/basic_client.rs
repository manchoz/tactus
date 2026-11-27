use arduino_router::{Client, Result};

#[tokio::main]
async fn main() -> Result<()> {
    #[cfg(unix)]
    let client = Client::connect_unix_default().await?;
    #[cfg(not(unix))]
    let client = Client::connect_tcp("127.0.0.1:8800").await?;

    let version = client.version().await?;
    println!("Connected to arduino-router {version}");

    let pong: String = client.call("ping", ()).await?;
    println!("ping -> {pong}");

    client.notify("status/led", (true,)).await?;
    client.close().await;
    Ok(())
}
