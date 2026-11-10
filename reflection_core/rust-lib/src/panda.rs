//! A `P2pandaBus` over `p2panda_core`, the host's shared p2panda node (ADR 0001).
//!
//! As with loam_core, every call to another module is Logos IPC, which waits for the host's
//! module thread, so the engine never calls p2panda_core itself: a dedicated thread does, and the
//! bus only queues commands to it. p2panda_core's `pending` events say which subscription has
//! items; the thread then drains it. A slow periodic drain covers a lost event.
use std::collections::HashMap;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use p2panda::{Hash, Topic, VerifyingKey};
use reflection_node::external::{BoxFuture, P2pandaBus, P2pandaItem};
use serde_json::Value;
use tokio::sync::{mpsc as tmpsc, oneshot};
use tracing::{info, warn};

/// The app name p2panda_core shows for Reflection.
pub const APP: &str = "reflection";

/// What the IPC thread needs from p2panda_core. The module implements it with the generated
/// client; tests use the engine directly.
pub trait PandaApi: Send + 'static {
    fn network_open(&mut self, network: &str, key: &str) -> Result<Value, String>;
    fn open(&mut self, network: &str, topic: &str, label: &str) -> Result<Value, String>;
    fn drain(&mut self, sub: &str) -> Result<Value, String>;
    fn publish(&mut self, sub: &str, message_b64: &str, prune: bool) -> Result<Value, String>;
    fn ephemeral(&mut self, sub: &str, message_b64: &str) -> Result<Value, String>;
    fn set_active(&mut self, sub: &str, active: bool) -> Result<Value, String>;
    fn import(&mut self, sub: &str, header_b64: &str, body_b64: &str) -> Result<Value, String>;
    fn bridge(&mut self, sub: &str, on: bool) -> Result<Value, String>;
    fn bridges(&mut self) -> Result<Value, String>;
}

type Reply<T> = oneshot::Sender<Result<T, String>>;

pub enum Command {
    NetworkOpen {
        network: String,
        key: String,
        reply: Reply<()>,
    },
    Open {
        topic: Topic,
        reply: Reply<tmpsc::UnboundedReceiver<P2pandaItem>>,
    },
    Publish {
        topic: Topic,
        message: Vec<u8>,
        prune: bool,
        reply: Reply<()>,
    },
    Ephemeral {
        topic: Topic,
        message: Vec<u8>,
        reply: Reply<()>,
    },
    SetActive {
        topic: Topic,
        active: bool,
    },
    /// Import a signed operation from Logos (networks "both").
    Import {
        topic: Topic,
        header: Vec<u8>,
        body: Option<Vec<u8>>,
        reply: Reply<()>,
    },
    Bridge {
        topic: Topic,
        on: bool,
        reply: Reply<String>,
    },
    /// Bridge states by topic hex.
    Bridges {
        reply: Reply<HashMap<String, String>>,
    },
    /// p2panda_core says the subscription with this tag has items.
    Pending(String),
    /// p2panda_core (re)started: open everything again.
    Restarted,
    /// The label (note name) the p2panda view shows, used when the note is opened.
    Label {
        topic: Topic,
        label: String,
    },
}

/// What the p2panda side looks like, for the user: whether the network is open, and the peers
/// p2panda synced with lately (from the subscriptions' sync events).
#[derive(Default)]
pub struct PandaStatus {
    pub open: bool,
    pub error: Option<String>,
    pub peers: HashMap<String, Instant>,
}

/// A peer counts as connected this long after its last sync session event.
const PEER_FRESH: Duration = Duration::from_secs(300);

impl PandaStatus {
    pub fn to_json(&self) -> Value {
        let peers = self
            .peers
            .values()
            .filter(|at| at.elapsed() < PEER_FRESH)
            .count();
        serde_json::json!({
            "open": self.open,
            "connected": self.open && peers > 0,
            "peers": peers,
            "error": self.error,
        })
    }
}

/// The bus the engine sees.
pub struct PandaBus {
    commands: mpsc::Sender<Command>,
    pub status: Arc<std::sync::Mutex<PandaStatus>>,
}

impl PandaBus {
    pub fn new() -> (Arc<Self>, mpsc::Receiver<Command>) {
        let (commands, rx) = mpsc::channel();
        (
            Arc::new(Self {
                commands,
                status: Arc::default(),
            }),
            rx,
        )
    }

    pub fn sender(&self) -> mpsc::Sender<Command> {
        self.commands.clone()
    }

    async fn ask<T>(&self, make: impl FnOnce(Reply<T>) -> Command) -> Result<T, String> {
        let (tx, rx) = oneshot::channel();
        self.commands
            .send(make(tx))
            .map_err(|_| "p2panda thread stopped".to_owned())?;
        rx.await.map_err(|_| "p2panda thread stopped".to_owned())?
    }

