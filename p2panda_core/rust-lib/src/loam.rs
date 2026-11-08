//! A `DeliveryBus` over loam_core, the shared Logos transport of the Basecamp host (the Logos
//! side of bridges). The same as reflection_core's.
//!
//! loam_core owns the host's one Delivery node, and Scala, Kith and the others send through it.
//! Every call to it is Logos IPC, which waits for the host's module thread. So this module
//! never calls loam_core from the engine: a dedicated thread does, and the bus only queues
//! commands to it. Received messages arrive on a second thread, from loam_core's `received`
//! event.
use std::collections::BTreeSet;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use logos_p2panda::{BoxFuture, BusError, BusMessage, DeliveryBus};
use serde_json::{json, Value};
use tokio::sync::broadcast;

pub enum Command {
    Join(String),
    Send(String, Vec<u8>),
}

pub struct LoamBus {
    commands: mpsc::Sender<Command>,
    messages: broadcast::Sender<BusMessage>,
}

impl LoamBus {
    pub fn new() -> (Arc<Self>, mpsc::Receiver<Command>) {
        let (commands, rx) = mpsc::channel();
        let (messages, _) = broadcast::channel(4096);
        (Arc::new(Self { commands, messages }), rx)
    }

    pub fn deliver(&self, content_topic: String, payload_b64: &str) {
        let Some(payload) = decode_b64(payload_b64) else {
            tracing::warn!("loam_core delivered a payload that is not base64");
            return;
        };
        let _ = self.messages.send(BusMessage {
            content_topic,
            payload,
        });
    }
}

impl DeliveryBus for LoamBus {
    fn subscribe(&self, content_topic: String) -> BoxFuture<'_, Result<(), BusError>> {
        let result = self
            .commands
            .send(Command::Join(content_topic))
            .map_err(|_| BusError("transport thread stopped".into()));
        Box::pin(async move { result })
    }

    fn unsubscribe(&self, _content_topic: String) -> BoxFuture<'_, Result<(), BusError>> {
        // loam_core has no leave; other topics' traffic is filtered by the backend.
        Box::pin(async { Ok(()) })
    }

    fn publish(
        &self,
        content_topic: String,
        payload: Vec<u8>,
    ) -> BoxFuture<'_, Result<(), BusError>> {
        let result = self
            .commands
            .send(Command::Send(content_topic, payload))
            .map_err(|_| BusError("transport thread stopped".into()));
        Box::pin(async move { result })
    }

    fn messages(&self) -> broadcast::Receiver<BusMessage> {
        self.messages.subscribe()
    }
}

/// loam_core encodes payloads with either base64 alphabet, padded or not.
fn decode_b64(s: &str) -> Option<Vec<u8>> {
    let normalized: String = s
        .trim()
        .trim_matches('"')
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '=')
        .map(|c| match c {
            '-' => '+',
            '_' => '/',
            c => c,
        })
        .collect();
    base64::engine::general_purpose::STANDARD_NO_PAD
        .decode(normalized)
        .ok()
}

pub fn encode_b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// What the transport thread needs from loam_core. The module implements it with the
/// generated client; tests use a fake.
pub trait LoamApi: Send + 'static {
    fn start(&mut self) -> Result<(), String>;
    fn status(&mut self) -> Result<String, String>;
    fn metrics(&mut self) -> Result<String, String>;
    fn join(&mut self, topic: &str) -> Result<(), String>;
    fn send(&mut self, topic: &str, payload_b64: &str) -> Result<(), String>;
}

const MAX_QUEUED: usize = 2000;

/// The transport thread: start loam_core, wait until it is connected, then join and send.
/// `report` receives the transport state for the view.
pub fn run<A: LoamApi>(mut api: A, commands: mpsc::Receiver<Command>, report: impl Fn(Value)) {
    let mut joined: BTreeSet<String> = BTreeSet::new();
    let mut queue: Vec<(String, Vec<u8>)> = Vec::new();
    let mut connected = false;
    let mut status = String::from("starting");
    let mut last_error: Option<String> = None;
    let mut metrics = Value::Null;
    let mut started = false;
    let mut last_poll = Instant::now() - Duration::from_secs(60);
    let mut last_metrics = Instant::now() - Duration::from_secs(60);

    loop {
        if !started {
            match api.start() {
                Ok(()) => started = true,
                Err(error) => {
                    last_error = Some(format!("loam_core start: {error}"));
                    status = "loam_core unavailable".into();
                }
            }
        }

        if last_poll.elapsed() >= Duration::from_secs(if connected { 5 } else { 1 }) {
            last_poll = Instant::now();
            match api.status() {
                Ok(s) => {
                    let s = s.trim().trim_matches('"').to_owned();
                    let now_connected = s.contains("Connected");
                    if now_connected && !connected {
                        // (Re)join everything: loam_core may have restarted its node.
                        for topic in &joined {
                            if let Err(error) = api.join(topic) {
                                last_error = Some(format!("join: {error}"));
                            }
                        }
                        for (topic, payload) in queue.drain(..) {
                            if let Err(error) = api.send(&topic, &encode_b64(&payload)) {
                                last_error = Some(format!("send: {error}"));
                            }
                        }
                    }
                    connected = now_connected;
                    status = s;
                }
                Err(error) => {
                    last_error = Some(format!("status: {error}"));
                    if started {
                        status = "loam_core not answering".into();
                    }
                }
            }
            if last_metrics.elapsed() >= Duration::from_secs(10) {
                last_metrics = Instant::now();
                if let Ok(m) = api.metrics() {
                    metrics = serde_json::from_str(m.trim()).unwrap_or(Value::String(m));
                }
            }
            report(json!({
                "status": status,
                "connected": connected,
                "joined": joined.len(),
                "queued": queue.len(),
                "error": last_error,
                "metrics": metrics,
            }));
        }

        match commands.recv_timeout(Duration::from_millis(500)) {
            Ok(Command::Join(topic)) => {
                if joined.insert(topic.clone()) && started {
                    // Joining before the node is up is fine: loam_core remembers the topic.
                    if let Err(error) = api.join(&topic) {
                        last_error = Some(format!("join: {error}"));
                    }
                }
            }
            Ok(Command::Send(topic, payload)) => {
                if connected {
                    if let Err(error) = api.send(&topic, &encode_b64(&payload)) {
                        last_error = Some(format!("send: {error}"));
                    }
                } else if queue.len() < MAX_QUEUED {
                    queue.push((topic, payload));
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_both_alphabets() {
        let bytes: Vec<u8> = (0..=255).collect();
        let standard = base64::engine::general_purpose::STANDARD.encode(&bytes);
        let url = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&bytes);
        assert_eq!(decode_b64(&standard).unwrap(), bytes);
        assert_eq!(decode_b64(&url).unwrap(), bytes);
        assert_eq!(decode_b64(&format!("\"{standard}\"")).unwrap(), bytes);
    }
}
