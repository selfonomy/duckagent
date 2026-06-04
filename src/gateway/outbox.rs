use super::GatewayRoute;
use serde::Serialize;
use serde_json::Value;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Serialize)]
pub(in crate::gateway) struct OutboxEvent {
    id: u64,
    timestamp: String,
    session_id: String,
    channel: String,
    conversation_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    thread_id: Option<String>,
    kind: String,
    payload: Value,
}

#[derive(Clone)]
pub(in crate::gateway) struct GatewayOutbox {
    inner: Arc<Mutex<OutboxState>>,
}

#[derive(Default)]
struct OutboxState {
    next_id: u64,
    events: Vec<OutboxEvent>,
}

impl GatewayOutbox {
    pub(in crate::gateway) fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(OutboxState {
                next_id: 1,
                events: Vec::new(),
            })),
        }
    }

    pub(in crate::gateway) fn push(&self, route: &GatewayRoute, kind: &str, payload: Value) {
        let mut guard = self.inner.lock().expect("gateway outbox mutex poisoned");
        let id = guard.next_id;
        guard.next_id += 1;
        guard.events.push(OutboxEvent {
            id,
            timestamp: chrono::Utc::now().to_rfc3339(),
            session_id: route.session_id.clone(),
            channel: route.key.channel.clone(),
            conversation_id: route.key.conversation_id.clone(),
            thread_id: route.key.thread_id.clone(),
            kind: kind.to_string(),
            payload,
        });
    }

    pub(in crate::gateway) fn list_since(&self, since: Option<u64>) -> Vec<OutboxEvent> {
        let guard = self.inner.lock().expect("gateway outbox mutex poisoned");
        guard
            .events
            .iter()
            .filter(|event| since.is_none_or(|since| event.id > since))
            .cloned()
            .collect()
    }
}