    /// Start the network's node in p2panda_core, signing with our key.
    pub async fn network_open(&self, network: String, key: String) -> Result<(), String> {
        self.ask(|reply| Command::NetworkOpen {
            network,
            key,
            reply,
        })
        .await
    }

    /// Ask to bridge a note to Logos (pending until the user confirms it in the p2panda view).
    pub async fn bridge(&self, topic: Topic, on: bool) -> Result<String, String> {
        self.ask(|reply| Command::Bridge { topic, on, reply }).await
    }

    pub async fn bridges(&self) -> Result<HashMap<String, String>, String> {
        self.ask(|reply| Command::Bridges { reply }).await
    }

    /// Name a note for the p2panda view; call before it is opened.
    pub fn set_label(&self, topic: Topic, label: String) {
        let _ = self.commands.send(Command::Label { topic, label });
    }
}

impl P2pandaBus for PandaBus {
    fn open(
        &self,
        topic: Topic,
    ) -> BoxFuture<'_, Result<tmpsc::UnboundedReceiver<P2pandaItem>, String>> {
        Box::pin(self.ask(move |reply| Command::Open { topic, reply }))
    }

    fn publish(
        &self,
        topic: Topic,
        message: Vec<u8>,
        prune: bool,
    ) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(self.ask(move |reply| Command::Publish {
            topic,
            message,
            prune,
            reply,
        }))
    }

    fn ephemeral(&self, topic: Topic, message: Vec<u8>) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(self.ask(move |reply| Command::Ephemeral {
            topic,
            message,
            reply,
        }))
    }

    fn set_active(&self, topic: Topic, active: bool) -> BoxFuture<'_, ()> {
        let _ = self.commands.send(Command::SetActive { topic, active });
        Box::pin(async {})
    }

    fn import(
        &self,
        topic: Topic,
        header: Vec<u8>,
        body: Option<Vec<u8>>,
    ) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(self.ask(move |reply| Command::Import {
            topic,
            header,
            body,
            reply,
        }))
    }
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn unb64(s: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::STANDARD.decode(s).ok()
}

/// A drained item as the bus delivers it.
fn item(value: &Value) -> Option<P2pandaItem> {
    let message = unb64(value["message"].as_str()?)?;
    let author: VerifyingKey = value["author"].as_str()?.parse().ok()?;
    match value["kind"].as_str()? {
        "operation" => Some(P2pandaItem::Operation {
            hash: value["hash"].as_str()?.parse::<Hash>().ok()?,
            author,
            message,
            // p2panda_core API 2: the signed operation, to pass it on to Logos.
            signed: value["header"]
                .as_str()
                .and_then(unb64)
                .map(|header| (header, value["body"].as_str().and_then(unb64))),
        }),
        "ephemeral" => Some(P2pandaItem::Ephemeral {
            author,
            timestamp: value["timestamp"].as_u64()?,
            message,
        }),
        _ => None,
    }
}

struct Open {
    sub: String,
    tag: String,
    tx: tmpsc::UnboundedSender<P2pandaItem>,
    active: bool,
    label: String,
}

