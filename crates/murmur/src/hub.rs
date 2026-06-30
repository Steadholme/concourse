//! In-process live-message fan-out hub, keyed by room.
//!
//! Each room owns a `tokio::sync::broadcast` channel. A connected WebSocket (see
//! [`crate::handlers::ws`]) subscribes to the rooms its user belongs to and forwards every frame
//! it receives to the socket; a `POST .../messages` (and presence transitions) [`publish`] a
//! pre-serialized JSON frame to the room. This is a SINGLE-PROCESS hub: it delivers live updates
//! to clients attached to THIS instance. The database is the source of truth, so a client that
//! missed a live frame (or is attached to another replica) still sees the message on its next
//! `GET .../messages` fetch — the hub is an accelerator, never the system of record.
//!
//! [`publish`] is non-blocking and infallible: `broadcast::Sender::send` returns the receiver
//! count and only errors when there are zero receivers, which we treat as "nobody listening" and
//! ignore. A slow/dead WebSocket can never block a sender — `broadcast` drops the laggard's
//! oldest frames (surfaced as `Lagged`, which the forwarder skips).

use std::collections::HashMap;
use std::sync::Mutex;

use tokio::sync::broadcast;

/// Per-room broadcast ring capacity. Frames beyond this for a lagging receiver are dropped
/// (the receiver sees `Lagged` and continues) rather than growing memory unboundedly.
const ROOM_CHANNEL_CAPACITY: usize = 256;

/// Live fan-out hub. Cheap to share behind an `Arc`; cloning is not needed (handlers hold an
/// `Arc<Hub>` in [`crate::AppState`]).
#[derive(Default)]
pub struct Hub {
    /// Room id -> the room's broadcast sender. Created lazily on first subscribe/publish.
    rooms: Mutex<HashMap<String, broadcast::Sender<String>>>,
}

impl Hub {
    pub fn new() -> Self {
        Self::default()
    }

    /// Get (or lazily create) the broadcast sender for `room_id`.
    fn sender(&self, room_id: &str) -> broadcast::Sender<String> {
        let mut rooms = self.rooms.lock().expect("hub lock poisoned");
        rooms
            .entry(room_id.to_string())
            .or_insert_with(|| broadcast::channel(ROOM_CHANNEL_CAPACITY).0)
            .clone()
    }

    /// Subscribe to a room's live frames. The returned receiver yields every frame published
    /// after this call.
    pub fn subscribe(&self, room_id: &str) -> broadcast::Receiver<String> {
        self.sender(room_id).subscribe()
    }

    /// Publish a pre-serialized JSON `frame` to everyone currently subscribed to `room_id`.
    /// Non-blocking; a room with no subscribers is a silent no-op (and we avoid allocating a
    /// channel for it).
    pub fn publish(&self, room_id: &str, frame: String) {
        // Only publish through an EXISTING channel: if nobody ever subscribed there is nothing to
        // deliver, and we avoid leaking a sender for an unwatched room.
        let sender = {
            let rooms = self.rooms.lock().expect("hub lock poisoned");
            rooms.get(room_id).cloned()
        };
        if let Some(sender) = sender {
            // `send` errors only when there are zero receivers — nothing to do in that case.
            let _ = sender.send(frame);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn subscriber_receives_published_frame() {
        let hub = Hub::new();
        let mut rx = hub.subscribe("room_a");
        hub.publish("room_a", "{\"type\":\"message\"}".to_string());
        let got = rx.recv().await.unwrap();
        assert_eq!(got, "{\"type\":\"message\"}");
    }

    #[tokio::test]
    async fn publish_to_unwatched_room_is_noop() {
        let hub = Hub::new();
        // No subscriber for room_b: publish must not panic and must not allocate a channel.
        hub.publish("room_b", "frame".to_string());
        assert!(!hub.rooms.lock().unwrap().contains_key("room_b"));
    }

    #[tokio::test]
    async fn rooms_are_isolated() {
        let hub = Hub::new();
        let mut rx_a = hub.subscribe("room_a");
        let _rx_b = hub.subscribe("room_b");
        hub.publish("room_b", "b-only".to_string());
        hub.publish("room_a", "a-only".to_string());
        // room_a's receiver only sees room_a's frame.
        assert_eq!(rx_a.recv().await.unwrap(), "a-only");
    }
}
