// Copyright (c) 2024 The Vacon Authors
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 2 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result};
use async_shutdown::ShutdownManager;
use bytes::{BufMut, Bytes, BytesMut};
use clap::{ArgAction, Parser};
use futures_channel::mpsc::{channel, Sender};
use futures_util::{future, pin_mut, stream::TryStreamExt, StreamExt};
use log::*;
use tokio::signal::unix::{signal, SignalKind};
use tokio_listener::{Connection, Listener, ListenerAddressLFlag};
use tokio_tungstenite::tungstenite::{
    handshake::server::{Request, Response},
    http::Method,
    protocol::WebSocketConfig,
    Error, Message,
};

/// WebSocket signaling server for Vacon.
#[derive(Parser)]
struct Args {
    #[clap(flatten)]
    listener: ListenerAddressLFlag,

    /// Exit after a grace period if no sessions are in progress. Useful when combined with socket
    /// activation.
    #[clap(long)]
    exit_on_idle: bool,

    /// Increase verbosity level
    #[clap(short, long, action = ArgAction::Count)]
    verbose: u8,
}

// A connected WebSocket client.
struct Client {
    // Client identifier, taken from the Sec-WebSocket-Key header.
    id: Bytes,

    // Channel for sending WebSocket messages to this client.
    tx: Sender<Message>,
}

// The clients that belong to a session, plus session metadata.
#[derive(Default)]
struct SessionValue {
    // The clients currently connected to this session.
    clients: Vec<Client>,

    // Total number of bytes of text/binary messages that have been sent to the session by clients.
    n_bytes: usize,

    // Total number of text/binary messages that have been sent to the session by clients.
    n_messages: usize,
}

// Map of session IDs to clients.
type SessionMap = Arc<Mutex<HashMap<Bytes, SessionValue>>>;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    stderrlog::new()
        .verbosity(args.verbose as usize)
        .module(module_path!())
        .init()
        .unwrap();

    let listener = args
        .listener
        .bind()
        .await
        .context("No listener specified")??;

    let session_map = SessionMap::new(Mutex::new(HashMap::new()));

    let shutdown = ShutdownManager::new();

    // Handle SIGTERM and SIGINT.
    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    tokio::spawn({
        let shutdown = shutdown.clone();
        async move {
            tokio::select! {
                _ = terminate.recv() => {
                    info!("Received terminate signal, shutting down");
                    shutdown.trigger_shutdown(0).ok();
                },
                _ = interrupt.recv() => {
                    info!("Received interrupt signal, shutting down");
                    shutdown.trigger_shutdown(0).ok();
                }
            }
        }
    });

    if args.exit_on_idle {
        // Every 60 seconds, check if there are any sessions in progress and if not, trigger a
        // shutdown.
        tokio::spawn({
            let session_map = session_map.clone();
            let shutdown = shutdown.clone();
            async move {
                let mut interval = tokio::time::interval(Duration::from_secs(60));

                // The first tick completes immediately.
                interval.tick().await;

                loop {
                    interval.tick().await;
                    {
                        let sessions = session_map.lock().unwrap();
                        trace!("[Sessions] Session map has {} sessions", sessions.len());
                        if sessions.len() == 0 {
                            info!("Shutting down due to idleness");
                            shutdown.trigger_shutdown(0).ok();
                        }
                    }
                }
            }
        });
    }

    // Run the server.
    match run_server(shutdown.clone(), session_map.clone(), listener).await {
        Ok(_) => {
            shutdown.trigger_shutdown(0).ok();
        }
        Err(e) => {
            error!("Server task finished with error: {e}");
            shutdown.trigger_shutdown(1).ok();
        }
    };

    let exit_code = shutdown.wait_shutdown_complete().await;
    std::process::exit(exit_code);
}

async fn run_server(
    shutdown: ShutdownManager<i32>,
    session_map: SessionMap,
    mut listener: Listener,
) -> Result<()> {
    while let Ok(connection) = shutdown.wrap_cancel(listener.accept()).await {
        let (stream, _address) = connection?;
        tokio::spawn(handle_connection(
            shutdown.clone(),
            session_map.clone(),
            stream,
        ));
    }
    Ok(())
}

