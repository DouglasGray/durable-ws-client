use futures::{
    stream::{SplitSink, SplitStream},
    Sink, Stream, StreamExt,
};
use http::Uri;
use tokio::net::TcpStream;
use tokio_tungstenite::{
    tungstenite::{self, handshake::client::Response, protocol::WebSocketConfig, Error},
    MaybeTlsStream, WebSocketStream,
};

/// A websocket connection builder. Takes a URL and returns a pair of
/// channels representing the sending and receiving side of the the
/// connection.
pub trait Connector {
    type Sender: Sink<tungstenite::Message, Error = Error>;
    type Receiver: Stream<Item = Result<tungstenite::Message, Error>>;

    fn connect(
        &mut self,
        config: Option<WebSocketConfig>,
        url: &Uri,
        disable_nagle: bool,
    ) -> impl std::future::Future<Output = Result<(Self::Sender, Self::Receiver, Response), Error>> + Send;
}

/// A WebSocket connection.
#[derive(Clone)]
pub struct WebSocketBuilder;

impl Connector for WebSocketBuilder {
    type Sender = SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, tungstenite::Message>;
    type Receiver = SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>;

    async fn connect(
        &mut self,
        config: Option<WebSocketConfig>,
        url: &Uri,
        disable_nagle: bool,
    ) -> Result<(Self::Sender, Self::Receiver, Response), Error> {
        let (ws, resp) =
            tokio_tungstenite::connect_async_with_config(url.clone(), config, disable_nagle)
                .await?;
        let (tx, rx) = ws.split();
        Ok((tx, rx, resp))
    }
}
