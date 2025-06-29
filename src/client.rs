use futures::{Sink, SinkExt, Stream, StreamExt};
use http::Uri;
use std::{error::Error as StdError, fmt, time::Duration};
use tokio::task::JoinHandle;
use tokio::{
    join, select,
    sync::{
        mpsc::{self, Receiver, Sender},
        oneshot::{self, Receiver as OnceReceiver, Sender as OnceSender},
    },
    time,
};
use tokio_tungstenite::tungstenite::{self, protocol::CloseFrame, Error};
use tokio_util::sync::CancellationToken;
use tower::retry::backoff::{Backoff, MakeBackoff};

use crate::{config::Config, connection::Connector};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Message {
    Text(String),
    Binary(Vec<u8>),
}

#[derive(Debug)]
pub struct NewConnection {
    pub channels: (Sender<Message>, Receiver<Message>),
    pub on_close: OnceReceiver<(Option<CloseFrame<'static>>, Option<Error>)>,
}

#[derive(Debug)]
pub enum ConnectError {
    Inner(Error),
    TimedOut,
}

impl fmt::Display for ConnectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Inner(e) => fmt::Display::fmt(e, f),
            Self::TimedOut => write!(f, "connection attempt timed out"),
        }
    }
}

impl StdError for ConnectError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Inner(e) => Some(e),
            Self::TimedOut => None,
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
        url: Uri,
        connection_builder: C,
        backoff_builder: Option<B>,
        connection_listener: Sender<Result<NewConnection, ConnectError>>,
    ) where
        C: Connector + Send + 'static,
        B: MakeBackoff + Send + 'static,
        C::Sender: Send + Unpin,
        C::Receiver: Send + Unpin,
    {
        run(
            config,
            url,
            connection_builder,
            backoff_builder,
            connection_listener,
        )
        .await;
    }
}

async fn run<C, B>(
    config: Config,
    url: Uri,
    mut connection_builder: C,
    mut backoff_builder: Option<B>,
    listener: Sender<Result<NewConnection, ConnectError>>,
) where
    C: Connector + Send + 'static,
    B: MakeBackoff + Send + 'static,
    C::Sender: Send + Unpin,
    C::Receiver: Send + Unpin,
{
    let mut backoff = backoff_builder.as_mut().map(|b| b.make_backoff());

    loop {
        let res = connect(config, &url, &mut connection_builder).await;

        match res {
            Ok(s) => {
                backoff = backoff_builder.as_mut().map(|b| b.make_backoff());

                let (ws_tx, ws_rx) = s;

                let (to_client_tx, to_client_rx) = mpsc::channel(config.channel_size());
                let (to_app_tx, to_app_rx) = mpsc::channel(config.channel_size());
                let (on_close_tx, on_close_rx) = oneshot::channel();

                // Connection has been successful, pass the channels
                // to the application
                if listener
                    .send(Ok(NewConnection {
                        channels: (to_client_tx, to_app_rx),
                        on_close: on_close_rx,
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
                    on_close_tx,
                )
                .await;
            }
            Err(e) => {
                if listener.send(Err(e)).await.is_err() {
                    return;
                }
            }
        }

        if let Some(b) = &mut backoff {
            b.next_backoff().await;
        }
    }
}

/// Try open a connection to `url`.
async fn connect<C>(
    config: Config,
    url: &Uri,
    connection_builder: &mut C,
) -> Result<(C::Sender, C::Receiver), ConnectError>
where
    C: Connector,
{
    let connection = connection_builder.connect(config.ws_config(), url, config.disable_nagle());

    if let Ok(res) = time::timeout(config.connect_timeout(), connection).await {
        res.map(|(tx, rx, _)| (tx, rx)).map_err(Into::into)
    } else {
        Err(ConnectError::TimedOut)
    }
}

async fn run_inner<Si, St>(
    close_timeout: Duration,
    ws_tx: Si,
    ws_rx: St,
    client_to_app_tx: Sender<Message>,
    app_to_client_rx: Receiver<Message>,
    on_close_tx: OnceSender<(Option<CloseFrame<'static>>, Option<Error>)>,
) where
    Si: Sink<tungstenite::Message, Error = Error> + Unpin + Send + 'static,
    St: Stream<Item = Result<tungstenite::Message, Error>> + Unpin + Send + 'static,
{
    let (error_tx, mut error_rx) = mpsc::channel(1);
    let (close_frame_tx, mut close_frame_rx) = mpsc::channel(1);

    let cancellation_token = CancellationToken::new();

    // Run reader and writer as different tasks to avoid potential
    // deadlock. A simpler approach would have been to `select` over
    // both receivers (from the websocket and from the application) in
    // the same future however this could lead to a deadlock if both
    // the client and application `await` on a send to each other when
    // both channels are full.
    let reader_task = spawn_reader_task(
        cancellation_token.clone(),
        close_frame_tx,
        error_tx.clone(),
        client_to_app_tx,
        ws_rx,
    );

    let writer_task = spawn_writer_task(
        cancellation_token.clone(),
        error_tx,
        app_to_client_rx,
        ws_tx,
    );

    let (reader_result, writer_result) = join!(reader_task, writer_task);

    // Tasks aren't cancelled so errors can only occur on panic, so unwrap to forward those
    let (ws_rx, ws_tx) = (reader_result.unwrap(), writer_result.unwrap());

    close_websocket(close_timeout, ws_tx, ws_rx).await;

    // Let the application know we've closed, and forward the close
    // frame and any errors
    let seen_close_frame = close_frame_rx.try_recv().ok();
    let seen_error = error_rx.try_recv().ok();

    on_close_tx.send((seen_close_frame, seen_error)).ok();
}

fn spawn_reader_task<S>(
    cancellation_token: CancellationToken,
    close_frame_tx: Sender<CloseFrame<'static>>,
    error_tx: Sender<Error>,
    app_tx: Sender<Message>,
    mut socket: S,
) -> JoinHandle<S>
where
    S: Stream<Item = Result<tungstenite::Message, Error>> + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let cancelled = cancellation_token.cancelled();

        let reader = read_from_socket(close_frame_tx, error_tx, app_tx, &mut socket);

        select! {
            _ = cancelled => {},
            _ = reader => {
                cancellation_token.cancel();
            }
        }

        socket
    })
}

