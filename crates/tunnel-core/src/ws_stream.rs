use bytes::BytesMut;
use futures_util::{SinkExt, StreamExt};
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message;

/// Adapts a tokio-tungstenite WebSocket into a futures::AsyncRead + AsyncWrite byte stream.
/// yamux requires futures-io traits (not tokio-io traits).
///
/// Ported from astral-relay/src/ws_stream.rs — same logic, different WebSocket type:
/// - Relay uses axum::extract::ws::WebSocket (server-side)
/// - Desktop uses tokio_tungstenite::WebSocketStream (client-side)
pub struct WsStream<S> {
    inner: Mutex<WsStreamInner<S>>,
}

struct WsStreamInner<S> {
    ws: S,
    read_buf: BytesMut,
    closed: bool,
}

impl<S> WsStream<S> {
    pub fn new(ws: S) -> Self {
        Self {
            inner: Mutex::new(WsStreamInner {
                ws,
                read_buf: BytesMut::new(),
                closed: false,
            }),
        }
    }
}

impl<S> futures_util::AsyncRead for WsStream<S>
where
    S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let mut inner = match self.inner.try_lock() {
            Ok(guard) => guard,
            Err(_) => {
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
        };

        // Return buffered data first
        if !inner.read_buf.is_empty() {
            let len = std::cmp::min(buf.len(), inner.read_buf.len());
            buf[..len].copy_from_slice(&inner.read_buf.split_to(len));
            return Poll::Ready(Ok(len));
        }

        if inner.closed {
            return Poll::Ready(Ok(0));
        }

        match inner.ws.poll_next_unpin(cx) {
            Poll::Ready(Some(Ok(msg))) => match msg {
                Message::Binary(data) => {
                    let len = std::cmp::min(buf.len(), data.len());
                    buf[..len].copy_from_slice(&data[..len]);
                    if len < data.len() {
                        inner.read_buf.extend_from_slice(&data[len..]);
                    }
                    Poll::Ready(Ok(len))
                }
                Message::Close(_) => {
                    inner.closed = true;
                    Poll::Ready(Ok(0))
                }
                Message::Ping(_) | Message::Pong(_) | Message::Text(_) | Message::Frame(_) => {
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
            },
            Poll::Ready(Some(Err(e))) => {
                inner.closed = true;
                Poll::Ready(Err(io::Error::new(io::ErrorKind::ConnectionReset, e)))
            }
            Poll::Ready(None) => {
                inner.closed = true;
                Poll::Ready(Ok(0))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<S> futures_util::AsyncWrite for WsStream<S>
where
    S: SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let mut inner = match self.inner.try_lock() {
            Ok(guard) => guard,
            Err(_) => {
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
        };

        if inner.closed {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "WebSocket closed",
            )));
        }

        match inner.ws.poll_ready_unpin(cx) {
            Poll::Ready(Ok(())) => {
                let data = buf.to_vec();
                let len = data.len();
                match inner.ws.start_send_unpin(Message::Binary(data.into())) {
                    Ok(()) => Poll::Ready(Ok(len)),
                    Err(e) => {
                        inner.closed = true;
                        Poll::Ready(Err(io::Error::new(io::ErrorKind::BrokenPipe, e)))
                    }
                }
            }
            Poll::Ready(Err(e)) => {
                inner.closed = true;
                Poll::Ready(Err(io::Error::new(io::ErrorKind::BrokenPipe, e)))
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut inner = match self.inner.try_lock() {
            Ok(guard) => guard,
            Err(_) => {
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
        };
        inner
            .ws
            .poll_flush_unpin(cx)
            .map_err(|e| io::Error::new(io::ErrorKind::BrokenPipe, e))
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut inner = match self.inner.try_lock() {
            Ok(guard) => guard,
            Err(_) => {
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
        };
        inner.closed = true;
        inner
            .ws
            .poll_close_unpin(cx)
            .map_err(|e| io::Error::new(io::ErrorKind::BrokenPipe, e))
    }
}

// Safety: WsStream uses Mutex internally for thread safety
unsafe impl<S: Send> Send for WsStream<S> {}
unsafe impl<S: Send> Sync for WsStream<S> {}
