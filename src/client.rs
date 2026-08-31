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
use tokio_util::sync::CancellationToken;
use tower::retry::backoff::{Backoff, MakeBackoff};
use yawc::{
    WebSocketError,
    frame::{Frame, OpCode},
};

use crate::{config::Config, connection::Connector};

pub use crate::message::{CloseCode, CloseFrame, Message, Utf8Bytes};

#[derive(Debug)]
pub struct NewConnection {
    pub channels: (Sender<Message>, Receiver<Message>),
    pub on_close: OnceReceiver<(Option<CloseFrame>, Option<WebSocketError>)>,
}

#[derive(Debug)]
pub enum ConnectError {
    Inner(WebSocketError),
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

impl From<WebSocketError> for ConnectError {
    fn from(e: WebSocketError) -> Self {
        Self::Inner(e)
    }
}

#[derive(Debug)]
pub struct ReconnectingClient {
    connection_tx: Sender<Result<NewConnection, ConnectError>>,
}

impl ReconnectingClient {
    pub fn new() -> (Self, Receiver<Result<NewConnection, ConnectError>>) {
        let (connection_tx, connection_rx) = mpsc::channel(1);

        (Self { connection_tx }, connection_rx)
    }

    pub async fn connect<C, B>(
        self,
        config: Config,
        url: Uri,
        connection_builder: C,
        backoff_builder: Option<B>,
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
            self.connection_tx,
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
        let permit = if let Ok(p) = listener.reserve().await {
            p
        } else {
            return;
        };

        let res = connect(&config, &url, &mut connection_builder).await;

        match res {
            Ok((ws_tx, ws_rx)) => {
                backoff = backoff_builder.as_mut().map(|b| b.make_backoff());

                let (to_client_tx, to_client_rx) = mpsc::channel(config.channel_size());
                let (to_app_tx, to_app_rx) = mpsc::channel(config.channel_size());
                let (on_close_tx, on_close_rx) = oneshot::channel();

                // Connection has been successful, pass the channels
                // to the application
                permit.send(Ok(NewConnection {
                    channels: (to_client_tx, to_app_rx),
                    on_close: on_close_rx,
                }));

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
                permit.send(Err(e));
            }
        }

        if let Some(b) = &mut backoff {
            b.next_backoff().await;
        }
    }
}

/// Try open a connection to `url`.
async fn connect<C>(
    config: &Config,
    url: &Uri,
    connection_builder: &mut C,
) -> Result<(C::Sender, C::Receiver), ConnectError>
where
    C: Connector,
{
    let connection =
        connection_builder.connect(config.ws_config().cloned(), url, config.disable_nagle());

    if let Ok(res) = time::timeout(config.connect_timeout(), connection).await {
        res.map_err(Into::into)
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
    on_close_tx: OnceSender<(Option<CloseFrame>, Option<WebSocketError>)>,
) where
    Si: Sink<Frame, Error = WebSocketError> + Unpin + Send + 'static,
    St: Stream<Item = Result<Frame, WebSocketError>> + Unpin + Send + 'static,
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
    //
    // Only the reader cancels the writer, which would otherwise wait on the
    // application forever once the socket is gone. Cancelling the reader in
    // turn would make what the application learns about the close depend on
    // when it drops its channels, as the frames still unread at that point
    // include the peer's close frame and any protocol error.
    let mut reader_task = spawn_reader_task(
        cancellation_token.clone(),
        close_frame_tx,
        error_tx.clone(),
        client_to_app_tx,
        ws_rx,
    );

    let writer_task = spawn_writer_task(cancellation_token, error_tx, app_to_client_rx, ws_tx);

    // The writer finishing means the application has stopped sending, either
    // because it dropped its sender or because the reader cancelled it.
    // Tasks aren't cancelled so errors can only occur on panic, so unwrap to forward those
    let mut ws_tx = writer_task.await.unwrap();

    // Send a close frame so the peer closes in turn, and give the reader until
    // `close_timeout` to see its close and reach end of stream.
    //
    // Note this sends a close frame rather than closing the sink: shutting the
    // sink down would also shut down the socket's write side, which the reader
    // still needs to flush the close frame yawc echoes on its behalf.
    let (_, drained) = join!(
        time::timeout(
            close_timeout,
            ws_tx.send(Frame::close(CloseCode::Normal, []))
        ),
        time::timeout(close_timeout, &mut reader_task),
    );

    match drained {
        Ok(reader_result) => {
            reader_result.unwrap();
        }
        // A peer that never closes must not wedge the connection loop.
        Err(_) => reader_task.abort(),
    }

    // Let the application know we've closed, and forward the close
    // frame and any errors
    let seen_close_frame = close_frame_rx.try_recv().ok();
    let seen_error = error_rx.try_recv().ok();

    on_close_tx.send((seen_close_frame, seen_error)).ok();
}

/// Reads until the socket ends, cancelling the writer on its way out.
fn spawn_reader_task<S>(
    cancellation_token: CancellationToken,
    close_frame_tx: Sender<CloseFrame>,
    error_tx: Sender<WebSocketError>,
    app_tx: Sender<Message>,
    mut socket: S,
) -> JoinHandle<()>
where
    S: Stream<Item = Result<Frame, WebSocketError>> + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let _cancel_writer_on_drop = cancellation_token.drop_guard();

