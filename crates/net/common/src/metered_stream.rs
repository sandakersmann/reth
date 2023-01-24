//! Support for metering ingress / egress over streams. Takes heavy inspiration from https://github.com/libp2p/rust-libp2p/blob/master/src/bandwidth.rs

// Copyright 2019 Parity Technologies (UK) Ltd.
//
// Permission is hereby granted, free of charge, to any person obtaining a
// copy of this software and associated documentation files (the "Software"),
// to deal in the Software without restriction, including without limitation
// the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the
// Software is furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS
// OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
// FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
// DEALINGS IN THE SOFTWARE.

use crate::stream::HasRemoteAddr;
use metrics::Counter;
use reth_metrics_derive::Metrics;
use std::{
    convert::TryFrom as _,
    io,
    net::SocketAddr,
    pin::Pin,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    task::{ready, Context, Poll},
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::TcpStream,
};

/// Meters ingress & egress of streams
#[derive(Debug)]
struct MeteredStreamCountsInner {
    /// Measures the number of inbound bytes
    ingress: AtomicU64,
    /// Measures the number of outbound bytes
    egress: AtomicU64,
}

/// Public shareable struct used for getting stream ingress/egress info.
/// Can be shared between multiple [`MeteredStream`]s to aggregate
/// ingress/egress measurements across all of them.
#[derive(Clone, Debug)]
pub struct MeteredStreamCounts {
    inner: Arc<MeteredStreamCountsInner>,
}

impl MeteredStreamCounts {
    /// Returns the total number of bytes that have been downloaded on all the streams.
    ///
    /// > **Note**: This method is by design subject to race conditions. The returned value should
    /// > only ever be used for statistics purposes.
    pub fn total_ingress(&self) -> u64 {
        self.inner.ingress.load(Ordering::Relaxed)
    }

    /// Returns the total number of bytes that have been uploaded on all the streams.
    ///
    /// > **Note**: This method is by design subject to race conditions. The returned value should
    /// > only ever be used for statistics purposes.
    pub fn total_egress(&self) -> u64 {
        self.inner.egress.load(Ordering::Relaxed)
    }
}

impl Default for MeteredStreamCounts {
    fn default() -> Self {
        Self {
            inner: Arc::new(MeteredStreamCountsInner {
                ingress: AtomicU64::new(0),
                egress: AtomicU64::new(0),
            }),
        }
    }
}

/// Exposes metrics for the ingress and egress on a given
/// metered stream
#[derive(Metrics)]
#[metrics(dynamic = true)]
struct MeteredStreamMetricsInner {
    /// Counts inbound bytes
    ingress_bytes: Counter,
    /// Counts outbound bytes
    egress_bytes: Counter,
}

/// Public shareable struct used for metered stream metrics
#[derive(Debug)]
pub struct MeteredStreamMetrics {
    inner: Arc<MeteredStreamMetricsInner>,
}

impl MeteredStreamMetrics {
    /// Creates an instance of  [`MeteredStreamMetrics`] with the given scope
    pub fn new(scope: &str, labels: impl metrics::IntoLabels + Clone) -> Self {
        Self { inner: Arc::new(MeteredStreamMetricsInner::new_with_labels(scope, labels)) }
    }
}

/// Wraps around a single stream that implements [`AsyncRead`] + [`AsyncWrite`] and meters the
/// ingress/egress through it
#[derive(Debug)]
#[pin_project::pin_project]
pub struct MeteredStream<S> {
    /// The stream this instruments
    #[pin]
    inner: S,
    /// The [`MeteredStreamCounts`] struct this uses to meter ingress / egress
    meter: MeteredStreamCounts,
    /// An optional  [`MeteredStreamMetrics`] struct expose metrics over the
    /// [`MeteredStreamCounts`].
    metrics: Option<MeteredStreamMetrics>,
}

impl<S> MeteredStream<S> {
    /// Creates a new [`MeteredStream`] wrapping around the provided stream,
    /// along with a new [`MeteredStreamCounts`]
    pub fn new(inner: S) -> Self {
        Self { inner, meter: MeteredStreamCounts::default(), metrics: None }
    }

    /// Attaches the provided [`MeteredStreamCounts`]
    pub fn set_meter(&mut self, meter: MeteredStreamCounts) {
        self.meter = meter;
    }

    /// Attaches the provided  [`MeteredStreamMetrics`]
    pub fn set_metrics(&mut self, metrics: MeteredStreamMetrics) {
        self.metrics = Some(metrics);
    }

    /// Provides a reference to the [`MeteredStreamCounts`] attached to this [`MeteredStream`]
    pub fn get_metered_stream_counts(&self) -> &MeteredStreamCounts {
        &self.meter
    }
}

impl<S> AsMut<MeteredStream<S>> for MeteredStream<S> {
    fn as_mut(&mut self) -> &mut MeteredStream<S> {
        self
    }
}

