//! Live events for the apps, sent over `GET /events`.
//!
//! Events only tell the apps what to reload. They carry IDs and kinds, never
//! report texts or tool arguments, so a dropped event loses nothing: the apps
//! read the data itself from the API.

use serde::Serialize;
use serde_json::Value;
use tokio::sync::broadcast;

/// Events kept for slow listeners before the oldest are dropped.
const CAPACITY: usize = 256;

/// One event.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AppEvent {
    /// Event name, such as `run.finished` or `tasks.changed`.
    pub name: String,
    /// Small JSON payload with IDs.
    pub data: Value,
}

impl AppEvent {
    /// Creates an event.
    pub fn new(name: impl Into<String>, data: Value) -> Self {
        Self {
            name: name.into(),
            data,
        }
    }
}

/// Fans events out to every connected app.
#[derive(Clone, Debug)]
pub struct EventBus {
    sender: broadcast::Sender<AppEvent>,
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

impl EventBus {
    /// Creates a bus without listeners.
    pub fn new() -> Self {
        Self {
            sender: broadcast::channel(CAPACITY).0,
        }
    }

    /// Sends an event. Without listeners, it is dropped.
    pub fn publish(&self, event: AppEvent) {
        let _ = self.sender.send(event);
    }

    /// Sends an event built from a name and a payload.
    pub fn emit(&self, name: &str, data: Value) {
        self.publish(AppEvent::new(name, data));
    }

    /// Starts listening.
    pub fn subscribe(&self) -> broadcast::Receiver<AppEvent> {
        self.sender.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn listeners_receive_events_in_order() {
        let bus = EventBus::default();
        bus.emit("dropped", json!({}));
        let mut receiver = bus.subscribe();
        bus.emit("first", json!({"id": 1}));
        bus.publish(AppEvent::new("second", json!({})));
        assert_eq!(receiver.recv().await.unwrap().name, "first");
        assert_eq!(receiver.recv().await.unwrap().name, "second");
    }
}
