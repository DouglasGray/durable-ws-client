//! Behaviour of the reconnect and dispatch loop, driven by an in-memory
//! [`Connector`] so the client can be exercised frame by frame without a
//! socket in the way.

use std::{
    collections::VecDeque,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::Duration,
};

use bytes::Bytes;
use durable_ws_client::{
    client::{ConnectError, Message, NewConnection, ReconnectingClient},
    config::{Config, ConfigBuilder},
    connection::Connector,
};
use futures::{Sink, Stream};
use http::Uri;
use tokio::sync::mpsc;
use tower::retry::backoff::ExponentialBackoffMaker;
use yawc::{
    Options, WebSocketError,
    close::CloseCode,
    frame::{Frame, OpCode},
};

/// What the connector does on the next connection attempt.
enum Attempt {
    /// Refuse the connection.
    Fail(WebSocketError),
    /// Never resolve, so the client's connect timeout fires.
    Hang,
    /// Connect, publishing the server end of the connection to the test.
    Serve,
}

/// The server end of a mock connection.
struct Server {
    to_client: Option<mpsc::Sender<Result<Frame, WebSocketError>>>,
    from_client: mpsc::UnboundedReceiver<Frame>,
}

impl Server {
    async fn send(&self, frame: Frame) {
        self.to_client
            .as_ref()
            .expect("connection is still open")
            .send(Ok(frame))
            .await
            .expect("client is still reading");
    }

    /// Surface a read error, as a corrupt or oversized frame would.
    async fn fail(&self, e: WebSocketError) {
        self.to_client
            .as_ref()
            .expect("connection is still open")
            .send(Err(e))
            .await
            .expect("client is still reading");
    }

    /// End the stream, as a socket reaching EOF would.
    fn disconnect(&mut self) {
        self.to_client = None;
    }

    /// The next frame the client wrote.
    async fn next_frame(&mut self) -> Option<Frame> {
        self.from_client.recv().await
    }
}

#[derive(Clone)]
struct MockConnector {
    attempts: Arc<Mutex<VecDeque<Attempt>>>,
    connections: mpsc::UnboundedSender<Server>,
}

impl MockConnector {
    /// Programs one outcome per connection attempt. Once they run out the
    /// connector hangs, so a client that reconnects more often than a test
    /// expects parks instead of spinning.
    fn new(attempts: impl IntoIterator<Item = Attempt>) -> (Self, mpsc::UnboundedReceiver<Server>) {
        let (connections, servers) = mpsc::unbounded_channel();

        let connector = Self {
            attempts: Arc::new(Mutex::new(attempts.into_iter().collect())),
            connections,
        };

        (connector, servers)
    }
}

impl Connector for MockConnector {
    type Sender = MockSink;
    type Receiver = MockStream;

    async fn connect(
        &mut self,
        _options: Option<Options>,
        _url: &Uri,
        _disable_nagle: bool,
    ) -> Result<(Self::Sender, Self::Receiver), WebSocketError> {
        let attempt = {
            let mut attempts = self.attempts.lock().unwrap();
            attempts.pop_front()
        };

        match attempt.unwrap_or(Attempt::Hang) {
            Attempt::Fail(e) => Err(e),
            Attempt::Hang => std::future::pending().await,
            Attempt::Serve => {
                let (to_client, from_server) = mpsc::channel(8);
                let (to_server, from_client) = mpsc::unbounded_channel();

                self.connections
                    .send(Server {
                        to_client: Some(to_client),
                        from_client,
                    })
                    .ok();

                Ok((MockSink { to_server }, MockStream(from_server)))
            }
        }
    }
}

struct MockSink {
    to_server: mpsc::UnboundedSender<Frame>,
}

impl Sink<Frame> for MockSink {
    type Error = WebSocketError;

    fn poll_ready(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn start_send(self: Pin<&mut Self>, item: Frame) -> Result<(), Self::Error> {
        self.to_server
            .send(item)
            .map_err(|_| WebSocketError::ConnectionClosed)
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }
}

struct MockStream(mpsc::Receiver<Result<Frame, WebSocketError>>);

impl Stream for MockStream {
    type Item = Result<Frame, WebSocketError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.0.poll_recv(cx)
    }
}

/// Runs a client against `connector` until the returned receiver is dropped.
fn spawn_client(
    connector: MockConnector,
    config: Config,
) -> mpsc::Receiver<Result<NewConnection, ConnectError>> {
    let (client, connections) = ReconnectingClient::new();

    tokio::spawn(async move {
        client
            .connect(
                config,
                Uri::from_static("ws://mock.invalid/"),
                connector,
                None::<ExponentialBackoffMaker>,
            )
            .await;
    });

    connections
}

