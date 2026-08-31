//! Tests that run the client over a real socket, against a server that speaks
//! the protocol by hand. Everything the client depends on — the handshake,
//! yawc's frame codec, its automatic ping and close handling — is exercised
//! for real here, which the in-memory tests in `client.rs` deliberately skip.

use std::time::Duration;

use base64::{Engine, engine::general_purpose::STANDARD};
use bytes::Bytes;
use durable_ws_client::{
    client::{ConnectError, Message, NewConnection, ReconnectingClient},
    config::Config,
    connection::WebSocketBuilder,
};
use futures::{SinkExt, StreamExt};
use http::Uri;
use sha1::{Digest, Sha1};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::mpsc,
};
use tokio_util::codec::Framed;
use tower::retry::backoff::ExponentialBackoffMaker;
use yawc::{
    Role,
    close::CloseCode,
    codec::{Codec, Decoder, Encoder},
    frame::{Frame, OpCode},
};

/// The magic value RFC 6455 mixes into `Sec-WebSocket-Accept`.
const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// Fails a test rather than hanging it if the client never gets there.
async fn expect<F: Future>(what: &str, f: F) -> F::Output {
    match tokio::time::timeout(Duration::from_secs(5), f).await {
        Ok(output) => output,
        Err(_) => panic!("timed out waiting for {what}"),
    }
}

