//! The engine of `p2panda_core` (docs/adr/0001-p2panda-core-module.md): p2panda nodes, one per
//! network id, shared by every app on the device, and an opt-in bridge of single topics to Logos
//! Delivery.
//!
//! Nothing here knows about Basecamp IPC; `module.rs` maps the module methods onto it, and the
//! tests drive it directly.
//!
//! Messages cross the API as base64 of their CBOR encoding, exactly as p2panda puts them on the
//! wire (for Reflection: a CBOR byte string holding a Loro update). The engine handles them as
//! generic CBOR values, so it carries any app's message type without knowing it.
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use ciborium::Value as Cbor;
use logos_p2panda::{Config as LogosConfig, DeliveryBus, LogosSync, Op};
use p2panda::streams::{
    EphemeralStreamPublisher, StreamEvent, StreamFrom, StreamPublisher,
};
use p2panda::{NetworkId, Topic};
use p2panda_core::SigningKey;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::runtime::Runtime;
use tokio::sync::Notify;
use tokio_stream::StreamExt;
use tracing::{error, info, warn};

pub const API_VERSION: u64 = 1;

/// Items a subscription may hold before the module stops reading ahead for it.
const MAX_QUEUED_OPS: usize = 4096;
/// Ephemeral messages are lossy: past this, the oldest are dropped.
const MAX_QUEUED_EPHEMERAL: usize = 256;
/// One drain returns at most this many items and, past the first, about this many bytes.
const DRAIN_MAX_ITEMS: usize = 256;
const DRAIN_MAX_BYTES: usize = 1024 * 1024;
/// A subscription not drained for this long is abandoned: no more `pending` for it.
const ABANDONED_AFTER: Duration = Duration::from_secs(600);
/// `set_active` is a lease.
const ACTIVE_LEASE: Duration = Duration::from_secs(60);

/// iroh relay and bootstrap node of p2panda's public network (as upstream Reflection uses).
const RELAY_URL: &str = "https://euc1-1.relay.n0.iroh.link.";
const BOOTSTRAP_NODE_ID: &str = "f992f1f5702087f89941ffb97ec3e7915996572c6814344a716c990b5537370c";

/// What the engine tells the host (module events).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// A subscription has items: drain it.
    Pending(String),
}

pub type Emit = Arc<dyn Fn(Event) + Send + Sync>;

/// How the engine runs.
#[derive(Clone)]
pub struct Options {
    /// No relay, no bootstrap node (tests: nodes of this process find each other locally).
    pub local: bool,
    /// How long the bridge collects operations before relaying them to Logos.
    pub relay_window: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            local: false,
            relay_window: logos_p2panda::bridge::RELAY_WINDOW,
        }
    }
}

pub struct Engine {
    rt: Runtime,
    inner: Arc<Inner>,
}

struct Inner {
    dir: PathBuf,
    bus: Arc<dyn DeliveryBus>,
    options: Options,
    emit: Emit,
    networks: Mutex<HashMap<String, Arc<Network>>>,
    subs: Mutex<HashMap<String, Arc<Sub>>>,
    bridges: Mutex<Bridges>,
}

/// One p2panda node, for one network id, signing with the key of the app that opened it.
struct Network {
    id: NetworkId,
    hex: String,
    app: String,
    key: SigningKey,
    node: p2panda::Node,
    pool: sqlx::SqlitePool,
    logos: tokio::sync::OnceCell<LogosSync>,
    /// Running bridges, by topic.
    bridged: Mutex<HashMap<Topic, Bridge>>,
}

/// A running bridge of one topic.
struct Bridge {
    task: tokio::task::AbortHandle,
    /// Operations to relay to Logos. Our own new operations go here directly: p2panda shows an
    /// operation published through one stream only to that stream, not to the bridge's.
    to_logos: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<Op>>>>,
}

