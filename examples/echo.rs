//! A runnable example, needing no network: it starts a local websocket server
//! and connects to it, showing what an application sees over the lifetime of a
//! connection — messages, the close frame the peer sent, and the reconnect
//! that follows.
//!
//! Run it with `cargo run --example echo`.

use base64::{Engine, engine::general_purpose::STANDARD};
use bytes::Bytes;
use durable_ws_client::{
    client::{Message, NewConnection, ReconnectingClient},
    config::Config,
    connection::WebSocketBuilder,
};
use futures::{SinkExt, StreamExt};
use sha1::{Digest, Sha1};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    join,
    net::{TcpListener, TcpStream},
};
use tokio_util::codec::Framed;
use tower::retry::backoff::ExponentialBackoffMaker;
use yawc::{
    Role,
    close::CloseCode,
    codec::{Codec, Decoder, Encoder},
    frame::Frame,
};

/// How many connections the server serves before going away, and the client
/// therefore makes. The second one is only reached by reconnecting.
const CONNECTIONS: usize = 2;

#[tokio::main]
async fn main() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/", listener.local_addr().unwrap());

    tokio::spawn(serve(listener));

    let websocket_builder = WebSocketBuilder::new();
    let backoff_builder = ExponentialBackoffMaker::default();

    let (reconnecting_client, mut new_connection_rx) = ReconnectingClient::new();

    let client_task = async move {
        reconnecting_client
            .connect(
                Config::default(),
                url.parse().unwrap(),
                websocket_builder,
                backoff_builder.into(),
            )
            .await;
    };

    let reader_task = async move {
        for attempt in 1..=CONNECTIONS {
            let NewConnection {
                channels: (tx, mut rx),
                on_close: connection_closed,
            } = new_connection_rx.recv().await.unwrap().unwrap();

            println!("connection {attempt} established");

            tx.send(Message::text("hello")).await.unwrap();

            // The receiver ends when the connection does, so this loop is how
            // an application notices the peer going away.
            while let Some(message) = rx.recv().await {
                println!("  message received: {message:?}");
            }

            drop(tx);
            drop(rx);

            if let Ok(on_close) = connection_closed.await {
                println!("  close data received: {on_close:?}");
            }
        }

        // Dropping the receiver stops the client, and with it this program.
    };

    join!(client_task, reader_task);
}

/// Greets each client, echoes one message back, then closes.
async fn serve(listener: TcpListener) {
    for _ in 0..CONNECTIONS {
        let mut ws = accept(&listener).await;

        ws.send(Frame::text(Bytes::from_static(b"welcome")))
            .await
            .unwrap();

        if let Some(Ok(frame)) = ws.next().await {
            ws.send(Frame::text(frame.into_payload())).await.unwrap();
        }

        ws.send(Frame::close(CloseCode::Normal, b"goodbye"))
            .await
            .unwrap();
    }
}

/// The uninteresting half: completes the RFC 6455 handshake by hand, then
/// speaks frames using yawc's codec in its server role. A real application
/// would reach for a server library instead.
async fn accept(listener: &TcpListener) -> Framed<TcpStream, Codec> {
    const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

    let (mut stream, _) = listener.accept().await.unwrap();

    // Read the request one byte at a time so nothing sent after the headers is
    // swallowed along with them.
    let mut request = Vec::new();
    let mut byte = [0u8; 1];
    while !request.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).await.unwrap();
        request.push(byte[0]);
    }

    let request = String::from_utf8(request).unwrap();
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
        .unwrap();

    Framed::new(
        stream,
        Codec::from((
            Decoder::new(Role::Server, 1 << 20),
            Encoder::new(Role::Server),
        )),
    )
}
