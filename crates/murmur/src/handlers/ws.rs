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
use std::collections::HashSet;
use tokio::sync::{broadcast, mpsc};
use tokio::time::{interval, Duration, MissedTickBehavior};

use crate::auth;
use crate::error::AppError;
use crate::handlers::presence_frame;
use crate::{ensure_lobby, AppState};

/// Aggregator capacity between the per-room forwarders and the socket send loop.
const FORWARD_BUFFER: usize = 256;
/// Bounded idle revocation latency for a socket that receives no Hub frames.
const STORE_REAUTHORIZE_SECS: u64 = 5;

struct ForwardedFrame {
    room_id: String,
    generation: crate::hub::HubGeneration,
    frame: Option<String>,
}

/// Upgrade handler. Resolves identity first (401 if absent), then hands the socket to
/// [`handle_socket`].
pub async fn ws_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Result<Response, AppError> {
    let (sub, email) = auth::require_user(&headers)?;
    auth::verify_websocket_origin(&headers, state.config.websocket_origin.as_deref())?;
    // Make sure the lobby membership exists so a brand-new user's socket has at least one room.
    ensure_lobby(&state, &sub, &email).await?;
    let rooms = state.store.list_user_rooms(&sub).await?;
    let room_ids: Vec<String> = rooms
        .into_iter()
        .filter(|room| !room.room.archived)
        .map(|room| room.room.id)
        .collect();
    Ok(ws.on_upgrade(move |socket| handle_socket(socket, state, sub, room_ids, email)))
}