/// A subscription: one app reading one topic.
struct Sub {
    handle: String,
    tag: String,
    app: String,
    label: Mutex<String>,
    network: Arc<Network>,
    topic: Topic,
    publisher: StreamPublisher<Cbor>,
    ephemeral: EphemeralStreamPublisher<Cbor>,
    queue: Mutex<Queue>,
    space: Notify,
    active_until: Mutex<Option<Instant>>,
    tasks: Mutex<Vec<tokio::task::AbortHandle>>,
}

#[derive(Default)]
struct Queue {
    ops: VecDeque<Value>,
    ephemeral: VecDeque<Value>,
    last_drain: Option<Instant>,
    created: Option<Instant>,
}

impl Queue {
    fn is_empty(&self) -> bool {
        self.ops.is_empty() && self.ephemeral.is_empty()
    }

    fn abandoned(&self) -> bool {
        self.last_drain.or(self.created).is_some_and(|at| at.elapsed() > ABANDONED_AFTER)
    }
}

/// Bridged topics, kept across restarts.
#[derive(Default, Serialize, Deserialize)]
struct Bridges {
    /// `"<network hex>/<topic hex>"` -> entry.
    topics: BTreeMap<String, BridgeEntry>,
}

#[derive(Clone, Serialize, Deserialize)]
struct BridgeEntry {
    network: String,
    topic: String,
    app: String,
    label: String,
    /// `pending` (an app asked; the user hasn't confirmed) or `on`.
    state: String,
}

fn bridge_key(network: &str, topic: &str) -> String {
    format!("{network}/{topic}")
}

