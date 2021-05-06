use async_trait::async_trait;
use futures::{
    stream::{SplitSink, SplitStream},
    Sink, Stream, StreamExt,
};
use tokio::net::TcpStream;
use tokio_tungstenite::{
    tungstenite::{
        handshake::client::Response, protocol::WebSocketConfig as WsConfig, Error as WsError,
        Message as WsMessage,
    },
    MaybeTlsStream, WebSocketStream,
};
use url::Url;

/// Takes a URL and builds a websocket connection.
///
/// Moved into a trait to make testing easier.
#[async_trait]
pub trait Builder {
    type Sender: Sink<WsMessage, Error = WsError>;
    type Receiver: Stream<Item = Result<WsMessage, WsError>>;

    async fn connect(
        &mut self,
        config: Option<WsConfig>,
        url: &Url,
    ) -> Result<(Self::Sender, Self::Receiver, Response), WsError>;
}

/// Opens a websocket via TCP.
#[derive(Clone)]
pub struct DefaultBuilder {}

impl DefaultBuilder {
    pub fn new() -> Self {
        DefaultBuilder {}
    }
}

#[async_trait]
impl Builder for DefaultBuilder {
    type Sender = SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, WsMessage>;
    type Receiver = SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>;

    async fn connect(
        &mut self,
        config: Option<WsConfig>,
        url: &Url,
    ) -> Result<(Self::Sender, Self::Receiver, Response), WsError> {
        let (ws, resp) = tokio_tungstenite::connect_async_with_config(url.clone(), config).await?;
        let (tx, rx) = ws.split();
        Ok((tx, rx, resp))
    }
}