async fn handle_socket(
    socket: WebSocket,
    state: AppState,
    sub: String,
    room_ids: Vec<String>,
    email: String,
) {
    let (mut ws_tx, mut ws_rx) = socket.split();
    let (tx, mut rx) = mpsc::channel::<ForwardedFrame>(FORWARD_BUFFER);

    // One forwarder per subscribed room: broadcast receiver -> shared mpsc.
    let mut forwarders = Vec::with_capacity(room_ids.len());
    for rid in &room_ids {
        let subscription = state.hub.subscribe(rid);
        let bcast = subscription.receiver;
        let subscribed_generation = subscription.generation;
        let room_id = rid.clone();
        let txc = tx.clone();
        forwarders.push(tokio::spawn(forward_room_frames(
            bcast,
            subscribed_generation,
            room_id,
            txc,
        )));
    }
    // Close the snapshot -> subscribe race. Admin revocation writes Store before invalidating the
    // Hub: a revoke before the last subscribe is visible here, while a later revoke is observed by
    // the already-live generation receiver above.
    if !subscriptions_still_authorized(&state, &sub, &room_ids).await {
        for forwarder in forwarders {
            forwarder.abort();
        }
        return;
    }
    // Keep `tx` alive in this scope so `rx` stays open even when the user has zero rooms (the
    // socket then simply parks until the client disconnects). Forwarders hold their own clones.

    // Announce presence (best-effort) to the user's rooms.
    for rid in &room_ids {
        state.hub.publish(rid, presence_frame(rid, &email, true));
    }

    let mut reauthorize = interval(Duration::from_secs(STORE_REAUTHORIZE_SECS));
    reauthorize.set_missed_tick_behavior(MissedTickBehavior::Delay);
    // Consume the immediate first tick; the post-subscribe Store check above already covered it.
    reauthorize.tick().await;

    // Main pump: forward hub frames out; watch the client for close/keepalive.
    loop {
        tokio::select! {
            maybe_frame = rx.recv() => {
                match maybe_frame {
                    Some(forwarded) => {
                        // Never write a queued frame from an authorization generation that has
                        // since been invalidated. Closing forces a fresh Store authorization.
                        if forwarded.generation != state.hub.generation(&forwarded.room_id) {
                            break;
                        }
                        // The Hub generation is process-local. Canonical Store reauthorization
                        // before every protected frame closes cross-process remove/ban/archive/
                        // delete races and fails closed on backend uncertainty.
                        if let Err(error) = state
                            .store
                            .authorize_room_read(&forwarded.room_id, &sub, true)
                            .await
                        {
                            tracing::warn!(
                                error = %error,
                                room_id = %forwarded.room_id,
                                "closing websocket after Store reauthorization failure"
                            );
                            break;
                        }
                        let Some(frame) = forwarded.frame else {
                            break;
                        };
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
            _ = reauthorize.tick() => {
                if !subscriptions_still_authorized(&state, &sub, &room_ids).await {
                    break;
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

async fn forward_room_frames(
    mut receiver: broadcast::Receiver<crate::hub::HubEnvelope>,
    subscribed_generation: crate::hub::HubGeneration,
    room_id: String,
    tx: mpsc::Sender<ForwardedFrame>,
) {
    loop {
        match receiver.recv().await {
            Ok(envelope) => {
                // An invalidation or post-invalidation frame must wake the main socket pump. A
                // silent forwarder exit would leave the original sender alive and park the socket
                // forever, so enqueue an internal sentinel before stopping.
                if envelope.generation != subscribed_generation {
                    let _ = tx
                        .send(ForwardedFrame {
                            room_id,
                            generation: envelope.generation,
                            frame: None,
                        })
                        .await;
                    break;
                }
                if tx
                    .send(ForwardedFrame {
                        room_id: room_id.clone(),
                        generation: envelope.generation,
                        frame: envelope.frame,
                    })
                    .await
                    .is_err()
                {
                    break; // socket gone — stop forwarding.
                }
            }
            // Laggard: we dropped some frames; keep going with the newest.
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
}

async fn subscriptions_still_authorized(
    state: &AppState,
    user_sub: &str,
    subscribed_room_ids: &[String],
) -> bool {
    let Ok(rooms) = state.store.list_user_rooms(user_sub).await else {
        return false;
    };
    let active: HashSet<String> = rooms
        .into_iter()
        .filter(|room| !room.room.archived)
        .map(|room| room.room.id)
        .collect();
    subscribed_room_ids
        .iter()
        .all(|room_id| active.contains(room_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Room;

    #[tokio::test]
    async fn post_subscribe_recheck_catches_revocation_missed_by_generation_snapshot() {
        let state = crate::build_dev_state();
        let room = Room {
            id: "room_ws_race".to_string(),
            name: "#race".to_string(),
            kind: "room".to_string(),
            created_by: "u_alice".to_string(),
            created_at: 1,
            archived: false,
            topic: String::new(),
        };
        state.store.ensure_room(&room).await.unwrap();
        state
            .store
            .ensure_membership(&room.id, "u_alice", "alice@hf", 1)
            .await
            .unwrap();
        let snapshot: Vec<String> = state
            .store
            .list_user_rooms("u_alice")
            .await
            .unwrap()
            .into_iter()
            .map(|user_room| user_room.room.id)
            .collect();
        assert_eq!(snapshot, vec![room.id.clone()]);

        // Revocation commits and invalidates before the socket subscribes. A generation-only
        // implementation samples the new generation and would therefore accept it indefinitely.
        state.store.ban_member(&room.id, "u_alice").await.unwrap();
        state.hub.invalidate(&room.id);
        let late_subscription = state.hub.subscribe(&room.id);
        assert_eq!(late_subscription.generation, state.hub.generation(&room.id));
        assert!(!subscriptions_still_authorized(&state, "u_alice", &snapshot).await);
    }

    #[tokio::test]
    async fn idle_invalidation_enqueues_socket_close_sentinel() {
        let state = crate::build_dev_state();
        let subscription = state.hub.subscribe("room_idle");
        let subscribed_generation = subscription.generation;
        let (tx, mut rx) = mpsc::channel(1);
        let forwarder = tokio::spawn(forward_room_frames(
            subscription.receiver,
            subscribed_generation,
            "room_idle".to_string(),
            tx,
        ));

        let invalidated_generation = state.hub.invalidate("room_idle");
        let forwarded = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("invalidation must wake the socket pump")
            .expect("forwarder must send a close sentinel");
        assert_eq!(forwarded.room_id, "room_idle");
        assert_eq!(forwarded.generation, invalidated_generation);
        assert!(forwarded.frame.is_none());
        assert_ne!(forwarded.generation, subscribed_generation);
        forwarder.await.unwrap();
    }

    #[tokio::test]
    async fn canonical_recheck_catches_cross_process_archive_without_hub_invalidation() {
        let state = crate::build_dev_state();
        let room = Room {
            id: "room_ws_cross_process".to_string(),
            name: "#cross-process".to_string(),
            kind: "room".to_string(),
            created_by: "u_alice".to_string(),
            created_at: 1,
            archived: false,
            topic: String::new(),
        };
        state.store.ensure_room(&room).await.unwrap();
        state
            .store
            .ensure_membership(&room.id, "u_alice", "alice@hf", 1)
            .await
            .unwrap();
        let generation = state.hub.generation(&room.id);
        let subscribed = vec![room.id.clone()];
        assert!(subscriptions_still_authorized(&state, "u_alice", &subscribed).await);

        // Simulate a different process: canonical Store changes but this process's Hub is never
        // invalidated. The idle/per-frame Store check must still close the authority.
        state.store.set_room_archived(&room.id, true).await.unwrap();
        assert_eq!(state.hub.generation(&room.id), generation);
        assert!(!subscriptions_still_authorized(&state, "u_alice", &subscribed).await);
        assert!(matches!(
            state
                .store
                .authorize_room_read(&room.id, "u_alice", true)
                .await,
            Err(crate::store::StoreMutationError::RoomArchived)
        ));
    }
}