fn parse32(what: &str, hex_str: &str) -> Result<[u8; 32], String> {
    let bytes = hex::decode(hex_str.trim().trim_matches('"'))
        .map_err(|_| format!("{what} is not hex"))?;
    bytes
        .try_into()
        .map_err(|_| format!("{what} must be 32 bytes (64 hex characters)"))
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn unb64(s: &str) -> Result<Vec<u8>, String> {
    base64::engine::general_purpose::STANDARD
        .decode(s.trim().trim_matches('"'))
        .map_err(|_| "not base64".to_owned())
}

fn cbor_encode(value: &Cbor) -> Vec<u8> {
    let mut bytes = Vec::new();
    // Writing into a Vec does not fail.
    let _ = ciborium::into_writer(value, &mut bytes);
    bytes
}

fn cbor_decode(bytes: &[u8]) -> Result<Cbor, String> {
    ciborium::from_reader(bytes).map_err(|error| format!("message is not CBOR: {error}"))
}

fn random_handle() -> String {
    hex::encode(rand::random::<[u8; 16]>())
}

impl Engine {
    pub fn start(dir: PathBuf, bus: Arc<dyn DeliveryBus>, options: Options, emit: Emit) -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("p2panda-core")
            .enable_all()
            .build()
            .expect("tokio runtime");
        let _ = std::fs::create_dir_all(&dir);
        let bridges = std::fs::read(dir.join("bridges.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        let inner = Arc::new(Inner {
            dir,
            bus,
            options,
            emit,
            networks: Mutex::new(HashMap::new()),
            subs: Mutex::new(HashMap::new()),
            bridges: Mutex::new(bridges),
        });
        let ticker = inner.clone();
        rt.spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                ticker.tick().await;
            }
        });
        Self { rt, inner }
    }

    pub fn api_version(&self) -> Value {
        json!({ "ok": true, "apiVersion": API_VERSION })
    }

    /// Start (or join) the node of a network, signing with `key`. One key per network: a second
    /// app on the same network must use the same key (it is the node's identity).
    pub fn network_open(&self, app: &str, network: &str, key: &str) -> Result<Value, String> {
        let id = parse32("network", network)?;
        let key = SigningKey::from_bytes(&parse32("key", key)?);
        let hex = hex::encode(id);
        if let Some(existing) = self.inner.networks.lock().unwrap().get(&hex) {
            if existing.key.verifying_key() != key.verifying_key() {
                return Err("this network is already open with another key".into());
            }
            return Ok(json!({ "ok": true, "author": key.verifying_key().to_hex() }));
        }
        let inner = self.inner.clone();
        let app = app.to_owned();
        let network = self
            .rt
            .block_on(async move { inner.start_network(&app, id, key).await })?;
        // Bridges the user confirmed earlier come back with the network.
        let confirmed: Vec<String> = self
            .inner
            .bridges
            .lock()
            .unwrap()
            .topics
            .values()
            .filter(|e| e.network == network.hex && e.state == "on")
            .map(|e| e.topic.clone())
            .collect();
        for topic in confirmed {
            if let Ok(bytes) = parse32("topic", &topic) {
                self.inner.start_bridge(&network, Topic::from(bytes), &self.rt);
            }
        }
        Ok(json!({ "ok": true, "author": network.key.verifying_key().to_hex() }))
    }

    /// Subscribe an app to a topic of an open network. `from`: `"start"` (every stored operation
    /// first) or `"frontier"` (what this node hasn't acknowledged yet).
    pub fn open(
        &self,
        app: &str,
        network: &str,
        topic: &str,
        from: &str,
        label: &str,
    ) -> Result<Value, String> {
        let network = self.network(network)?;
        let topic = Topic::from(parse32("topic", topic)?);
        let from = match from.trim().trim_matches('"') {
            "" | "start" => StreamFrom::Start,
            "frontier" => StreamFrom::Frontier,
            other => return Err(format!("unknown 'from': {other}")),
        };
        let inner = self.inner.clone();
        let sub = self.rt.block_on(async move {
            inner
                .open(network, topic, from, app.to_owned(), label.to_owned())
                .await
        })?;
        Ok(json!({ "ok": true, "sub": sub.handle, "tag": sub.tag }))
    }

    pub fn close(&self, sub: &str) -> Result<Value, String> {
        let sub = self
            .inner
            .subs
            .lock()
            .unwrap()
            .remove(sub.trim().trim_matches('"'))
            .ok_or("no such subscription")?;
        for task in sub.tasks.lock().unwrap().drain(..) {
            task.abort();
        }
        let inner = self.inner.clone();
        self.rt.spawn(async move { inner.update_activity(&sub.network, sub.topic).await });
        Ok(json!({ "ok": true }))
    }

    /// The next items of a subscription (operations first, then ephemeral messages).
    pub fn drain(&self, sub: &str, max: usize) -> Result<Value, String> {
        let sub = self.sub(sub)?;
        let max = max.clamp(1, DRAIN_MAX_ITEMS);
        let mut queue = sub.queue.lock().unwrap();
        queue.last_drain = Some(Instant::now());
        let mut items = Vec::new();
        let mut bytes = 0;
        while items.len() < max {
            let next = if !queue.ops.is_empty() {
                queue.ops.front()
            } else {
                queue.ephemeral.front()
            };
            let Some(next) = next else { break };
            let size = next.to_string().len();
            if !items.is_empty() && bytes + size > DRAIN_MAX_BYTES {
                break;
            }
            bytes += size;
            let item = if !queue.ops.is_empty() {
                queue.ops.pop_front()
            } else {
                queue.ephemeral.pop_front()
            };
            items.extend(item);
        }
        let more = !queue.is_empty();
        let pending = queue.ops.len() + queue.ephemeral.len();
        drop(queue);
        sub.space.notify_waiters();
        Ok(json!({ "ok": true, "items": items, "more": more, "pending": pending }))
    }

    /// Append a message to the topic's log of the network's author (`prune`: a snapshot).
    pub fn publish(&self, sub: &str, message_b64: &str, prune: bool) -> Result<Value, String> {
        let sub = self.sub(sub)?;
        let message = cbor_decode(&unb64(message_b64)?)?;
        let hash = self.rt.block_on(async move {
            let published = if prune {
                sub.publisher.prune(Some(message)).await
            } else {
                sub.publisher.publish(message).await
            }
            .map_err(|error| error.to_string())?;
            let hash = published.hash();
            let network = sub.network.clone();
            let topic = sub.topic;
            // Relay it once processed, without making the caller wait for that.
            tokio::spawn(async move {
                if let Ok(event) = published.await {
                    network.relay(
                        topic,
                        Op::from_parts(event.header().clone(), event.body().map(|b| b.to_bytes())),
                    );
                }
            });
            Ok::<_, String>(hash)
        })?;
        Ok(json!({ "ok": true, "hash": hash.to_hex() }))
    }

    /// Send an ephemeral message (presence, cursors).
    pub fn ephemeral(&self, sub: &str, message_b64: &str) -> Result<Value, String> {
        let sub = self.sub(sub)?;
        let message = cbor_decode(&unb64(message_b64)?)?;
        self.rt.block_on(async move {
            sub.ephemeral
                .publish(message)
                .await
                .map_err(|error| error.to_string())
        })?;
        Ok(json!({ "ok": true }))
    }

    /// Whether the user has the topic open (a lease of `ACTIVE_LEASE`; renew it).
    pub fn set_active(&self, sub: &str, active: bool) -> Result<Value, String> {
        let sub = self.sub(sub)?;
        *sub.active_until.lock().unwrap() = active.then(|| Instant::now() + ACTIVE_LEASE);
        let inner = self.inner.clone();
        self.rt.spawn(async move { inner.update_activity(&sub.network, sub.topic).await });
        Ok(json!({ "ok": true }))
    }

    /// An app asks to bridge its topic to Logos (`on`: becomes `pending` until the user confirms
    /// it with `bridge_confirm`), or stops bridging it (`off`: at once).
    pub fn bridge(&self, sub: &str, on: bool) -> Result<Value, String> {
        let sub = self.sub(sub)?;
        let key = bridge_key(&sub.network.hex, &hex::encode(sub.topic.to_bytes()));
        let state = if on {
            let mut bridges = self.inner.bridges.lock().unwrap();
            let entry = bridges
                .topics
                .entry(key)
                .or_insert_with(|| BridgeEntry {
                    network: sub.network.hex.clone(),
                    topic: hex::encode(sub.topic.to_bytes()),
                    app: sub.app.clone(),
                    label: sub.label.lock().unwrap().clone(),
                    state: "pending".into(),
                });
            entry.state.clone()
        } else {
            self.inner.bridges.lock().unwrap().topics.remove(&key);
            sub.network.stop_bridge(sub.topic);
            "off".to_owned()
        };
        self.inner.save_bridges();
        Ok(json!({ "ok": true, "state": state }))
    }

    /// The user confirms (or refuses, or stops) bridging a topic. For the p2panda view only.
    pub fn bridge_confirm(&self, network: &str, topic: &str, on: bool) -> Result<Value, String> {
        let network_hex = hex::encode(parse32("network", network)?);
        let topic_hex = hex::encode(parse32("topic", topic)?);
        let key = bridge_key(&network_hex, &topic_hex);
        if on {
            {
                let mut bridges = self.inner.bridges.lock().unwrap();
                let entry = bridges.topics.get_mut(&key).ok_or("no such bridge request")?;
                entry.state = "on".into();
            }
            self.inner.save_bridges();
            if let Some(network) = self.inner.networks.lock().unwrap().get(&network_hex).cloned() {
                let topic = Topic::from(parse32("topic", &topic_hex)?);
                self.inner.start_bridge(&network, topic, &self.rt);
            }
        } else {
            self.inner.bridges.lock().unwrap().topics.remove(&key);
            self.inner.save_bridges();
            if let Some(network) = self.inner.networks.lock().unwrap().get(&network_hex) {
                network.stop_bridge(Topic::from(parse32("topic", &topic_hex)?));
            }
        }
        Ok(json!({ "ok": true }))
    }

    /// Bridged topics and requests, for the p2panda view.
    pub fn bridges(&self) -> Value {
        let bridges = self.inner.bridges.lock().unwrap();
        let list: Vec<&BridgeEntry> = bridges.topics.values().collect();
        json!({ "ok": true, "bridges": list })
    }

    /// Pretend RLN budget for the bridge's Logos side (per network; `None`: no limit).
    pub fn set_message_budget(&self, budget: Option<u64>) {
        let networks: Vec<Arc<Network>> =
            self.inner.networks.lock().unwrap().values().cloned().collect();
        for network in networks {
            if let Some(logos) = network.logos.get() {
                logos.set_message_budget(budget);
            }
        }
    }

    pub fn status(&self) -> Value {
        let networks: Vec<Value> = self
            .inner
            .networks
            .lock()
            .unwrap()
            .values()
            .map(|n| {
                json!({
                    "network": n.hex,
                    "app": n.app,
                    "author": n.key.verifying_key().to_hex(),
                    "bridged": n.bridged.lock().unwrap().len(),
                    "logos": n.logos.get().map(|l| serde_json::to_value(l.stats()).unwrap_or(Value::Null)),
                })
            })
            .collect();
        let subs: Vec<Value> = self
            .inner
            .subs
            .lock()
            .unwrap()
            .values()
            .map(|s| {
                let q = s.queue.lock().unwrap();
                json!({
                    "app": s.app,
                    "label": *s.label.lock().unwrap(),
                    "network": s.network.hex,
                    "topic": hex::encode(s.topic.to_bytes()),
                    "queued": q.ops.len() + q.ephemeral.len(),
                    "abandoned": q.abandoned(),
                    "active": s.active_until.lock().unwrap().is_some_and(|t| t > Instant::now()),
                })
            })
            .collect();
        json!({ "ok": true, "apiVersion": API_VERSION, "networks": networks, "subscriptions": subs })
    }

    fn network(&self, network: &str) -> Result<Arc<Network>, String> {
        let hex = hex::encode(parse32("network", network)?);
        self.inner
            .networks
            .lock()
            .unwrap()
            .get(&hex)
            .cloned()
            .ok_or_else(|| "network not open (call network_open first)".to_owned())
    }

    fn sub(&self, sub: &str) -> Result<Arc<Sub>, String> {
        self.inner
            .subs
            .lock()
            .unwrap()
            .get(sub.trim().trim_matches('"'))
            .cloned()
            .ok_or_else(|| "no such subscription".to_owned())
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        let rt = std::mem::replace(
            &mut self.rt,
            tokio::runtime::Builder::new_current_thread().build().unwrap(),
        );
        rt.shutdown_background();
    }
}

