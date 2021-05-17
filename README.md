# durable-ws-client

A reconnecting websocket client written in Rust.

The client (located in `src/client.rs`) will attempt to reconnect on
any error, while informing the application of the error so it can
permanently terminate the connection if desired.

### Example usage

Requires an entry along the lines of `tokio = { version = "1.5",
features = ["rt-multi-thread", "macros"] }` in your `Cargo.toml` to
work.

```
use durable_ws_client::{
    backoff::FixedBackoff,
    client::{Client, Config, Event},
    websocket::DefaultBuilder,
};
use std::time::Duration;

#[tokio::main]
async fn main() {
    let builder = DefaultBuilder::new();
    let backoff = FixedBackoff::new(Duration::from_secs(1));

    let mut event_rx = Client::connect(
        Config::default(),
        "wss://www.bitmex.com/realtime".parse().unwrap(),
        builder,
        backoff,
    )
    .await;

    match event_rx.recv().await.unwrap() {
        Event::AttemptingNewConnection => {}
        _ => panic!("unexpected event"),
    }

    let (_msg_tx, mut msg_rx) = match event_rx.recv().await.unwrap() {
        Event::Connected(tx, rx) => (tx, rx),
        _ => panic!("unexpected event"),
    };

    let recvd_msg = msg_rx.recv().await.unwrap();

    println!("received message: {:?}", recvd_msg);
}
```