        read_from_socket(close_frame_tx, error_tx, app_tx, &mut socket).await;
    })
}

/// Writes until the application stops sending, or until the reader reports
/// that the socket has gone.
fn spawn_writer_task<S>(
    cancellation_token: CancellationToken,
    error_tx: Sender<WebSocketError>,
    mut app_rx: Receiver<Message>,
    mut socket: S,
) -> JoinHandle<S>
where
    S: Sink<Frame, Error = WebSocketError> + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let cancelled = cancellation_token.cancelled();

        let writer = write_to_socket(error_tx, &mut app_rx, &mut socket);

        select! {
            _ = cancelled => {},
            _ = writer => {}
        }

        close_and_drain_receiver(app_rx).await;

        socket
    })
}

/// Read messages from `socket` and forward them to the application.
///
/// Payloads are handed over as the [`bytes::Bytes`] yawc read them into, so
/// no message data is copied on this path.
async fn read_from_socket<S>(
    close_frame_tx: Sender<CloseFrame>,
    error_tx: Sender<WebSocketError>,
    app_tx: Sender<Message>,
    socket: &mut S,
) where
    S: Stream<Item = Result<Frame, WebSocketError>> + Unpin,
{
    loop {
        match socket.next().await {
            None => break,
            Some(ws_frame) => match ws_frame {
                Ok(ws_frame) => {
                    let msg = match ws_frame.opcode() {
                        // yawc only validates text as UTF-8 when
                        // `Options::check_utf8` is set, so validate here
                        // instead: it is the scan `Utf8Bytes` needs anyway.
                        OpCode::Text => match Utf8Bytes::try_from(ws_frame.into_payload()) {
                            Ok(text) => Message::Text(text),
                            Err(_) => {
                                error_tx.try_send(WebSocketError::InvalidUTF8).ok();
                                break;
                            }
                        },
                        OpCode::Binary => Message::Binary(ws_frame.into_payload()),
                        // Pings are answered by yawc, and continuations are
                        // assembled before they reach us
                        OpCode::Ping | OpCode::Pong | OpCode::Continuation => continue,
                        OpCode::Close => {
                            if let Some(f) = close_frame(ws_frame) {
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
                    if e.is_closed() {
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

/// Split a close frame into the status code and reason the application sees.
///
/// A peer may close without a status code, in which case the payload is empty
/// and there is nothing to report.
fn close_frame(frame: Frame) -> Option<CloseFrame> {
    let code = frame.close_code()?;

    // The first two bytes are the status code; the rest, if any, is the
    // reason. Slicing shares the payload rather than copying it.
    let reason = Utf8Bytes::try_from(frame.into_payload().slice(2..)).unwrap_or_default();

    Some(CloseFrame { code, reason })
}

/// Read messages from the application via `app_rx` and send them down
/// `socket`.
async fn write_to_socket<S>(
    error_tx: Sender<WebSocketError>,
    app_rx: &mut Receiver<Message>,
    socket: &mut S,
) where
    S: Sink<Frame, Error = WebSocketError> + Unpin,
{
    loop {
        match app_rx.recv().await {
            None => break,
            Some(msg_to_send) => {
                let ws_frame = match msg_to_send {
                    Message::Text(s) => Frame::text(s.into_bytes()),
                    Message::Binary(b) => Frame::binary(b),
                };

                if let Err(e) = socket.send(ws_frame).await {
                    if e.is_closed() {
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

async fn close_and_drain_receiver<T>(mut rx: Receiver<T>) {
    rx.close();

    while rx.recv().await.is_some() {}
}