fn spawn_writer_task<S>(
    cancellation_token: CancellationToken,
    error_tx: Sender<Error>,
    mut app_rx: Receiver<Message>,
    mut socket: S,
) -> JoinHandle<S>
where
    S: Sink<tungstenite::Message, Error = Error> + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let cancelled = cancellation_token.cancelled();

        let writer = write_to_socket(error_tx, &mut app_rx, &mut socket);

        select! {
            _ = cancelled => {},
            _ = writer => {
                cancellation_token.cancel();
            }
        }

        close_and_drain_receiver(app_rx).await;

        socket
    })
}

/// Read messages from `socket` and forward them to the application.
async fn read_from_socket<S>(
    close_frame_tx: Sender<CloseFrame<'static>>,
    error_tx: Sender<Error>,
    app_tx: Sender<Message>,
    socket: &mut S,
) where
    S: Stream<Item = Result<tungstenite::Message, Error>> + Unpin,
{
    loop {
        match socket.next().await {
            None => break,
            Some(ws_msg) => match ws_msg {
                Ok(ws_msg) => {
                    let msg = match ws_msg {
                        tungstenite::Message::Text(s) => Message::Text(s),
                        tungstenite::Message::Binary(b) => Message::Binary(b),
                        tungstenite::Message::Ping(_)
                        | tungstenite::Message::Pong(_)
                        | tungstenite::Message::Frame(_) => continue,
                        tungstenite::Message::Close(frame) => {
                            if let Some(f) = frame {
                                close_frame_tx.try_send(f).ok();
                            }
                            continue;
                        }
                    };

                    if app_tx.send(msg).await.is_err() {
                        break;
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
            },
        }
    }
}

/// Read messages from the application via `app_rx` and send them down
/// `socket`.
async fn write_to_socket<S>(error_tx: Sender<Error>, app_rx: &mut Receiver<Message>, socket: &mut S)
where
    S: Sink<tungstenite::Message, Error = Error> + Unpin,
{
    loop {
        match app_rx.recv().await {
            None => break,
            Some(msg_to_send) => {
                let ws_msg = match msg_to_send {
                    Message::Text(s) => tungstenite::Message::Text(s),
                    Message::Binary(b) => tungstenite::Message::Binary(b),
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

/// Handle the websocket close sequence.
async fn close_websocket<Si, St>(timeout: Duration, mut ws_tx: Si, mut ws_rx: St)
where
    Si: Sink<tungstenite::Message, Error = Error> + Unpin,
    St: Stream<Item = Result<tungstenite::Message, Error>> + Unpin,
{
    // TODO: determine the proper close frame we need to send from
    // seen errors, if any.
    let close_websocket = ws_tx.send(tungstenite::Message::Close(None));

    let drain_receiver = async { while ws_rx.next().await.is_some() {} };

    let close_and_drain = async { join!(close_websocket, drain_receiver) };

    time::timeout(timeout, close_and_drain).await.ok();
}

async fn close_and_drain_receiver<T>(mut rx: Receiver<T>) {
    rx.close();

    while rx.recv().await.is_some() {}
}
