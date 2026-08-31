use futures::{
    Sink, Stream, StreamExt,
    stream::{SplitSink, SplitStream},
};
use http::Uri;
use std::{
    pin::Pin,
    task::{Context, Poll, ready},
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpStream,
};
use url::Url;
use yawc::{MaybeTlsStream, Options, WebSocket, WebSocketError, frame::Frame};

pub use tokio_rustls::TlsConnector;

/// A websocket connection builder. Takes a URL and returns a pair of
/// channels representing the sending and receiving side of the the
/// connection.
pub trait Connector {
    type Sender: Sink<Frame, Error = WebSocketError>;
    type Receiver: Stream<Item = Result<Frame, WebSocketError>>;

    /// Open a connection to `url`.
    ///
    /// `disable_nagle` overrides [`Options::no_delay`], which is where yawc
    /// keeps the same setting.
    fn connect(
        &mut self,
        options: Option<Options>,
        url: &Uri,
        disable_nagle: bool,
    ) -> impl std::future::Future<Output = Result<(Self::Sender, Self::Receiver), WebSocketError>> + Send;
}

/// Adapts a [`WebSocket`] into a `Stream` of `Result<Frame, WebSocketError>`.
///
/// yawc's own `Stream` implementation reports a read error as end-of-stream,
/// which would hide why a connection dropped. Driving `poll_next_frame`
/// directly keeps the error so it can be forwarded to the application through
/// `NewConnection::on_close`.
pub struct FrameStream<S> {
    inner: WebSocket<S>,
    /// Set once an error has been yielded; the connection is unusable
    /// afterwards, so the stream ends rather than polling the socket again.
    finished: bool,
}

impl<S> FrameStream<S> {
    pub fn new(inner: WebSocket<S>) -> Self {
        Self {
            inner,
            finished: false,
        }
    }

    pub fn into_inner(self) -> WebSocket<S> {
        self.inner
    }
}

impl<S> Stream for FrameStream<S>
where
    S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    type Item = Result<Frame, WebSocketError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        if this.finished {
            return Poll::Ready(None);
        }

        match ready!(this.inner.poll_next_frame(cx)) {
            Ok(frame) => Poll::Ready(Some(Ok(frame))),
            Err(e) => {
                this.finished = true;
                Poll::Ready(Some(Err(e)))
            }
        }
    }
}

impl<S> Sink<Frame> for FrameStream<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    type Error = WebSocketError;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.get_mut().inner).poll_ready(cx)
    }

    fn start_send(self: Pin<&mut Self>, item: Frame) -> Result<(), Self::Error> {
        Pin::new(&mut self.get_mut().inner).start_send(item)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.get_mut().inner).poll_close(cx)
    }
}

/// A WebSocket connection.
#[derive(Clone, Default)]
pub struct WebSocketBuilder {
    connector: Option<TlsConnector>,
}

impl WebSocketBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Use a custom rustls connector for `wss://` connections, for instance
    /// one built from the platform's native root certificates. Without one,
    /// yawc uses its bundled webpki roots.
    pub fn with_connector(mut self, connector: TlsConnector) -> Self {
        self.connector = Some(connector);
        self
    }
}

impl Connector for WebSocketBuilder {
    type Sender = SplitSink<FrameStream<MaybeTlsStream<TcpStream>>, Frame>;
    type Receiver = SplitStream<FrameStream<MaybeTlsStream<TcpStream>>>;

    async fn connect(
        &mut self,
        options: Option<Options>,
        url: &Uri,
        disable_nagle: bool,
    ) -> Result<(Self::Sender, Self::Receiver), WebSocketError> {
        let url = Url::parse(&url.to_string())?;

        let mut options = options.unwrap_or_default();
        options.no_delay = disable_nagle;

        let mut builder = WebSocket::connect(url).with_options(options);

        if let Some(connector) = self.connector.clone() {
            builder = builder.with_connector(connector);
        }

        Ok(FrameStream::new(builder.await?).split())
    }
}