async fn handle_connection(
    shutdown: ShutdownManager<i32>,
    session_map: SessionMap,
    stream: Connection,
) {
    let _ = match shutdown.delay_shutdown_token() {
        Ok(token) => token,
        Err(_) => {
            error!("Shutdown already started, closing connection");
            return;
        }
    };

    let mut client_id = BytesMut::with_capacity(128);
    let mut session_id = BytesMut::with_capacity(128);

    let callback = |req: &Request, response: Response| {
        trace!("[Request] {} {}", req.method(), req.uri());
        for (ref header, _value) in req.headers() {
            trace!("[Request] > {}: {:?}", header, _value);
        }

        // Check if the method and URI is for our endpoint.
        if req.method() == Method::GET && req.uri().path() == "/api/v1/offer-answer" {
            // Use the Sec-WebSocket-Key header as the client ID.
            if let Some(value) = req.headers().get("sec-websocket-key") {
                client_id.put(value.as_bytes());
            }

            // Use the URI query string as the session ID.
            if let Some(value) = req.uri().query() {
                session_id.put(value.as_bytes());
            }

            // Check that the client and session IDs are at least one byte long.
            if !client_id.is_empty() && !session_id.is_empty() {
                return Ok(response);
            } else {
                // Otherwise return 400 Bad Request.
                return Ok(Response::builder().status(400).body(()).unwrap());
            }
        }

        // Any other methods or URIs are 404 Not Found.
        Ok(Response::builder().status(404).body(()).unwrap())
    };

    // Accept the WebSocket connection using the callback above and with a custom config for
    // enforcing extremely low resource limits.
    let ws_stream = match tokio_tungstenite::accept_hdr_async_with_config(
        stream,
        callback,
        Some(WebSocketConfig {
            write_buffer_size: 2048,
            max_write_buffer_size: 4096,
            max_message_size: Some(8192),
            max_frame_size: Some(4096),
            ..Default::default()
        }),
    )
    .await
    {
        Ok(ws_stream) => ws_stream,
        Err(e) => {
            warn!("Error during WebSocket handshake: {}", e);
            return;
        }
    };

    let client_id = client_id.freeze();
    let session_id = session_id.freeze();

    // Get the number of clients already in the session, excluding the client connection that was
    // just accepted above.
    let n_clients = session_map
        .lock()
        .unwrap()
        .get(&session_id)
        .map_or(0, |s| s.clients.len());

    if n_clients >= 2 {
        warn!("Closing connection from client {client_id:?}, tried to join full session {session_id:?}");
        return;
    }

    debug!("Client {client_id:?} connected to session {session_id:?}");

    // Create a bounded MPSC channel for communicating with this client.
    let (tx, rx) = channel(2);

    // Add the client to the session map under the session ID that the client provided. Create an
    // entry for the session if it doesn't already exist.
    session_map
        .lock()
        .unwrap()
        .entry(session_id.clone())
        .or_default()
        .clients
        .push(Client {
            id: client_id.clone(),
            tx,
        });

    let (outgoing, incoming) = ws_stream.split();

    // If there's already another client waiting in this session, send the session start indicator
    // to the other client to kick off the offer/answer. The session start indicator is a 1-byte
    // binary message with the value 0.
    if n_clients >= 1 {
        let mut sessions = session_map.lock().unwrap();
        if let Some(session) = sessions.get_mut(&session_id) {
            // Filter out ourself from the clients in this session.
            let mut recipients = session
                .clients
                .iter_mut()
                .filter(|client| client.id != client_id);

            // Get the first client from the list of recipients.
            if let Some(recp) = recipients.next() {
                // Send the session start indicator to the other client's MPSC channel.
                trace!("Sending session start indicator to client {:?}", recp.id);
                if let Err(e) = recp.tx.try_send(Message::Binary(vec![0])) {
                    debug!("try_send() to client {:?} failed: {e}", recp.id);
                }
            }
        }
    }

    // The provided closure here will be called for each item that the incoming stream produces.
    let send_incoming = incoming.try_for_each(|msg| {
        if msg.is_empty() {
            trace!("Received empty message from client {client_id:?}");
        }

        if msg.is_close() {
            trace!("Received close message from client {client_id:?}");
        }

        if msg.is_ping() {
            trace!("Received ping message from client {client_id:?}");
        }

        if msg.is_pong() {
            trace!("Received pong message from client {client_id:?}");
        }

        if msg.is_text() || msg.is_binary() {
            trace!(
                "Received message from client {client_id:?}, length {} bytes",
                msg.len()
            );

            // Look up the session.
            let mut sessions = session_map.lock().unwrap();
            if let Some(session) = sessions.get_mut(&session_id) {
                // Accounting.
                session.n_bytes += msg.len();
                session.n_messages += 1;

                // Enforce quota (max number of messages, max number of bytes) on session.
                if session.n_messages < 25 && session.n_bytes < 25 * 1024 {
                    // Filter out ourself.
                    let recipients = session
                        .clients
                        .iter_mut()
                        .filter(|client| client.id != client_id);

                    // Send the message to the other client(s).
                    for recp in recipients {
                        trace!(
                            "Sending message to client {:?}, length {} bytes",
                            recp.id,
                            msg.len(),
                        );
                        if let Err(e) = recp.tx.try_send(msg.clone()) {
                            // Don't block on the MPSC channel send, ignore errors.
                            debug!("try_send() to client {:?} failed: {e}", recp.id);
                        }
                    }
                } else {
                    warn!(
                        "Session {session_id:?} exceeded quota (n_bytes {}, n_messages {})",
                        session.n_bytes, session.n_messages
                    );
                    return future::err(Error::AttackAttempt);
                }
            }
        }

        future::ok(())
    });

    // Forward messages from the other client to this client.
    let receive_from_others = rx.map(Ok).forward(outgoing);

    // Needed for iteration.
    pin_mut!(send_incoming, receive_from_others);

    // Wait for one of the two futures to complete.
    future::select(send_incoming, receive_from_others).await;

    debug!("Client {client_id:?} disconnected from session {session_id:?}");

    // Remove the client and the session if necessary.
    {
        let mut sessions = session_map.lock().unwrap();

        // Remove the client from the session.
        if let Some(session) = sessions.get_mut(&session_id) {
            session.clients.retain(|client| client.id != client_id);

            // Remove the session from the session map if it has no more clients.
            if session.clients.is_empty() {
                debug!(
                    "Removing session {session_id:?}, n_bytes: {}, n_messages: {}",
                    session.n_bytes, session.n_messages
                );
                sessions.remove(&session_id);
            }
        }
    }
}
