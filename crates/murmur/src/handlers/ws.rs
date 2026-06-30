//! `GET /ws` — the live message/presence stream.
//!
//! SSO-gated like everything else: the user's identity is resolved from the gateway headers
//! BEFORE the upgrade, so an unauthenticated upgrade is refused with `401`. After upgrade, the
//! socket subscribes (via the in-process [`Hub`](crate::hub::Hub)) to every room the user belongs
//! to AT CONNECT TIME and forwards each room frame to the client. A page reload re-subscribes, so
//! a freshly-joined room starts streaming on the next connect — the DB remains the source of
//! truth, so nothing is lost in the meantime.
//!
//! Concurrency: the socket is split into a send half and a receive half. One forwarder task per
//! subscribed room drains that room's `broadcast::Receiver` into a shared `mpsc`; the main loop
//! `select!`s between that `mpsc` (-> push to client) and the client's own frames (close /
//! keepalive). A slow client can never block a publisher — `broadcast` drops the laggard's oldest
//! frames (`Lagged`, which the forwarder skips).

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::Response;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{broadcast, mpsc};

use crate::auth;
use crate::error::AppError;
use crate::handlers::presence_frame;
use crate::{ensure_lobby, AppState};

/// Aggregator capacity between the per-room forwarders and the socket send loop.
const FORWARD_BUFFER: usize = 256;

/// Upgrade handler. Resolves identity first (401 if absent), then hands the socket to
/// [`handle_socket`].
pub async fn ws_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Result<Response, AppError> {
    let (sub, email) = auth::require_user(&headers)?;
    // Make sure the lobby membership exists so a brand-new user's socket has at least one room.
    ensure_lobby(&state, &sub, &email).await;
    Ok(ws.on_upgrade(move |socket| handle_socket(socket, state, sub, email)))
}

async fn handle_socket(socket: WebSocket, state: AppState, _sub: String, email: String) {
    // Snapshot the user's rooms at connect time.
    let rooms = state.store.list_user_rooms(&_sub).await;
    let room_ids: Vec<String> = rooms.into_iter().map(|r| r.room.id).collect();

    let (mut ws_tx, mut ws_rx) = socket.split();
    let (tx, mut rx) = mpsc::channel::<String>(FORWARD_BUFFER);

    // One forwarder per subscribed room: broadcast receiver -> shared mpsc.
    let mut forwarders = Vec::with_capacity(room_ids.len());
    for rid in &room_ids {
        let mut bcast = state.hub.subscribe(rid);
        let txc = tx.clone();
        forwarders.push(tokio::spawn(async move {
            loop {
                match bcast.recv().await {
                    Ok(frame) => {
                        if txc.send(frame).await.is_err() {
                            break; // socket gone — stop forwarding.
                        }
                    }
                    // Laggard: we dropped some frames; keep going with the newest.
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }));
    }
    // Keep `tx` alive in this scope so `rx` stays open even when the user has zero rooms (the
    // socket then simply parks until the client disconnects). Forwarders hold their own clones.

    // Announce presence (best-effort) to the user's rooms.
    for rid in &room_ids {
        state.hub.publish(rid, presence_frame(rid, &email, true));
    }

    // Main pump: forward hub frames out; watch the client for close/keepalive.
    loop {
        tokio::select! {
            maybe_frame = rx.recv() => {
                match maybe_frame {
                    Some(frame) => {
                        if ws_tx.send(Message::Text(frame.into())).await.is_err() {
                            break; // client write failed — tear down.
                        }
                    }
                    None => break, // all senders dropped (cannot happen while `tx` is held).
                }
            }
            inbound = ws_rx.next() => {
                match inbound {
                    Some(Ok(Message::Close(_))) | None => break,
                    // Inbound text/ping/pong are treated as keepalive and ignored (the client
                    // sends via the REST API, not the socket).
                    Some(Ok(_)) => {}
                    Some(Err(_)) => break,
                }
            }
        }
    }

    // Announce offline + stop forwarders.
    for rid in &room_ids {
        state.hub.publish(rid, presence_frame(rid, &email, false));
    }
    for f in forwarders {
        f.abort();
    }
}