impl Network {
    fn stop_bridge(&self, topic: Topic) {
        if let Some(bridge) = self.bridged.lock().unwrap().remove(&topic) {
            bridge.task.abort();
            info!(%topic, "bridge stopped");
        }
    }

    /// Hand an operation to the topic's bridge, if it is bridged.
    fn relay(&self, topic: Topic, op: Op) {
        if let Some(bridge) = self.bridged.lock().unwrap().get(&topic) {
            if let Some(tx) = bridge.to_logos.lock().unwrap().as_ref() {
                let _ = tx.send(op);
            }
        }
    }
}

/// An operation as a drained item.
fn operation_item<E>(operation: &p2panda::streams::ProcessedOperation<Cbor>, source: &E) -> Value
where
    E: std::fmt::Debug,
{
    let processed = operation.processed();
    let header = processed.header();
    let source = format!("{source:?}");
    let source = if source.starts_with("SyncSession") {
        "p2panda"
    } else if source.starts_with("ExternalStream") {
        "logos"
    } else {
        "local"
    };
    json!({
        "kind": "operation",
        "hash": operation.id().to_hex(),
        "author": operation.author().to_hex(),
        "seq": u64::from(header.seq_num),
        "prune": header.extensions.prune_flag().is_set(),
        "timestamp": operation.timestamp(),
        "message": b64(&cbor_encode(operation.message())),
        "source": source,
    })
}

