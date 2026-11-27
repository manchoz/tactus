//! Virtual Serial Monitor TCP stream client (`127.0.0.1:7500` by default).
//!
//! `arduino-router` exposes a TCP monitor proxy (via `-m` / `127.0.0.1:7500`) that bridges
//! the MCU's `Monitor.print` / `Monitor.read` (`mon/write` / `mon/read` RPCs) to plain TCP
//! byte streams.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::{TcpStream, ToSocketAddrs};

/// Default TCP address where `arduino-router` exposes the MCU monitor proxy.
pub const DEFAULT_MONITOR_ADDR: &str = "127.0.0.1:7500";

/// Async client stream connected to the `arduino-router` virtual serial monitor proxy.
pub struct MonitorStream<S = TcpStream> {
    stream: S,
}

impl MonitorStream<TcpStream> {
    /// Connect to the default `arduino-router` monitor proxy at [`DEFAULT_MONITOR_ADDR`].
    pub async fn connect_default() -> io::Result<Self> {
        Self::connect(DEFAULT_MONITOR_ADDR).await
    }

    /// Connect to an `arduino-router` monitor proxy at `addr`.
    pub async fn connect(addr: impl ToSocketAddrs) -> io::Result<Self> {
        let stream = TcpStream::connect(addr).await?;
        let _ = stream.set_nodelay(true);
        Ok(Self { stream })
    }
}

impl<S> MonitorStream<S> {
    /// Wrap any `AsyncRead + AsyncWrite` stream as a [`MonitorStream`].
    #[must_use]
    pub const fn from_stream(stream: S) -> Self {
        Self { stream }
    }

    /// Borrow the underlying stream.
    #[must_use]
    pub const fn get_ref(&self) -> &S {
        &self.stream
    }

    /// Mutably borrow the underlying stream.
    #[must_use]
    pub fn get_mut(&mut self) -> &mut S {
        &mut self.stream
    }

    /// Consume the wrapper and return the underlying stream.
    #[must_use]
    pub fn into_inner(self) -> S {
        self.stream
    }
}

impl<S: AsyncRead + Unpin> MonitorStream<S> {
    /// Read available monitor bytes into `buf` (returns `0` on EOF).
    pub async fn read_bytes(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.stream.read(buf).await
    }
}

impl<S: AsyncWrite + Unpin> MonitorStream<S> {
    /// Write all bytes to the monitor stream and flush.
    pub async fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        self.stream.write_all(data).await?;
        self.stream.flush().await
    }

    /// Write a UTF-8 string slice to the monitor stream and flush.
    pub async fn write_str(&mut self, s: &str) -> io::Result<()> {
        self.write_all(s.as_bytes()).await
    }

    /// Flush buffered monitor output.
    pub async fn flush(&mut self) -> io::Result<()> {
        self.stream.flush().await
    }

    /// Shut down the write half of the monitor stream.
    pub async fn shutdown(&mut self) -> io::Result<()> {
        self.stream.shutdown().await
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for MonitorStream<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for MonitorStream<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}