/// The p2panda thread: runs until the bus is dropped.
pub fn run<A: PandaApi>(
    mut api: A,
    commands: mpsc::Receiver<Command>,
    status: Arc<std::sync::Mutex<PandaStatus>>,
) {
    let mut network: Option<(String, String)> = None;
    let mut topics: HashMap<Topic, Open> = HashMap::new();
    let mut labels: HashMap<Topic, String> = HashMap::new();
    let mut last_poll = Instant::now();
    let mut last_lease = Instant::now();

    let open_topic = |api: &mut A,
                      network: &str,
                      topic: &Topic,
                      label: &str|
     -> Result<(String, String), String> {
        let reply = api.open(network, &hex::encode(topic.to_bytes()), label)?;
        let sub = reply["sub"]
            .as_str()
            .ok_or("no subscription in reply")?
            .to_owned();
        let tag = reply["tag"].as_str().unwrap_or_default().to_owned();
        Ok((sub, tag))
    };

    loop {
        let drain = |api: &mut A, open: &Open| {
            for _ in 0..64 {
                let reply = match api.drain(&open.sub) {
                    Ok(reply) => reply,
                    Err(error) => {
                        warn!("drain failed: {error}");
                        return;
                    }
                };
                for value in reply["items"].as_array().into_iter().flatten() {
                    if value["kind"] == "sync" {
                        if let Some(remote) = value["remote"].as_str() {
                            let mut status = status.lock().unwrap();
                            if value["state"] == "failed" {
                                status.peers.remove(remote);
                            } else {
                                status.peers.insert(remote.to_owned(), Instant::now());
                            }
                        }
                        continue;
                    }
                    if let Some(item) = item(value) {
                        let _ = open.tx.send(item);
                    }
                }
                if reply["more"] != true {
                    return;
                }
            }
        };

        match commands.recv_timeout(Duration::from_secs(1)) {
            Ok(Command::NetworkOpen {
                network: id,
                key,
                reply,
            }) => {
                let result = api.network_open(&id, &key).map(|_| ());
                {
                    let mut status = status.lock().unwrap();
                    status.open = result.is_ok();
                    status.error = result.as_ref().err().cloned();
                }
                if result.is_ok() {
                    network = Some((id, key));
                }
                let _ = reply.send(result);
            }
            Ok(Command::Open { topic, reply }) => {
                let result = match &network {
                    None => Err("p2panda network not open".to_owned()),
                    Some((id, _)) => {
                        let label = labels.get(&topic).cloned().unwrap_or_default();
                        open_topic(&mut api, id, &topic, &label).map(|(sub, tag)| {
                            let (tx, rx) = tmpsc::unbounded_channel();
                            let open = Open {
                                sub,
                                tag,
                                tx,
                                active: false,
                                label,
                            };
                            drain(&mut api, &open);
                            topics.insert(topic, open);
                            rx
                        })
                    }
                };
                let _ = reply.send(result);
            }
            Ok(Command::Publish {
                topic,
                message,
                prune,
                reply,
            }) => {
                let result = match topics.get(&topic) {
                    None => Err("note not open in p2panda_core".to_owned()),
                    Some(open) => api.publish(&open.sub, &b64(&message), prune).map(|_| ()),
                };
                let _ = reply.send(result);
            }
            Ok(Command::Ephemeral {
                topic,
                message,
                reply,
            }) => {
                let result = match topics.get(&topic) {
                    None => Err("note not open in p2panda_core".to_owned()),
                    Some(open) => api.ephemeral(&open.sub, &b64(&message)).map(|_| ()),
                };
                let _ = reply.send(result);
            }
            Ok(Command::Import {
                topic,
                header,
                body,
                reply,
            }) => {
                let result = match topics.get(&topic) {
                    None => Err("note not open in p2panda_core".to_owned()),
                    Some(open) => api
                        .import(
                            &open.sub,
                            &b64(&header),
                            &body.as_deref().map(b64).unwrap_or_default(),
                        )
                        .map(|_| ()),
                };
                let _ = reply.send(result);
            }
            Ok(Command::SetActive { topic, active }) => {
                if let Some(open) = topics.get_mut(&topic) {
                    open.active = active;
                    let _ = api.set_active(&open.sub, active);
                }
            }
            Ok(Command::Bridge { topic, on, reply }) => {
                let result = match topics.get(&topic) {
                    None => Err("note not open in p2panda_core".to_owned()),
                    Some(open) => api.bridge(&open.sub, on).and_then(|reply| {
                        reply["state"]
                            .as_str()
                            .map(str::to_owned)
                            .ok_or_else(|| "no state in reply".to_owned())
                    }),
                };
                let _ = reply.send(result);
            }
            Ok(Command::Bridges { reply }) => {
                let result = api.bridges().map(|reply| {
                    reply["bridges"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter(|b| {
                            network
                                .as_ref()
                                .is_some_and(|(id, _)| b["network"].as_str() == Some(id.as_str()))
                        })
                        .filter_map(|b| {
                            Some((
                                b["topic"].as_str()?.to_owned(),
                                b["state"].as_str()?.to_owned(),
                            ))
                        })
                        .collect()
                });
                let _ = reply.send(result);
            }
            Ok(Command::Pending(tag)) => {
                if let Some(open) = topics.values().find(|o| o.tag == tag) {
                    drain(&mut api, open);
                }
            }
            Ok(Command::Label { topic, label }) => {
                labels.insert(topic, label);
            }
            Ok(Command::Restarted) => {
                info!("p2panda_core restarted, opening the network and notes again");
                if let Some((id, key)) = &network {
                    if let Err(error) = api.network_open(id, key) {
                        warn!("can't open the network again: {error}");
                    }
                    for (topic, open) in topics.iter_mut() {
                        match open_topic(&mut api, id, topic, &open.label) {
                            Ok((sub, tag)) => {
                                open.sub = sub;
                                open.tag = tag;
                                if open.active {
                                    let _ = api.set_active(&open.sub, true);
                                }
                            }
                            Err(error) => warn!("can't open a note again: {error}"),
                        }
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }

        // A lost `pending` event must not stall a note: drain everything now and then.
        if last_poll.elapsed() >= Duration::from_secs(10) {
            last_poll = Instant::now();
            for open in topics.values() {
                drain(&mut api, open);
            }
        }
        // `set_active` is a 60 s lease in p2panda_core.
        if last_lease.elapsed() >= Duration::from_secs(30) {
            last_lease = Instant::now();
            for open in topics.values().filter(|o| o.active) {
                let _ = api.set_active(&open.sub, true);
            }
        }
    }
}
