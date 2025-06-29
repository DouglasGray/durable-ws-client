use durable_ws_client::{
    client::{NewConnection, ReconnectingClient},
    config::Config,
    connection::WebSocketBuilder,
};
use tokio::{join, sync::mpsc};
use tower::retry::backoff::ExponentialBackoffMaker;

#[tokio::main]
async fn main() {
    let websocket_builder = WebSocketBuilder;
    let backoff_builder = ExponentialBackoffMaker::default();

    let (new_connection_tx, mut new_connection_rx) = mpsc::channel(1);

    let client_task = async move {
        ReconnectingClient::connect(
            Config::default(),
            "wss://www.bitmex.com/realtime".parse().unwrap(),
            websocket_builder,
            backoff_builder.into(),
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