impl Inner {
    async fn start_network(
        &self,
        app: &str,
        id: [u8; 32],
        key: SigningKey,
    ) -> Result<Arc<Network>, String> {
        let hex = hex::encode(id);
        let dir = self.dir.join(&hex);
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let pool = open_pool(&dir.join("p2panda.sqlite")).await?;
        let network_id: NetworkId = id.into();
        let mut builder = p2panda::Node::builder()
            .network_id(network_id)
            .signing_key(key.clone())
            .database_pool(pool.clone());
        if !self.options.local {
            let relay: p2panda::RelayUrl = RELAY_URL.parse().map_err(|_| "bad relay url")?;
            let bootstrap: p2panda::VerifyingKey =
                BOOTSTRAP_NODE_ID.parse().map_err(|_| "bad bootstrap node id")?;
            builder = builder.bootstrap(bootstrap, relay.clone()).relay_url(relay);
        }
        let node = builder.spawn().await.map_err(|e| e.to_string())?;
        info!(network = %hex, app, "p2panda node started");
        let network = Arc::new(Network {
            id: network_id,
            hex: hex.clone(),
            app: app.to_owned(),
            key,
            node,
            pool,
            logos: tokio::sync::OnceCell::new(),
            bridged: Mutex::new(HashMap::new()),
        });
        self.networks.lock().unwrap().insert(hex, network.clone());
        Ok(network)
    }

