use futures::{Sink, SinkExt, Stream, StreamExt};
use std::{error::Error as StdError, fmt, time::Duration};
use tokio::{
    join, select,
    sync::{
        mpsc::{self, Receiver, Sender},
        oneshot::{self, Receiver as OnceReceiver, Sender as OnceSender},
    },
    time,
};
use tokio_tungstenite::tungstenite::{
    self,
    protocol::{CloseFrame, WebSocketConfig},
    Error,
};
use url::Url;

use crate::{backoff::BackoffGenerator, config::Config, connection::Connector};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Message {
    Text(String),
    Binary(Vec<u8>),
}

#[derive(Debug)]
pub struct NewConnection {
    pub channels: (Sender<Message>, Receiver<Message>),
    pub connection_closed: OnceReceiver<(Option<CloseFrame<'static>>, Option<Error>)>,
}

#[derive(Debug)]
pub enum ConnectError {
    Inner(Error),
    TimedOut,
}

impl fmt::Display for ConnectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use ConnectError::*;
        match self {
            Inner(e) => fmt::Display::fmt(e, f),
            TimedOut => write!(f, "connection attempt timed out"),
        }
    }
}

impl StdError for ConnectError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        if let ConnectError::Inner(e) = self {
            Some(e)
        } else {
            None
        }
    }
}