impl<Stream: AsyncRead> AsyncRead for MeteredStream<Stream> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.project();
        let num_bytes_u64 = {
            let init_num_bytes = buf.filled().len();
            ready!(this.inner.poll_read(cx, buf))?;
            u64::try_from(buf.filled().len() - init_num_bytes).unwrap_or(u64::max_value())
        };
        let current_ingress =
            this.meter.inner.ingress.fetch_add(num_bytes_u64, Ordering::Relaxed) + num_bytes_u64;

        if let Some(metered_stream_metrics) = &this.metrics {
            metered_stream_metrics.inner.ingress_bytes.absolute(current_ingress);
        }

        Poll::Ready(Ok(()))
    }
}

impl<Stream: AsyncWrite> AsyncWrite for MeteredStream<Stream> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.project();
        let num_bytes = ready!(this.inner.poll_write(cx, buf))?;
        let num_bytes_u64 = { u64::try_from(num_bytes).unwrap_or(u64::max_value()) };
        let current_egress =
            this.meter.inner.egress.fetch_add(num_bytes_u64, Ordering::Relaxed) + num_bytes_u64;

        if let Some(metered_stream_metrics) = &this.metrics {
            metered_stream_metrics.inner.egress_bytes.absolute(current_egress);
        }

        Poll::Ready(Ok(num_bytes))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.project();
        this.inner.poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.project();
        this.inner.poll_shutdown(cx)
    }
}

impl HasRemoteAddr for MeteredStream<TcpStream> {
    fn remote_addr(&self) -> Option<SocketAddr> {
        self.inner.remote_addr()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{
        io::{duplex, AsyncReadExt, AsyncWriteExt, DuplexStream},
        net::{TcpListener, TcpStream},
    };

    fn create_metered_duplex<'a>(
        client_meter: MeteredStreamCounts,
        server_meter: MeteredStreamCounts,
    ) -> (MeteredStream<DuplexStream>, MeteredStream<DuplexStream>) {
        let (client, server) = duplex(64);
        let (mut metered_client, mut metered_server) =
            (MeteredStream::new(client), MeteredStream::new(server));

        metered_client.set_meter(client_meter);
        metered_server.set_meter(server_meter);

        (metered_client, metered_server)
    }

    async fn duplex_stream_ping_pong(
        client: &mut MeteredStream<DuplexStream>,
        server: &mut MeteredStream<DuplexStream>,
    ) {
        let mut buf = [0u8; 4];

        client.write_all(b"ping").await.unwrap();
        server.read(&mut buf).await.unwrap();

        server.write_all(b"pong").await.unwrap();
        client.read(&mut buf).await.unwrap();
    }

    fn assert_io_counts(
        metered_stream_counts: &MeteredStreamCounts,
        expected_ingress: u64,
        expected_egress: u64,
    ) {
        let actual_ingress = metered_stream_counts.total_ingress();
        assert_eq!(
            actual_ingress, expected_ingress,
            "Expected {expected_ingress} inbound bytes, but got {actual_ingress}",
        );

        let actual_egress = metered_stream_counts.total_egress();
        assert_eq!(
            actual_egress, expected_egress,
            "Expected {expected_egress} inbound bytes, but got {actual_egress}",
        );
    }

    #[tokio::test]
    async fn test_count_read_write() {
        // Taken in large part from https://docs.rs/tokio/latest/tokio/io/struct.DuplexStream.html#example

        let (mut metered_client, mut metered_server) =
            create_metered_duplex(MeteredStreamCounts::default(), MeteredStreamCounts::default());

        duplex_stream_ping_pong(&mut metered_client, &mut metered_server).await;

        assert_io_counts(metered_client.get_metered_stream_counts(), 4, 4);
        assert_io_counts(metered_server.get_metered_stream_counts(), 4, 4);
    }

    #[tokio::test]
    async fn test_read_equals_write_tcp() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server_addr = listener.local_addr().unwrap();

        let client_stream = TcpStream::connect(server_addr).await.unwrap();
        let mut metered_client_stream = MeteredStream::new(client_stream);

        let client_meter = metered_client_stream.meter.clone();

        let handle = tokio::spawn(async move {
            let (server_stream, _) = listener.accept().await.unwrap();
            let mut metered_server_stream = MeteredStream::new(server_stream);

            let mut buf = [0u8; 4];

            metered_server_stream.read(&mut buf).await.unwrap();

            assert_eq!(metered_server_stream.meter.total_ingress(), client_meter.total_egress());
        });

        metered_client_stream.write_all(b"ping").await.unwrap();

        handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_multiple_streams_one_meter() {
        let shared_client_counts = MeteredStreamCounts::default();
        let shared_server_counts = MeteredStreamCounts::default();

        let (mut metered_client_1, mut metered_server_1) =
            create_metered_duplex(shared_client_counts.clone(), shared_server_counts.clone());
        let (mut metered_client_2, mut metered_server_2) =
            create_metered_duplex(shared_client_counts.clone(), shared_server_counts.clone());

        duplex_stream_ping_pong(&mut metered_client_1, &mut metered_server_1).await;
        duplex_stream_ping_pong(&mut metered_client_2, &mut metered_server_2).await;

        assert_io_counts(&shared_client_counts, 8, 8);
        assert_io_counts(&shared_server_counts, 8, 8);
    }
}