/// Starts a client that connects once, and returns both ends of that
/// connection.
async fn connected() -> (Server, NewConnection) {
    let (connector, mut servers) = MockConnector::new([Attempt::Serve]);
    let mut connections = spawn_client(connector, Config::default());

    let connection = connections
        .recv()
        .await
        .expect("client is running")
        .expect("connection succeeded");

    // Hold the receiver open for as long as the connection lives, so the
    // client's loop is not torn down mid-test.
    Box::leak(Box::new(connections));

    (servers.recv().await.expect("server end"), connection)
}

#[tokio::test]
async fn text_payloads_reach_the_application_without_being_copied() {
    let payload = Bytes::from_static(br#"{"table":"trade"}"#);
    let address = payload.as_ptr();

    let (server, connection) = connected().await;
    let (_to_ws, mut from_ws) = connection.channels;

    server.send(Frame::text(payload)).await;

    let Some(Message::Text(text)) = from_ws.recv().await else {
        panic!("expected a text message");
    };

    assert_eq!(text.as_str(), r#"{"table":"trade"}"#);
    assert_eq!(
        text.as_bytes().as_ptr(),
        address,
        "text payload was copied on its way to the application"
    );
}

#[tokio::test]
async fn binary_payloads_reach_the_application_without_being_copied() {
    let payload = Bytes::from_static(&[0x00, 0x01, 0x02]);
    let address = payload.as_ptr();

    let (server, connection) = connected().await;
    let (_to_ws, mut from_ws) = connection.channels;

    server.send(Frame::binary(payload)).await;

    let Some(Message::Binary(data)) = from_ws.recv().await else {
        panic!("expected a binary message");
    };

    assert_eq!(&data[..], &[0x00, 0x01, 0x02]);
    assert_eq!(
        data.as_ptr(),
        address,
        "binary payload was copied on its way to the application"
    );
}

#[tokio::test]
async fn application_payloads_reach_the_socket_without_being_copied() {
    let payload = Bytes::from_static(b"subscribe");
    let address = payload.as_ptr();

    let (mut server, connection) = connected().await;
    let (to_ws, _from_ws) = connection.channels;

    to_ws
        .send(Message::binary(payload))
        .await
        .expect("client is running");

    let frame = server.next_frame().await.expect("a frame was written");

    assert_eq!(frame.opcode(), OpCode::Binary);
    assert_eq!(
        frame.into_payload().as_ptr(),
        address,
        "payload was copied on its way to the socket"
    );
}

#[tokio::test]
async fn text_messages_are_written_as_text_frames() {
    let (mut server, connection) = connected().await;
    let (to_ws, _from_ws) = connection.channels;

    to_ws
        .send(Message::text("ping"))
        .await
        .expect("client is running");

    let frame = server.next_frame().await.expect("a frame was written");

    assert_eq!(frame.opcode(), OpCode::Text);
    assert_eq!(&frame.into_payload()[..], b"ping");
}

#[tokio::test]
async fn control_frames_are_not_forwarded_to_the_application() {
    let (server, connection) = connected().await;
    let (_to_ws, mut from_ws) = connection.channels;

    server.send(Frame::ping(Bytes::from_static(b"hi"))).await;
    server.send(Frame::pong(Bytes::from_static(b"hi"))).await;
    server.send(Frame::text(Bytes::from_static(b"data"))).await;

    let Some(Message::Text(text)) = from_ws.recv().await else {
        panic!("expected a text message");
    };

    assert_eq!(text.as_str(), "data", "a control frame was forwarded");
}

#[tokio::test]
async fn a_close_frame_is_reported_to_the_application() {
    let (mut server, connection) = connected().await;
    let (to_ws, mut from_ws) = connection.channels;

    server
        .send(Frame::close(CloseCode::Away, b"maintenance"))
        .await;
    server.disconnect();

    // This application reads to end of stream before letting go of its
    // channels; the test below covers one that does not.
    assert!(from_ws.recv().await.is_none());
    drop((to_ws, from_ws));

    let (close_frame, error) = connection.on_close.await.expect("close was reported");

    let close_frame = close_frame.expect("a close frame was seen");
    assert_eq!(close_frame.code, CloseCode::Away);
    assert_eq!(close_frame.reason, "maintenance");
    assert!(error.is_none(), "unexpected error: {error:?}");

    let closing = server.next_frame().await.expect("the client wrote a frame");
    assert_eq!(
        closing.opcode(),
        OpCode::Close,
        "the client did not close the connection"
    );
}

/// The application is entitled to drop its channels the moment it is done,
/// without first reading to end of stream. What it is told about the close
/// must not depend on that choice.
#[tokio::test]
async fn a_close_frame_is_reported_when_the_application_drops_first() {
    let (mut server, connection) = connected().await;

    server
        .send(Frame::close(CloseCode::Away, b"maintenance"))
        .await;
    server.disconnect();

    drop(connection.channels);

    let (close_frame, error) = connection.on_close.await.expect("close was reported");

    let close_frame = close_frame.expect("a close frame was seen");
    assert_eq!(close_frame.code, CloseCode::Away);
    assert_eq!(close_frame.reason, "maintenance");
    assert!(error.is_none(), "unexpected error: {error:?}");
}

#[tokio::test]
async fn a_read_error_is_reported_when_the_application_drops_first() {
    let (mut server, connection) = connected().await;

    server.fail(WebSocketError::FrameTooLarge).await;
    server.disconnect();

    drop(connection.channels);

    let (_, error) = connection.on_close.await.expect("close was reported");

    assert!(
        matches!(error, Some(WebSocketError::FrameTooLarge)),
        "expected the read error to be forwarded, got {error:?}"
    );
}

#[tokio::test]
async fn a_close_frame_without_a_reason_is_reported() {
    let (mut server, connection) = connected().await;
    let (to_ws, mut from_ws) = connection.channels;

    server.send(Frame::close(CloseCode::Normal, [])).await;
    server.disconnect();

    assert!(from_ws.recv().await.is_none());
    drop((to_ws, from_ws));

    let (close_frame, _) = connection.on_close.await.expect("close was reported");

    let close_frame = close_frame.expect("a close frame was seen");
    assert_eq!(close_frame.code, CloseCode::Normal);
    assert_eq!(close_frame.reason, "");
}

#[tokio::test]
async fn a_read_error_is_reported_to_the_application() {
    let (mut server, connection) = connected().await;
    let (to_ws, mut from_ws) = connection.channels;

    server.fail(WebSocketError::FrameTooLarge).await;
    server.disconnect();

    assert!(from_ws.recv().await.is_none());
    drop((to_ws, from_ws));

    let (close_frame, error) = connection.on_close.await.expect("close was reported");

    assert!(close_frame.is_none());
    assert!(
        matches!(error, Some(WebSocketError::FrameTooLarge)),
        "expected the read error to be forwarded, got {error:?}"
    );
}

#[tokio::test]
async fn invalid_utf8_in_a_text_frame_is_reported_as_an_error() {
    let (mut server, connection) = connected().await;
    let (to_ws, mut from_ws) = connection.channels;

    server
        .send(Frame::text(Bytes::from_static(&[0xff, 0xfe])))
        .await;
    server.disconnect();

    assert!(
        from_ws.recv().await.is_none(),
        "invalid text was forwarded to the application"
    );
    drop(to_ws);

    let (_, error) = connection.on_close.await.expect("close was reported");

    assert!(
        matches!(error, Some(WebSocketError::InvalidUTF8)),
        "expected an invalid UTF-8 error, got {error:?}"
    );
}

#[tokio::test]
async fn a_failed_attempt_is_reported_and_then_retried() {
    let (connector, _servers) = MockConnector::new([
        Attempt::Fail(WebSocketError::InvalidStatusCode(503)),
        Attempt::Serve,
    ]);

    let mut connections = spawn_client(connector, Config::default());

    let first = connections.recv().await.expect("client is running");
    assert!(
        matches!(
            first,
            Err(ConnectError::Inner(WebSocketError::InvalidStatusCode(503)))
        ),
        "expected the connect error to be reported"
    );

    let second = connections.recv().await.expect("client is running");
    assert!(second.is_ok(), "the client did not retry after failing");
}

#[tokio::test]
async fn a_connect_that_never_completes_times_out() {
    let config = ConfigBuilder::new()
        .connect_timeout(Duration::from_millis(50))
        .build();

    let (connector, _servers) = MockConnector::new([Attempt::Hang]);
    let mut connections = spawn_client(connector, config);

    let first = connections.recv().await.expect("client is running");

    assert!(
        matches!(first, Err(ConnectError::TimedOut)),
        "expected a timeout"
    );
}

#[tokio::test]
async fn a_dropped_connection_is_reestablished() {
    let (connector, mut servers) = MockConnector::new([Attempt::Serve, Attempt::Serve]);
    let mut connections = spawn_client(connector, Config::default());

    let first = connections
        .recv()
        .await
        .expect("client is running")
        .expect("connection succeeded");

    let mut server = servers.recv().await.expect("server end");
    server.disconnect();
    drop(first);

    let second = connections.recv().await.expect("client is running");
    assert!(second.is_ok(), "the client did not reconnect");
}
