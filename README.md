# durable-ws-client

A reconnecting websocket client written in Rust, using
[`tokio-tungstenite`](https://github.com/snapview/tokio-tungstenite).

The client (located in `src/client.rs`) will attempt to reconnect on
any error, while informing the application of the error so it can
permanently terminate the connection if desired.

### Example usage

Requires an entry along the lines of `tokio = { version = "1.45.1",
features = ["rt-multi-thread", "macros", "sync"] }` in your
`Cargo.toml` to work, along with some kind of TLS feature enabled on
this library, for example `"native-tls"`.

```
use durable_ws_client::{
    client::{NewConnection, ReconnectingClient},
    config::Config,
    connection::WebSocketBuilder,
};
use tokio::{join, sync::mpsc};
use tower::retry::backoff::ExponentialBackoffMaker;

#[tokio::main]
async fn main() {
    let websocket = WebSocketBuilder;
    let backoff_builder = ExponentialBackoffMaker::default();

    let (new_connection_tx, mut new_connection_rx) = mpsc::channel(1);

    let client_task = async move {
        ReconnectingClient::connect(
            Config::default(),
            "wss://www.bitmex.com/realtime".parse().unwrap(),
            websocket,
            backoff_builder,
            new_connection_tx,
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