impl From<Error> for ConnectError {
    fn from(e: Error) -> Self {
        Self::Inner(e)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconnectingClient;

impl ReconnectingClient {
    pub async fn connect<C, B>(
        config: Config,
        url: Url,
        connection_builder: C,
        backoff_generator: B,
        connection_listener: Sender<Result<NewConnection, ConnectError>>,
    ) where
        C: Connector + Send + 'static,
        B: BackoffGenerator + Send + 'static,
        C::Sender: Send + Unpin,
        C::Receiver: Send + Unpin,
    {
        run(
            config,
            url,
            connection_builder,
            backoff_generator,
            connection_listener,
        )
        .await;
    }
}

async fn run<C, B>(
    config: Config,
    url: Url,
    mut connection_builder: C,
    mut backoff_generator: B,
    listener: Sender<Result<NewConnection, ConnectError>>,
) where
    C: Connector + Send + 'static,
    B: BackoffGenerator + Send + 'static,
    C::Sender: Send + Unpin,
    C::Receiver: Send + Unpin,
{
    loop {
        let res = connect(
            config.ws_config(),
            &url,
            config.connect_timeout(),
            &mut connection_builder,
        )
        .await;

        match res {
            Ok(s) => {
                backoff_generator.reset();

                let (ws_tx, ws_rx) = s;

                let (to_client_tx, to_client_rx) = mpsc::channel(config.channel_size());
                let (to_app_tx, to_app_rx) = mpsc::channel(config.channel_size());
                let (close_finished_tx, close_finished_rx) = oneshot::channel();

                // Connection has been successful, pass the channels
                // to the application
                if listener
                    .send(Ok(NewConnection {
                        channels: (to_client_tx, to_app_rx),
                        connection_closed: close_finished_rx,
                    }))
                    .await
                    .is_err()
                {
                    return;
                }

                run_inner(
                    config.close_timeout(),
                    ws_tx,
                    ws_rx,
                    to_app_tx,
                    to_client_rx,
                    close_finished_tx,
                )
                .await;
            }
            Err(e) => {
                if listener.send(Err(e)).await.is_err() {
                    return;
                }
            }
        }

        let wait_for = backoff_generator.next_delay();

        time::sleep(wait_for).await;
    }
}

/// Try open a connection to `url`.
async fn connect<C>(
    config: Option<WebSocketConfig>,
    url: &Url,
    timeout: Duration,
    connection_builder: &mut C,
) -> Result<(C::Sender, C::Receiver), ConnectError>
where
    C: Connector,
{
    let connection = connection_builder.connect(config, &url);

    if let Ok(res) = time::timeout(timeout, connection).await {
        res.map(|(tx, rx, _)| (tx, rx)).map_err(Into::into)
    } else {
        Err(ConnectError::TimedOut)
    }
}

async fn run_inner<Si, St>(
    close_timeout: Duration,
    mut ws_tx: Si,
    mut ws_rx: St,
    client_to_app_tx: Sender<Message>,
    mut app_to_client_rx: Receiver<Message>,
    connection_closed_tx: OnceSender<(Option<CloseFrame<'static>>, Option<Error>)>,
) where
    Si: Sink<tungstenite::Message, Error = Error> + Unpin,
    St: Stream<Item = Result<tungstenite::Message, Error>> + Unpin,
{
    let (error_tx, mut error_rx) = mpsc::channel(1);
    let (close_frame_tx, mut close_frame_rx) = mpsc::channel(1);

    let (reader_stopped_tx, reader_stopped_rx) = oneshot::channel();
    let (writer_stopped_tx, writer_stopped_rx) = oneshot::channel();

    // Run reader and writer as different futures to avoid potential
    // deadlock. A simpler approach would have been to `select` over
    // both receivers (from the websocket and from the application) in
    // the same future however this could lead to a deadlock if both
    // the client and application `await` on a send to each other when
    // both channels are full.
    let writer = async {
        write_to_socket(
            writer_stopped_rx,
            &error_tx.clone(),
            &mut app_to_client_rx,
            &mut ws_tx,
        )
        .await;

        // Let reader know it should stop
        reader_stopped_tx.send(()).ok();

        // Close receiver
        close_and_drain_receiver(app_to_client_rx).await;

        ws_tx
    };

    let reader = async {
        read_from_socket(
            reader_stopped_rx,
            &close_frame_tx,
            &error_tx,
            client_to_app_tx,
            &mut ws_rx,
        )
        .await;

        // Let writer know it should stop
        writer_stopped_tx.send(()).ok();

        ws_rx
    };

    let (ws_tx, ws_rx) = join!(writer, reader);

    // Close websocket if required
    close_websocket(close_timeout, ws_tx, ws_rx).await;

    // Let the application know we've closed, and forward the close
    // frame and any errors
    let seen_close_frame = close_frame_rx.try_recv().ok();
    let seen_error = error_rx.try_recv().ok();

    connection_closed_tx
        .send((seen_close_frame, seen_error))
        .ok();
}

/// Read messages from `socket` and forward them to the application.
async fn read_from_socket<S>(
    mut shutdown: OnceReceiver<()>,
    close_frame_tx: &Sender<CloseFrame<'static>>,
    error_tx: &Sender<Error>,
    app_tx: Sender<Message>,
    socket: &mut S,
) where
    S: Stream<Item = Result<tungstenite::Message, Error>> + Unpin,
{
    loop {
        select! {
            _ = &mut shutdown => break,
            ws_msg = socket.next() => match ws_msg {
                None => break,
                Some(ws_msg) => match ws_msg {
                    Ok(ws_msg) => {
                        use tungstenite::Message::*;

                        let msg = match ws_msg {
                            Text(s) => Message::Text(s),
                            Binary(b) => Message::Binary(b),
                            Ping(_) | Pong(_) | Frame(_) => continue,
                            Close(frame) => {
                                if let Some(f) = frame {
                                    close_frame_tx.try_send(f).ok();
                                }
                                continue;
                            },
                        };

                        if app_tx.send(msg).await.is_err() {
                            break
                        }
                    }
                    Err(e) => {
                        if let Error::ConnectionClosed | Error::AlreadyClosed = e {
                            // Do nothing, stream will close shortly
                        } else {
                            error_tx.try_send(e).ok();
                            break;
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
    error_tx: &Sender<Error>,
    app_rx: &mut Receiver<Message>,
    socket: &mut S,
) where
    S: Sink<tungstenite::Message, Error = Error> + Unpin,
{
    loop {
        select! {
           _ = &mut shutdown => break,
           msg_to_send = app_rx.recv() => match msg_to_send {
               None => break,
               Some(msg_to_send) => {
                   use tungstenite::Message::*;

                   let ws_msg = match msg_to_send {
                       Message::Text(s) => Text(s),
                       Message::Binary(b) => Binary(b)
                   };

                   if let Err(e) = socket.send(ws_msg).await {
                       if let Error::ConnectionClosed | Error::AlreadyClosed = e {
                           // Do nothing, stream will close shortly
                       } else {
                           error_tx.try_send(e).ok();
                           break;
                       }
                   }
               }
           }
        }
    }
}

/// Handle the websocket close sequence.
async fn close_websocket<Si, St>(timeout: Duration, mut ws_tx: Si, mut ws_rx: St)
where
    Si: Sink<tungstenite::Message, Error = Error> + Unpin,
    St: Stream<Item = Result<tungstenite::Message, Error>> + Unpin,
{
    use tungstenite::Message::*;

    // TODO: determine the proper close frame we need to send from
    // seen errors, if any.
    let close_websocket = ws_tx.send(Close(None));

    let drain_receiver = async { while let Some(_) = ws_rx.next().await {} };

    let close_and_drain = async { join!(close_websocket, drain_receiver) };

    time::timeout(timeout, close_and_drain).await.ok();
}

async fn close_and_drain_receiver<T>(mut rx: Receiver<T>) {
    rx.close();

    while let Some(_) = rx.recv().await {}
}