    async fn open(
        self: &Arc<Self>,
        network: Arc<Network>,
        topic: Topic,
        from: StreamFrom,
        app: String,
        label: String,
    ) -> Result<Arc<Sub>, String> {
        let (publisher, mut events) = network
            .node
            .stream_from::<Cbor>(topic, from)
            .await
            .map_err(|e| e.to_string())?;
        let (ephemeral, mut ephemeral_events) = network
            .node
            .ephemeral_stream::<Cbor>(topic)
            .await
            .map_err(|e| e.to_string())?;
        let sub = Arc::new(Sub {
            handle: random_handle(),
            tag: hex::encode(rand::random::<[u8; 4]>()),
            app,
            label: Mutex::new(label),
            network: network.clone(),
            topic,
            publisher,
            ephemeral,
            queue: Mutex::new(Queue {
                created: Some(Instant::now()),
                ..Queue::default()
            }),
            space: Notify::new(),
            active_until: Mutex::new(None),
            tasks: Mutex::new(Vec::new()),
        });

        // Operations: wait while the subscription is full (nothing is lost: the stream waits).
        let ops_sub = sub.clone();
        let ops_inner = self.clone();
        let ops_task = tokio::spawn(async move {
            while let Some(event) = events.next().await {
                let item = match event {
                    StreamEvent::Processed {
                        operation, source, ..
                    } => {
                        let item = operation_item(&operation, &source);
                        let _ = operation.ack().await;
                        item
                    }
                    StreamEvent::SyncStarted { remote_node_id, .. } => {
                        json!({ "kind": "sync", "remote": remote_node_id.to_string(), "state": "started" })
                    }
                    StreamEvent::SyncEnded {
                        remote_node_id,
                        error,
                        ..
                    } => json!({
                        "kind": "sync",
                        "remote": remote_node_id.to_string(),
                        "state": if error.is_some() { "failed" } else { "ended" },
                        "error": error.map(|e| e.to_string()),
                    }),
                    other => {
                        warn!("stream event: {other:?}");
                        continue;
                    }
                };
                loop {
                    let was_empty = {
                        let mut queue = ops_sub.queue.lock().unwrap();
                        if queue.ops.len() < MAX_QUEUED_OPS {
                            let was_empty = queue.is_empty();
                            queue.ops.push_back(item.clone());
                            Some(was_empty)
                        } else {
                            None
                        }
                    };
                    match was_empty {
                        Some(was_empty) => {
                            if was_empty {
                                (ops_inner.emit)(Event::Pending(ops_sub.tag.clone()));
                            }
                            break;
                        }
                        None => ops_sub.space.notified().await,
                    }
                }
            }
        });

        // Ephemeral messages: lossy, the oldest go first.
        let eph_sub = sub.clone();
        let eph_inner = self.clone();
        let eph_task = tokio::spawn(async move {
            while let Some(message) = ephemeral_events.next().await {
                let item = json!({
                    "kind": "ephemeral",
                    "author": message.author().to_hex(),
                    "timestamp": message.timestamp(),
                    "message": b64(&cbor_encode(message.body())),
                });
                let was_empty = {
                    let mut queue = eph_sub.queue.lock().unwrap();
                    let was_empty = queue.is_empty();
                    queue.ephemeral.push_back(item);
                    while queue.ephemeral.len() > MAX_QUEUED_EPHEMERAL {
                        queue.ephemeral.pop_front();
                    }
                    was_empty
                };
                if was_empty {
                    (eph_inner.emit)(Event::Pending(eph_sub.tag.clone()));
                }
            }
        });
        sub.tasks
            .lock()
            .unwrap()
            .extend([ops_task.abort_handle(), eph_task.abort_handle()]);
        self.subs.lock().unwrap().insert(sub.handle.clone(), sub.clone());
        Ok(sub)
    }

