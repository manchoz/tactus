use arduino_router::{Client, MonitorStream, Result};

#[tokio::main]
async fn main() -> Result<()> {
    #[cfg(unix)]
    let client = Client::connect_unix_default().await?;
    #[cfg(not(unix))]
    let client = Client::connect_tcp("127.0.0.1:8800").await?;

    let mut mon = MonitorStream::connect_default().await?;
    println!("Monitor connected? {}", client.monitor_connected().await?);

    mon.write_str("hello from rust\n").await?;

    let mut buf = [0u8; 256];
    let n = mon.read_bytes(&mut buf).await?;
    println!("Read {} byte(s) from MCU monitor", n);

    client.close().await;
    Ok(())
}
