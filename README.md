# durable-ws-client

A reconnecting websocket client written in Rust, using
[`yawc`](https://github.com/infinitefield/yawc).

The client (located in `src/client.rs`) will attempt to reconnect on
any error, while informing the application of the error so it can
permanently terminate the connection if desired.

Payloads are passed through as `bytes::Bytes`, so a message is handed
to the application without its contents being copied out of the
connection's read buffer. Text arrives as `Utf8Bytes`, which validates
once on receipt and derefs to `str`.

### Example usage

Requires an entry along the lines of `tokio = { version = "1.45.1",
features = ["rt-multi-thread", "macros", "sync"] }` in your
`Cargo.toml` to work. TLS is handled by rustls; the default
`"rustls-ring"` feature picks the crypto provider, and
`"rustls-aws-lc-rs"` is the alternative. To verify certificates against
the platform's native roots rather than the bundled webpki ones, pass a
`tokio_rustls::TlsConnector` to `WebSocketBuilder::with_connector`.

The sketch below shows the shape of the API. For a version you can
actually run, `cargo run --example echo` starts a local server and
connects to it, so it needs no network and shows a reconnect as well as
a close.

```
use durable_ws_client::{
    client::{NewConnection, ReconnectingClient},
    config::Config,
    connection::WebSocketBuilder,
};
use tokio::join;
use tower::retry::backoff::ExponentialBackoffMaker;

#[tokio::main]
async fn main() {
    let websocket_builder = WebSocketBuilder::new();
    let backoff_builder = ExponentialBackoffMaker::default();

    let (reconnecting_client, mut new_connection_rx) = ReconnectingClient::new();

    let client_task = async move {
        reconnecting_client
            .connect(
                Config::default(),
                "wss://example.com/socket".parse().unwrap(),
                websocket_builder,
                backoff_builder.into(),
            )
            .await;
    };

    let reader_task = async move {
        let NewConnection {
            channels: (tx, mut rx),
            on_close: connection_closed,
        } = new_connection_rx.recv().await.unwrap().unwrap();

        println!("message received: {:?}", rx.recv().await.unwrap());

        drop(tx);
        drop(rx);

        if let Ok(on_close) = connection_closed.await {
            println!("close data received: {:?}", on_close);
        }
    };

    join!(client_task, reader_task);
}
```