    /// Once a second: remind subscriptions with items (not abandoned ones), expire leases.
    async fn tick(&self) {
        let subs: Vec<Arc<Sub>> = self.subs.lock().unwrap().values().cloned().collect();
        let mut expired = Vec::new();
        for sub in subs {
            let remind = {
                let queue = sub.queue.lock().unwrap();
                !queue.is_empty()
                    && !queue.abandoned()
                    && queue.last_drain.is_none_or(|at| at.elapsed() > Duration::from_secs(1))
            };
            if remind {
                (self.emit)(Event::Pending(sub.tag.clone()));
            }
            let mut active = sub.active_until.lock().unwrap();
            let abandoned = sub.queue.lock().unwrap().abandoned();
            if active.is_some_and(|until| until <= Instant::now() || abandoned) {
                *active = None;
                expired.push(sub.clone());
            }
        }
        for sub in expired {
            self.update_activity(&sub.network, sub.topic).await;
        }
    }

    /// A topic is active while any subscription holds a lease on it; the bridge's Logos side
    /// sends heads only for active topics.
    async fn update_activity(&self, network: &Arc<Network>, topic: Topic) {
        let active = self.subs.lock().unwrap().values().any(|s| {
            Arc::ptr_eq(&s.network, network)
                && s.topic == topic
                && s.active_until.lock().unwrap().is_some_and(|t| t > Instant::now())
        });
        if let Some(logos) = network.logos.get() {
            if network.bridged.lock().unwrap().contains_key(&topic) {
                let _ = logos.set_active(topic, active).await;
            }
        }
    }

    /// Hand an item to every subscription of a topic (not waiting for full ones).
    fn deliver(&self, network: &Arc<Network>, topic: Topic, item: Value) {
        let subs: Vec<Arc<Sub>> = self
            .subs
            .lock()
            .unwrap()
            .values()
            .filter(|s| Arc::ptr_eq(&s.network, network) && s.topic == topic)
            .cloned()
            .collect();
        for sub in subs {
            let was_empty = {
                let mut queue = sub.queue.lock().unwrap();
                let was_empty = queue.is_empty();
                queue.ops.push_back(item.clone());
                was_empty
            };
            if was_empty {
                (self.emit)(Event::Pending(sub.tag.clone()));
            }
        }
    }

