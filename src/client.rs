use futures::{join, Sink, SinkExt, Stream, StreamExt};
use std::time::Duration;
use tokio::{
    select,
    sync::{mpsc, oneshot},
    time,
};
use tokio_tungstenite::tungstenite::{
    protocol::{CloseFrame, WebSocketConfig as WsConfig},
    Error as WsError, Message as WsMessage,
};
use url::Url;

use crate::{backoff::BackoffGenerator, websocket::Builder as WebSocketBuilder};

type Responder<E> = oneshot::Sender<Result<(), E>>;

/// Messages which may be sent down the websocket.
///
/// The result of the send will be returned via `Responder`.
#[derive(Debug)]
pub enum MsgToSend {
    Text(String, Responder<WsError>),
    Binary(Vec<u8>, Responder<WsError>),
}

/// Messages which may be received from the websocket.
#[derive(Debug, Clone)]
pub enum MsgRecvd {
    Text(String),
    Binary(Vec<u8>),
    /// Peer has initated closing.
    Close(Option<CloseFrame<'static>>),
}

/// Events that may occur in the lifetime of the client.
#[derive(Debug)]
pub enum Event {
    /// Sent before each new connection attempt.
    AttemptingNewConnection,
    /// Sent when a connection attempt fails.
    FailedToConnect(FailReason),
    /// Signifies a connection has succeeded, and passes a writer and
    /// reader pair. These may be used to send messages down and
    /// receive messages from the websocket, respectively.
    Connected(
        mpsc::Sender<MsgToSend>,
        mpsc::Receiver<Result<MsgRecvd, WsError>>,
    ),
    /// Sent when the connection closes. The reader and writer pair
    /// above will be closed before this message is sent.
    ConnectionClosed,
}

/// Reasons a connection attempt may fail.
#[derive(Debug)]
pub enum FailReason {
    Error(WsError),
    TimedOut,
}

impl From<WsError> for FailReason {
    fn from(e: WsError) -> Self {
        FailReason::Error(e)
    }
}

/// Client configuration.
pub struct Config {
    /// Passed to `tokio-tungstenite` when connecting.
    pub ws_config: Option<WsConfig>,
    /// How long to wait for the connection attempt to complete before
    /// bailing.
    pub connect_timeout: Duration,
    /// How long to wait for the connection to close if initiated
    /// client-side.
    pub close_timeout: Duration,
    /// Channel size to use for communicating between the client and
    /// application.
    pub channel_size: usize,
}

/// A reconnecting websocket client. This will continually try to
/// restore a websocket connection to the provided `url` if it
/// closes.
///
/// It passes any errors to the application so it may determine
/// whether it wants to stop. This may be done by dropping the
/// returned `mpsc::Receiver`.
#[derive(Debug)]
pub struct Client;

impl Client {
    pub async fn connect<W, B>(
        config: Config,
        url: Url,
        websocket_builder: W,
        backoff_generator: B,
    ) -> mpsc::Receiver<Event>
    where
        W: 'static + WebSocketBuilder + Send + Clone,
        B: 'static + BackoffGenerator + Send,
        W::Sender: Send + Unpin,
        W::Receiver: Send + Unpin,
    {
        let (tx, rx) = mpsc::channel(config.channel_size);

        tokio::spawn(async move {
            run(config, url, websocket_builder, backoff_generator, tx).await;
        });

        rx
    }
}

async fn run<W, B>(
    config: Config,
    url: Url,
    mut websocket_builder: W,
    mut backoff_generator: B,
    listener: mpsc::Sender<Event>,
) where
    W: 'static + WebSocketBuilder + Send + Clone,
    B: 'static + BackoffGenerator + Send,
    W::Sender: Send + Unpin,
    W::Receiver: Send + Unpin,
{
    use Event::*;

    loop {
        if let Err(_) = listener.send(AttemptingNewConnection).await {
            return;
        }

        // Try connect
        let res = connect(
            config.ws_config.clone(),
            &url,
            config.connect_timeout,
            &mut websocket_builder,
        )
        .await;

        match res {
            Ok(s) => {
                backoff_generator.reset();

                let (ws_tx, ws_rx) = s;

                let (app_to_client_tx, app_to_client_rx) = mpsc::channel(config.channel_size);
                let (client_to_app_tx, client_to_app_rx) = mpsc::channel(config.channel_size);

                // Connection has been successful, let the application know
                if let Err(_) = listener
                    .send(Connected(app_to_client_tx, client_to_app_rx))
                    .await
                {
                    return;
                }

                run_inner(
                    config.close_timeout,
                    ws_tx,
                    ws_rx,
                    client_to_app_tx,
                    app_to_client_rx,
                )
                .await;

                // Inform the application that the connection has closed
                if let Err(_) = listener.send(ConnectionClosed).await {
                    return;
                }
            }
            Err(e) => {
                // Let the application know the connection attempt failed
                if let Err(_) = listener.send(FailedToConnect(e)).await {
                    return;
                }
            }
        }

        // Wait some time before trying again
        let wait_for = backoff_generator.next_delay();

        time::sleep(wait_for).await;
    }
}

async fn run_inner<Si, St>(
    close_timeout: Duration,
    mut ws_tx: Si,
    mut ws_rx: St,
    client_to_app_tx: mpsc::Sender<Result<MsgRecvd, WsError>>,
    mut app_to_client_rx: mpsc::Receiver<MsgToSend>,
) where
    Si: Sink<WsMessage, Error = WsError> + Unpin,
    St: Stream<Item = Result<WsMessage, WsError>> + Unpin,
{
    let (read_shutdown_tx, read_shutdown_rx) = oneshot::channel();
    let (write_shutdown_tx, write_shutdown_rx) = oneshot::channel();

    // Run reader and writer in different tasks to avoid
    // potential deadlock with the application. A simpler
    // approach would have been to `select` over both
    // receivers (from websocket and from application) in
    // the same task however this could lead to a deadlock
    // if both the client and application `await` on a
    // send to each other when both channels are full.
    let write_handle = async move {
        write_to_socket(write_shutdown_rx, &mut ws_tx, &mut app_to_client_rx).await;

        // Let reader task know it should stop
        let _ = read_shutdown_tx.send(());

        // Close receiver
        close_and_drain_receiver(app_to_client_rx).await;

        ws_tx
    };

    let read_handle = async move {
        read_from_socket(read_shutdown_rx, &mut ws_rx, client_to_app_tx).await;

        // Let writer task know it should stop
        let _ = write_shutdown_tx.send(());

        ws_rx
    };

    let (ws_tx, ws_rx) = join!(write_handle, read_handle);

    // Close websocket if required
    close_websocket(ws_tx, ws_rx, close_timeout).await;
}

/// Connect to `url` and return sender and receiver halves of the
/// channel.
async fn connect<W>(
    config: Option<WsConfig>,
    url: &Url,
    timeout: Duration,
    websocket_builder: &mut W,
) -> Result<(W::Sender, W::Receiver), FailReason>
where
    W: WebSocketBuilder,
{
    let connect_fut = websocket_builder.connect(config, &url);

    match time::timeout(timeout, connect_fut).await {
        Ok(res) => match res {
            Ok((tx, rx, _)) => Ok((tx, rx)),
            Err(e) => Err(e.into()),
        },
        Err(_) => Err(FailReason::TimedOut),
    }
}

/// Read messages from `socket` and forward them to the
/// application via `app_tx`.
async fn read_from_socket<S>(
    mut shutdown: oneshot::Receiver<()>,
    socket: &mut S,
    app_tx: mpsc::Sender<Result<MsgRecvd, WsError>>,
) where
    S: Stream<Item = Result<WsMessage, WsError>> + Unpin,
{
    loop {
        select! {
            _ = &mut shutdown => return,
            ws_msg = socket.next() => match ws_msg {
                None => return,
                Some(ws_msg) => match ws_msg {
                    Ok(ws_msg) => {
                        if let Some(msg) = process_recvd_msg(ws_msg) {
                            if let Err(_) = app_tx.send(Ok(msg)).await {
                                return;
                            }
                        }
                    }
                    Err(e) => match e {
                        WsError::ConnectionClosed | WsError::AlreadyClosed => (),
                        _ => {
                            if let Err(_) = app_tx.send(Err(e)).await {
                                return;
                            }
                        }
                    }
                }

            }
        }
    }
}

/// Read messages from the application via `app_rx` and send them down
/// `socket`.
async fn write_to_socket<S>(
    mut shutdown: oneshot::Receiver<()>,
    socket: &mut S,
    app_rx: &mut mpsc::Receiver<MsgToSend>,
) where
    S: Sink<WsMessage, Error = WsError> + Unpin,
{
    loop {
        select! {
            _ = &mut shutdown => return,
            msg_to_send = app_rx.recv() => match msg_to_send {
                None => return,
                Some(msg_to_send) => {
                    let (ws_msg, resp) = match msg_to_send {
                        MsgToSend::Text(s, resp) => (WsMessage::Text(s), resp),
                        MsgToSend::Binary(b, resp) => (WsMessage::Binary(b), resp)
                    };

                    let res = socket.send(ws_msg).await;

                    let _ = resp.send(res);
                }
            }
        }
    }
}

fn process_recvd_msg(ws_msg: WsMessage) -> Option<MsgRecvd> {
    match ws_msg {
        WsMessage::Text(s) => Some(MsgRecvd::Text(s)),
        WsMessage::Binary(b) => Some(MsgRecvd::Binary(b)),
        WsMessage::Close(f) => Some(MsgRecvd::Close(f)),
        WsMessage::Ping(_) | WsMessage::Pong(_) => None,
    }
}

async fn close_and_drain_receiver<T>(mut rx: mpsc::Receiver<T>) {
    rx.close();

    while let Some(_) = rx.recv().await {}
}

async fn close_websocket<Si, St>(mut ws_tx: Si, mut ws_rx: St, close_timeout: Duration)
where
    Si: Sink<WsMessage, Error = WsError> + Unpin,
    St: Stream<Item = Result<WsMessage, WsError>> + Unpin,
{
    // Send close frame
    if let Err(_) = ws_tx.send(WsMessage::Close(None)).await {
        return;
    }

    // Consume messages till the connection closes
    let drain_fut = async { while let Some(_) = ws_rx.next().await {} };

    let _ = time::timeout(close_timeout, drain_fut).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receiving_a_message_should_work() {
        todo!()
    }

    #[test]
    fn sending_a_message_should_work() {
        todo!()
    }

    mod utils {
        use async_channel::{
            Receiver as UnboundedReceiver, Sender as UnboundedSender, TryRecvError,
        };
        use futures::{Sink, Stream};
        use std::{
            pin::Pin,
            task::{Context, Poll},
        };
        use tokio_tungstenite::tungstenite::{Error as WsError, Message as WsMessage};

        // Create a websocket stream, plus sender to submit messages
        // into the stream
        fn create_ws_stream() -> (
            UnboundedSender<Result<WsMessage, WsError>>,
            impl Stream<Item = Result<WsMessage, WsError>>,
        ) {
            async_channel::unbounded()
        }

        // Create a websocket sink, along with a sender to submit
        // errors to and a receiver to read the messages that have
        // been submitted to the sink
        fn create_ws_sink() -> (
            UnboundedSender<WsError>,
            UnboundedReceiver<WsMessage>,
            impl Sink<WsMessage, Error = WsError>,
        ) {
            let (err_tx, err_rx) = async_channel::unbounded();
            let (ws_tx, ws_rx) = async_channel::unbounded();

            let sink = TestSink {
                sink: ws_tx,
                errs: err_rx,
            };

            (err_tx, ws_rx, sink)
        }

        struct TestSink {
            sink: UnboundedSender<WsMessage>,
            errs: UnboundedReceiver<WsError>,
        }

        impl Sink<WsMessage> for TestSink {
            type Error = WsError;

            fn poll_ready(
                self: Pin<&mut Self>,
                _: &mut Context<'_>,
            ) -> Poll<Result<(), Self::Error>> {
                Poll::Ready(Ok(()))
            }

            fn start_send(self: Pin<&mut Self>, item: WsMessage) -> Result<(), Self::Error> {
                match self.errs.try_recv() {
                    Ok(e) => return Err(e),
                    Err(TryRecvError::Closed) => return Err(WsError::ConnectionClosed),
                    Err(TryRecvError::Empty) => {}
                }

                if let Err(_) = self.sink.try_send(item) {
                    return Err(WsError::ConnectionClosed);
                }

                Ok(())
            }

            fn poll_flush(
                self: Pin<&mut Self>,
                _: &mut Context<'_>,
            ) -> Poll<Result<(), Self::Error>> {
                Poll::Ready(Ok(()))
            }

            fn poll_close(
                self: Pin<&mut Self>,
                _: &mut Context<'_>,
            ) -> Poll<Result<(), Self::Error>> {
                Poll::Ready(Ok(()))
            }
        }
    }
}