/// Completes the websocket handshake, then speaks frames using yawc's codec in
/// its server role.
async fn accept(listener: &TcpListener) -> Framed<TcpStream, Codec> {
    let (mut stream, _) = listener.accept().await.expect("a client connected");

    // Read the request one byte at a time so nothing the client sends after
    // the headers is swallowed along with them.
    let mut request = Vec::new();
    let mut byte = [0u8; 1];
    while !request.ends_with(b"\r\n\r\n") {
        stream
            .read_exact(&mut byte)
            .await
            .expect("the request headers");
        request.push(byte[0]);
    }

    let request = String::from_utf8(request).expect("the request is text");
    let key = request
        .lines()
        .find(|line| line.to_ascii_lowercase().starts_with("sec-websocket-key:"))
        .and_then(|line| line.split_once(':'))
        .map(|(_, value)| value.trim())
        .expect("the client sent a key");

    let mut hasher = Sha1::new();
    hasher.update(key.as_bytes());
    hasher.update(GUID.as_bytes());
    let accept = STANDARD.encode(hasher.finalize());

    stream
        .write_all(
            format!(
                "HTTP/1.1 101 Switching Protocols\r\n\
                 Upgrade: websocket\r\n\
                 Connection: Upgrade\r\n\
                 Sec-WebSocket-Accept: {accept}\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .expect("the handshake response was written");

    Framed::new(
        stream,
        Codec::from((
            Decoder::new(Role::Server, 16 << 20),
            Encoder::new(Role::Server),
        )),
    )
}

/// Runs a client against `listener`'s address until the returned receiver is
/// dropped.
fn spawn_client(listener: &TcpListener) -> mpsc::Receiver<Result<NewConnection, ConnectError>> {
    let uri: Uri = format!("ws://{}/", listener.local_addr().expect("a bound address"))
        .parse()
        .expect("a valid url");

    let (client, connections) = ReconnectingClient::new();

    tokio::spawn(async move {
        client
            .connect(
                Config::default(),
                uri,
                WebSocketBuilder::new(),
                None::<ExponentialBackoffMaker>,
            )
            .await;
    });

    connections
}

async fn listener() -> TcpListener {
    TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a local port")
}

async fn connect(
    connections: &mut mpsc::Receiver<Result<NewConnection, ConnectError>>,
) -> NewConnection {
    expect("a connection", connections.recv())
        .await
        .expect("the client is running")
        .expect("the connection succeeded")
}

#[tokio::test]
async fn messages_round_trip_over_a_real_socket() {
    let listener = listener().await;
    let mut connections = spawn_client(&listener);

    let (mut server, connection) = tokio::join!(accept(&listener), connect(&mut connections));
    let (to_ws, mut from_ws) = connection.channels;

    to_ws
        .send(Message::text("hello"))
        .await
        .expect("the client is running");

    let frame = expect("a text frame", server.next())
        .await
        .expect("a frame arrived")
        .expect("the frame decoded");
    assert_eq!(frame.opcode(), OpCode::Text);
    assert_eq!(frame.payload().as_ref(), b"hello");

    server
        .send(Frame::text(Bytes::from_static(b"hello back")))
        .await
        .expect("the echo was written");

    let Some(Message::Text(text)) = expect("a text message", from_ws.recv()).await else {
        panic!("expected a text message");
    };
    assert_eq!(text.as_str(), "hello back");

    to_ws
        .send(Message::binary(Bytes::from_static(&[1, 2, 3])))
        .await
        .expect("the client is running");

    let frame = expect("a binary frame", server.next())
        .await
        .expect("a frame arrived")
        .expect("the frame decoded");
    assert_eq!(frame.opcode(), OpCode::Binary);
    assert_eq!(frame.payload().as_ref(), &[1, 2, 3]);
}

#[tokio::test]
async fn pings_are_answered_without_reaching_the_application() {
    let listener = listener().await;
    let mut connections = spawn_client(&listener);

    let (mut server, connection) = tokio::join!(accept(&listener), connect(&mut connections));
    let (_to_ws, mut from_ws) = connection.channels;

    server
        .send(Frame::ping(Bytes::from_static(b"beat")))
        .await
        .expect("the ping was written");

    let frame = expect("a pong", server.next())
        .await
        .expect("a frame arrived")
        .expect("the frame decoded");

    assert_eq!(frame.opcode(), OpCode::Pong);
    assert_eq!(frame.payload().as_ref(), b"beat");

    // The ping must not have been forwarded: the next thing the application
    // sees is the message that follows it.
    server
        .send(Frame::text(Bytes::from_static(b"data")))
        .await
        .expect("the message was written");

    let Some(Message::Text(text)) = expect("a text message", from_ws.recv()).await else {
        panic!("expected a text message");
    };
    assert_eq!(text.as_str(), "data");
}

#[tokio::test]
async fn a_server_close_is_reported_and_answered() {
    let listener = listener().await;
    let mut connections = spawn_client(&listener);

    let (mut server, connection) = tokio::join!(accept(&listener), connect(&mut connections));
    let (to_ws, mut from_ws) = connection.channels;

    server
        .send(Frame::close(CloseCode::Away, b"maintenance"))
        .await
        .expect("the close was written");

    // The application reads until the client signals end of stream, then
    // winds its own side down. Dropping the sender first would end the writer
    // task, which cancels the reader before it has seen the close frame.
    assert!(expect("end of stream", from_ws.recv()).await.is_none());
    drop((to_ws, from_ws));

    let (close_frame, error) = expect("the close report", connection.on_close)
        .await
        .expect("close was reported");

    let close_frame = close_frame.expect("a close frame was seen");
    assert_eq!(close_frame.code, CloseCode::Away);
    assert_eq!(close_frame.reason, "maintenance");
    assert!(error.is_none(), "unexpected error: {error:?}");

    let echoed = expect("a close frame", server.next())
        .await
        .expect("a frame arrived")
        .expect("the frame decoded");
    assert_eq!(echoed.opcode(), OpCode::Close);
}

/// As above, but over a real socket, and with the application dropping its
/// channels straight away rather than reading to end of stream first.
#[tokio::test]
async fn a_server_close_is_reported_when_the_application_drops_first() {
    let listener = listener().await;
    let mut connections = spawn_client(&listener);

    let (mut server, connection) = tokio::join!(accept(&listener), connect(&mut connections));

    server
        .send(Frame::close(CloseCode::Away, b"maintenance"))
        .await
        .expect("the close was written");

    drop(connection.channels);

    let (close_frame, error) = expect("the close report", connection.on_close)
        .await
        .expect("close was reported");

    let close_frame = close_frame.expect("a close frame was seen");
    assert_eq!(close_frame.code, CloseCode::Away);
    assert_eq!(close_frame.reason, "maintenance");
    assert!(error.is_none(), "unexpected error: {error:?}");
}

#[tokio::test]
async fn a_protocol_error_is_reported_when_the_application_drops_first() {
    let listener = listener().await;
    let mut connections = spawn_client(&listener);

    let (server, connection) = tokio::join!(accept(&listener), connect(&mut connections));

    // 0x83 is FIN plus opcode 3, which is not a valid opcode.
    server
        .into_inner()
        .write_all(&[0x83, 0x00])
        .await
        .expect("the bad frame was written");

    drop(connection.channels);

    let (_, error) = expect("the close report", connection.on_close)
        .await
        .expect("close was reported");

    assert!(
        error.is_some(),
        "the read error was lost because the application dropped its channels first"
    );
}

#[tokio::test]
async fn a_protocol_error_is_reported_to_the_application() {
    let listener = listener().await;
    let mut connections = spawn_client(&listener);

    let (server, connection) = tokio::join!(accept(&listener), connect(&mut connections));
    let (to_ws, mut from_ws) = connection.channels;

    // 0x83 is FIN plus opcode 3, which is not a valid opcode.
    server
        .into_inner()
        .write_all(&[0x83, 0x00])
        .await
        .expect("the bad frame was written");

    assert!(expect("end of stream", from_ws.recv()).await.is_none());
    drop((to_ws, from_ws));

    let (_, error) = expect("the close report", connection.on_close)
        .await
        .expect("close was reported");

    // Without preserving read errors out of yawc's stream, this arrives as a
    // silent end-of-stream instead.
    assert!(
        error.is_some(),
        "the read error was swallowed instead of being reported"
    );
}

#[tokio::test]
async fn the_client_reconnects_after_the_server_goes_away() {
    let listener = listener().await;
    let mut connections = spawn_client(&listener);

    let (server, connection) = tokio::join!(accept(&listener), connect(&mut connections));

    drop(server);
    drop(connection);

    let (mut server, connection) = tokio::join!(accept(&listener), connect(&mut connections));
    let (to_ws, _from_ws) = connection.channels;

    to_ws
        .send(Message::text("still here"))
        .await
        .expect("the client is running");

    let frame = expect("a frame on the new connection", server.next())
        .await
        .expect("a frame arrived")
        .expect("the frame decoded");

    assert_eq!(frame.payload().as_ref(), b"still here");
}