    fn save_bridges(&self) {
        let bytes = serde_json::to_vec_pretty(&*self.bridges.lock().unwrap()).unwrap_or_default();
        if let Err(error) = std::fs::write(self.dir.join("bridges.json"), bytes) {
            warn!("can't save bridges: {error}");
        }
    }

    fn start_bridge(self: &Arc<Self>, network: &Arc<Network>, topic: Topic, rt: &Runtime) {
        if network.bridged.lock().unwrap().contains_key(&topic) {
            return;
        }
        let inner = self.clone();
        let network2 = network.clone();
        let to_logos = Arc::new(Mutex::new(None));
        let to_logos2 = to_logos.clone();
        let task = rt.spawn(async move { inner.bridge_task(network2, topic, to_logos2).await });
        network.bridged.lock().unwrap().insert(
            topic,
            Bridge {
                task: task.abort_handle(),
                to_logos,
            },
        );
    }

    /// Relay one topic both ways between the network's node and Logos Delivery.
    async fn bridge_task(
        self: Arc<Self>,
        network: Arc<Network>,
        topic: Topic,
        to_logos_slot: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<Op>>>>,
    ) {
        let logos = match network
            .logos
            .get_or_try_init(|| async {
                LogosSync::new(
                    self.bus.clone(),
                    network.key.clone(),
                    network.id,
                    network.pool.clone(),
                    LogosConfig::default(),
                )
                .await
            })
            .await
        {
            Ok(logos) => logos.clone(),
            Err(error) => {
                error!("can't start the Logos side of the bridge: {error}");
                return;
            }
        };
        let (publisher, mut events) = match network
            .node
            .stream_from::<Cbor>(topic, StreamFrom::Start)
            .await
        {
            Ok(stream) => stream,
            Err(error) => {
                error!(%topic, "can't open the topic for bridging: {error}");
                return;
            }
        };
        match logos_p2panda::bridge::from_logos(&logos, topic).await {
            Ok(from_logos) => {
                if let Err(error) = publisher.import(from_logos).await {
                    error!(%topic, "can't import from Logos: {error}");
                    return;
                }
            }
            Err(error) => {
                error!(%topic, "can't subscribe on Logos: {error}");
                return;
            }
        }
        let to_logos =
            logos_p2panda::bridge::to_logos(logos.clone(), topic, self.options.relay_window);
        *to_logos_slot.lock().unwrap() = Some(to_logos.clone());
        let _ = self.update_activity(&network, topic).await;
        info!(%topic, network = %network.hex, "bridge running");
        while let Some(event) = events.next().await {
            if let StreamEvent::Processed {
                operation, source, ..
            } = event
            {
                let processed = operation.processed();
                let op = Op::from_parts(
                    processed.header().clone(),
                    processed.body().map(|body| body.to_bytes()),
                );
                let imported = matches!(source, p2panda::streams::Source::ExternalStream { .. });
                if imported {
                    // From Logos: the app subscriptions' own streams don't see imports.
                    self.deliver(&network, topic, operation_item(&operation, &source));
                } else if to_logos.send(op).is_err() {
                    break;
                }
                let _ = operation.ack().await;
            }
        }
    }
}

/// The network's database: p2panda's store (its migrations run here, as `reflection-node` does)
/// and the Logos side's operations (`logos_operations`, created by `LogosSync`).
async fn open_pool(path: &Path) -> Result<sqlx::SqlitePool, String> {
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(path)
        .shared_cache(true)
        .create_if_missing(true);
    let pool = sqlx::SqlitePool::connect_with(options)
        .await
        .map_err(|e| e.to_string())?;
    p2panda_store::sqlite::migrations()
        .run(&pool)
        .await
        .map_err(|e| e.to_string())?;
    Ok(pool)
}
