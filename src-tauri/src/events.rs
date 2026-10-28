//! Event delivery decoupled from Tauri, so the engine (audio, signal path,
//! scrobbling) can run both under the GUI and in the headless daemon.
//!
//! `emit` mirrors `tauri::Emitter::emit`'s shape (event name + serializable
//! payload, returning a `Result`) so call sites read the same as before.

use serde::Serialize;
use std::sync::Arc;

type EmitFn = dyn Fn(&str, serde_json::Value) + Send + Sync;

#[derive(Clone)]
pub struct EventSink(Arc<EmitFn>);

impl EventSink {
    pub fn new(f: impl Fn(&str, serde_json::Value) + Send + Sync + 'static) -> Self {
        Self(Arc::new(f))
    }

    /// A sink that drops every event. Useful in tests and for components
    /// whose events nobody listens to.
    pub fn noop() -> Self {
        Self::new(|_, _| {})
    }

    pub fn emit<S: Serialize>(&self, event: &str, payload: S) -> Result<(), serde_json::Error> {
        let value = serde_json::to_value(payload)?;
        (self.0)(event, value);
        Ok(())
    }
}

impl std::fmt::Debug for EventSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("EventSink")
    }
}

#[cfg(feature = "gui")]
impl From<tauri::AppHandle> for EventSink {
    fn from(app: tauri::AppHandle) -> Self {
        use tauri::Emitter;
        Self::new(move |event, payload| {
            if let Err(e) = app.emit(event, payload) {
                log::debug!("[events] emit {event} failed: {e}");
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn emit_serializes_payload_and_forwards_name() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = {
            let seen = Arc::clone(&seen);
            EventSink::new(move |name, value| seen.lock().unwrap().push((name.to_string(), value)))
        };
        sink.emit("track-finished", ()).unwrap();
        sink.emit("audio-error", serde_json::json!({ "kind": "x" })).unwrap();
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0], ("track-finished".to_string(), serde_json::Value::Null));
        assert_eq!(seen[1].1["kind"], "x");
    }
}
