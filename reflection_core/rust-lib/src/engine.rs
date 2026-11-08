//! The Reflection engine of the Basecamp app: documents, their Loro text, and sync through
//! `reflection-node` over a Logos `DeliveryBus`.
//!
//! Wire-compatible with Reflection's GTK document layer (`reflection-doc`): one Loro text
//! container `document`, local changes published as Loro updates ("deltas"), and a Loro snapshot
//! published as a prune a few seconds after the last change.
//!
//! Message budget (RLN): Logos Delivery charges per message published, so the engine publishes
//! as little as it can (reflection ADR 0008). Every note is subscribed in the background, which
//! receives and costs nothing; only the note open in the view is active (presence, catch-up).
//! Local edits are merged and published at most once per `publish_wait`, and one snapshot
//! follows after the user stops typing. Nobody knows the real RLN limits yet, so the engine
//! assumes none: the user can set a pretend budget per epoch (`Settings`) to try out tiers,
//! and pacing follows it.
//!
//! Threading: every public method is called on the host's module thread and returns quickly.
//! Anything that touches the network or the database runs on the engine's own tokio runtime.
//! Nothing here waits for the transport, which itself waits for the module thread (Logos IPC).
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use loro::{ExportMode, Frontiers, LoroDoc, UpdateOptions, VersionVector};
use p2panda::{Hash, SigningKey, Topic, VerifyingKey};
use reflection_node::logos::DeliveryBus;

use crate::panda::PandaBus;
use reflection_node::{Node, TopicStream, TopicSubscription, TopicSubscriptionError};
use serde_json::{json, Value};
use tokio::runtime::Runtime;
use tracing::{error, info, warn};

/// The text container of a Reflection document (same id as `reflection-doc`).
static TEXT_ID: LazyLock<loro::ContainerID> =
    LazyLock::new(|| loro::ContainerID::new_root("document", loro::ContainerType::Text));

/// Reflection's network id (`reflection-doc`: `Hash::digest(b"reflection-v2")`).
const NETWORK_NAME: &[u8] = b"reflection-v2";

/// A snapshot is published once the user has stopped typing for this long.
const SNAPSHOT_QUIET: Duration = Duration::from_secs(10);

/// Merged local edits are published at most this often while the budget allows.
const PUBLISH_INTERVAL: Duration = Duration::from_secs(1);

/// Our cursor is sent at most this often (or half as often as edits, when budget is short).
const CURSOR_INTERVAL: Duration = Duration::from_secs(1);

/// User settings, kept in `settings.json`.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    /// Pretend RLN budget, messages per 10-minute epoch. `None`: no limit.
    pub budget: Option<u64>,
    /// Send our cursor position to the others on the open note.
    pub share_cursor: bool,
    /// "logos" (Logos Delivery through loam_core, the default) or "p2panda" (p2panda_core: the
    /// network of upstream Reflection, with notes bridged to Logos on request). Takes effect when
    /// Reflection starts.
    pub sync_mode: Option<String>,
}

impl Settings {
    pub fn mode(&self) -> &str {
        match self.sync_mode.as_deref() {
            Some("p2panda") => "p2panda",
            _ => "logos",
        }
    }
}

/// Reflection's ephemeral message (as `reflection-doc` sends it, postcard-encoded): where
/// someone's cursor is, as Loro cursors that stay on their character as the text changes.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
enum EphemeralData {
    Cursor {
        insert_cursor: Option<loro::cursor::Cursor>,
        selection_bound: Option<loro::cursor::Cursor>,
    },
}

/// How a budget is being spent this epoch.
#[derive(Clone, Copy, Debug)]
struct Spend {
    budget: Option<u64>,
    used: u64,
    seconds_left: u64,
}

impl Spend {
    /// Kept back for presence and catch-up.
    fn reserve(budget: u64) -> u64 {
        (budget / 20).max(2)
    }

    /// How long to wait before publishing merged edits: the base interval while less than half
    /// the budget is used, then the rest spread over the rest of the epoch, and nothing once only
    /// the reserve is left (until the next epoch).
    fn publish_wait(&self) -> Duration {
        let Some(budget) = self.budget else {
            return PUBLISH_INTERVAL;
        };
        let remaining = budget.saturating_sub(self.used);
        let reserve = Self::reserve(budget);
        if self.used * 2 < budget {
            PUBLISH_INTERVAL
        } else if remaining <= reserve {
            Duration::from_secs(self.seconds_left + 1)
        } else {
            PUBLISH_INTERVAL.max(Duration::from_secs_f64(
                self.seconds_left as f64 / (remaining - reserve) as f64,
            ))
        }
    }

    /// Cursors come after edits: half as often, and not at all when the budget is short.
    fn cursor_wait(&self) -> Option<Duration> {
        let Some(budget) = self.budget else {
            return Some(CURSOR_INTERVAL);
        };
        let remaining = budget.saturating_sub(self.used);
        if remaining <= Self::reserve(budget) * 3 {
            return None;
        }
        Some(CURSOR_INTERVAL.max(self.publish_wait() * 2))
    }
}

/// The view polls `state` every second; without a poll for this long, nothing is open.
const VIEW_GONE: Duration = Duration::from_secs(20);

/// Versions the view may still edit from.
const KEPT_VERSIONS: usize = 256;

/// Longest document name, as in `reflection-doc`.
const NAME_LENGTH: usize = 124;

/// Largest text the app accepts from the view, so a snapshot fits one Logos message.
const MAX_TEXT_BYTES: usize = 100 * 1024;

pub struct Engine {
    rt: Runtime,
    inner: Arc<Inner>,
}

struct Inner {
    /// The running node; replaced when the sync mode changes.
    node: Mutex<Option<Arc<Node>>>,
    /// Serializes starting and switching the node.
    switching: tokio::sync::Mutex<()>,
    /// Logos Delivery through loam_core, for the logos mode.
    bus: Arc<dyn DeliveryBus>,
    node_error: Mutex<Option<String>>,
    docs: Mutex<BTreeMap<String, Arc<Doc>>>,
    me: Mutex<Option<String>>,
    transport: Mutex<Value>,
    /// The note open in the view, and when the view last asked.
    active: Mutex<Option<String>>,
    last_poll: Mutex<Instant>,
    /// The note whose stream is active now; changed only by `apply_active`, one at a time.
    applied: tokio::sync::Mutex<Option<String>>,
    settings: Mutex<Settings>,
    settings_path: PathBuf,
    /// The node's data directory (also the local snapshot cache).
    dir: PathBuf,
    /// The sync mode the running node uses.
    running_mode: Mutex<String>,
    /// p2panda_core, when the host has it.
    panda: Option<Arc<PandaBus>>,
    /// Bridge state of notes in p2panda mode, by note id ("pending" / "on"), refreshed in the
    /// background (asking p2panda_core from the module thread would deadlock).
    bridge_states: Mutex<HashMap<String, String>>,
}

impl Engine {
    /// Start the engine. The node comes up in the background; documents created before that
    /// are kept in memory and published once it is ready.
    pub fn start(
        persist_dir: PathBuf,
        bus: Arc<dyn DeliveryBus>,
        panda: Option<Arc<PandaBus>>,
    ) -> Self {
        let settings_path = persist_dir.join("settings.json");
        let settings: Settings = std::fs::read(&settings_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("reflection")
            .enable_all()
            .build()
            .expect("tokio runtime");
        let inner = Arc::new(Inner {
            node: Mutex::new(None),
            switching: tokio::sync::Mutex::new(()),
            bus: bus.clone(),
            node_error: Mutex::new(None),
            docs: Mutex::new(BTreeMap::new()),
            me: Mutex::new(None),
            transport: Mutex::new(json!({ "status": "starting" })),
            active: Mutex::new(None),
            last_poll: Mutex::new(Instant::now()),
            applied: tokio::sync::Mutex::new(None),
            settings: Mutex::new(settings),
            settings_path,
            dir: persist_dir.clone(),
            running_mode: Mutex::new(String::new()),
            panda,
            bridge_states: Mutex::new(HashMap::new()),
        });
        let task_inner = inner.clone();
        rt.spawn(async move {
            if let Err(error) = task_inner.clone().start_node().await {
                error!("reflection node failed to start: {error}");
                *task_inner.node_error.lock().unwrap() = Some(error);
            }
        });
        // Keep the local snapshot of every note that changed, and the bridge states.
        let cache_inner = inner.clone();
        rt.spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                cache_inner.save_snapshots();
                cache_inner.refresh_bridges().await;
            }
        });
        // The view closed: deactivate its note.
        let watch_inner = inner.clone();
        rt.spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                if watch_inner.last_poll.lock().unwrap().elapsed() > VIEW_GONE {
                    *watch_inner.active.lock().unwrap() = None;
                    watch_inner.apply_active().await;
                }
            }
        });
        Self { rt, inner }
    }

    /// The note open in the view (`""` for none). Only that one publishes presence and asks
    /// for catch-up.
    fn view_opened(&self, doc_id: &str) {
        *self.inner.last_poll.lock().unwrap() = Instant::now();
        let key = doc_id.trim().trim_matches('"').to_lowercase();
        let wanted = (!key.is_empty() && self.doc(&key).is_some()).then_some(key);
        {
            let mut active = self.inner.active.lock().unwrap();
            if *active == wanted {
                return;
            }
            *active = wanted;
        }
        let inner = self.inner.clone();
        self.rt.spawn(async move { inner.apply_active().await });
    }

    pub fn settings(&self) -> Value {
        json!({ "ok": true, "settings": *self.inner.settings.lock().unwrap() })
    }

    /// Change settings: a JSON object with any of `budget` (a number, or null for no limit)
    /// and `shareCursor`.
    pub fn set_settings(&self, changes: &str) -> Result<Value, String> {
        let changes: Value =
            serde_json::from_str(changes.trim()).map_err(|e| format!("Bad settings: {e}"))?;
        let mut settings = self.inner.settings.lock().unwrap().clone();
        if let Some(budget) = changes.get("budget") {
            settings.budget = match budget {
                Value::Null => None,
                Value::Number(n) => Some(
                    n.as_u64()
                        .filter(|n| *n > 0)
                        .ok_or("The budget is a positive whole number")?,
                ),
                Value::String(s) if s.trim().is_empty() => None,
                Value::String(s) => Some(
                    s.trim()
                        .parse::<u64>()
                        .ok()
                        .filter(|n| *n > 0)
                        .ok_or("The budget is a positive whole number")?,
                ),
                _ => return Err("The budget is a positive whole number".into()),
            };
        }
        let mut stop_sharing = false;
        if let Some(mode) = changes.get("syncMode").and_then(Value::as_str) {
            if mode != "logos" && mode != "p2panda" {
                return Err("syncMode is \"logos\" or \"p2panda\"".into());
            }
            settings.sync_mode = Some(mode.to_owned());
        }
        if let Some(share) = changes.get("shareCursor").and_then(Value::as_bool) {
            stop_sharing = settings.share_cursor && !share;
            settings.share_cursor = share;
        }
        if let Err(error) = std::fs::write(
            &self.inner.settings_path,
            serde_json::to_vec_pretty(&settings).unwrap_or_default(),
        ) {
            warn!("can't save settings: {error}");
        }
        if let Some(node) = self.inner.node() {
            node.set_message_budget(settings.budget);
        }
        let switch = settings.mode() != self.inner.running_mode.lock().unwrap().as_str();
        *self.inner.settings.lock().unwrap() = settings.clone();
        if stop_sharing {
            // Take our cursor off the others' screens.
            if let Some(doc) = self.active_doc() {
                self.rt
                    .spawn(async move { doc.send_cursor_now(None).await });
            }
        }
        if switch {
            let inner = self.inner.clone();
            self.rt.spawn(async move { inner.switch_node().await });
        }
        Ok(json!({ "ok": true, "settings": settings, "switching": switch }))
    }

    /// Ask to carry a note over Logos as well (p2panda mode): it stays pending until the user
    /// confirms it in the p2panda view. Returns at once; `state` shows the outcome.
    pub fn bridge(&self, doc_id: &str, on: bool) -> Result<Value, String> {
        let doc = self.doc(doc_id).ok_or("No such note")?;
        if self.inner.running_mode.lock().unwrap().as_str() != "p2panda" {
            return Err("Bridging is for p2panda mode (Message stats > Sync)".into());
        }
        let panda = self
            .inner
            .panda
            .clone()
            .ok_or("p2panda_core is not available")?;
        let inner = self.inner.clone();
        let hex = doc.hex();
        self.rt.spawn(async move {
            match panda.bridge(Topic::from(doc.id), on).await {
                Ok(state) => {
                    let mut states = inner.bridge_states.lock().unwrap();
                    if state == "off" {
                        states.remove(&hex);
                    } else {
                        states.insert(hex, state);
                    }
                }
                Err(error) => warn!("bridge request failed: {error}"),
            }
        });
        Ok(json!({ "ok": true, "requested": on }))
    }

    fn active_doc(&self) -> Option<Arc<Doc>> {
        let active = self.inner.active.lock().unwrap().clone()?;
        self.doc(&active)
    }

    /// Where the user's cursor is in the open note: UTF-16 offsets (as QML counts them) into
    /// the text of version `base`. Sent to the others if the user shares their cursor.
    pub fn set_cursor(
        &self,
        doc_id: &str,
        base: &str,
        position: i64,
        anchor: i64,
    ) -> Result<Value, String> {
        let doc = self.doc(doc_id).ok_or("No such note")?;
        let base = base.trim().trim_matches('"').parse::<u64>().ok();
        doc.set_own_cursor(base, position, anchor)?;
        if self.inner.settings.lock().unwrap().share_cursor {
            if let Some(wait) = self.inner.spend().cursor_wait() {
                let doc = doc.clone();
                self.rt
                    .spawn(async move { doc.request_cursor_publish(wait) });
            }
        }
        Ok(json!({ "ok": true }))
    }

    /// Message counters, for the debug panel: what this device sent and received over Logos
    /// Delivery, per RLN epoch and per note.
    pub fn stats(&self) -> Value {
        let stats = self.inner.node().and_then(|node| node.transport_stats());
        let names: BTreeMap<String, String> = self
            .inner
            .docs
            .lock()
            .unwrap()
            .iter()
            .map(|(id, doc)| (id.clone(), doc.display_name()))
            .collect();
        let spend = self.inner.spend();
        let settings = self.inner.settings.lock().unwrap().clone();
        json!({
            "ok": true,
            "settings": settings,
            "budget": settings.budget,
            "publishInterval": spend.publish_wait().as_secs_f64(),
            "cursorInterval": spend.cursor_wait().map(|d| d.as_secs_f64()),
            "snapshotQuiet": SNAPSHOT_QUIET.as_secs(),
            "active": *self.inner.active.lock().unwrap(),
            "names": names,
            "transport": *self.inner.transport.lock().unwrap(),
            "stats": stats,
        })
    }

    /// Transport state shown to the user (set by the module from loam_core).
    pub fn set_transport(&self, transport: Value) {
        *self.inner.transport.lock().unwrap() = transport;
    }

    /// Everything the view shows: the document list, and the open document if `doc_id` is set.
    pub fn state(&self, doc_id: &str) -> Value {
        self.view_opened(doc_id);
        let me = self.inner.me.lock().unwrap().clone().unwrap_or_default();
        let docs: Vec<Arc<Doc>> = self.inner.docs.lock().unwrap().values().cloned().collect();
        let mut list: Vec<Value> = docs.iter().map(|doc| doc.summary(&me)).collect();
        list.sort_by(|a, b| b["updated"].as_u64().cmp(&a["updated"].as_u64()));
        let current = if doc_id.is_empty() {
            Value::Null
        } else {
            match self.doc(doc_id) {
                Some(doc) => {
                    let mut detail = doc.detail(&me);
                    detail["bridge"] = json!(self
                        .inner
                        .bridge_states
                        .lock()
                        .unwrap()
                        .get(&doc.hex())
                        .cloned()
                        .unwrap_or_else(|| "off".into()));
                    detail
                }
                None => Value::Null,
            }
        };
        let node = match (&*self.inner.node_error.lock().unwrap(), self.inner.node()) {
            (Some(error), _) => format!("error: {error}"),
            (None, Some(_)) => "ready".to_owned(),
            (None, None) => "starting".to_owned(),
        };
        json!({
            "ok": true,
            "node": node,
            "me": *self.inner.me.lock().unwrap(),
            "transport": *self.inner.transport.lock().unwrap(),
            "settings": *self.inner.settings.lock().unwrap(),
            "syncMode": *self.inner.running_mode.lock().unwrap(),
            "p2pandaAvailable": self.inner.panda.is_some(),
            "docs": list,
            "current": current,
        })
    }

    /// A new document, with `title` as its first line.
    pub fn create(&self, title: &str) -> Result<Value, String> {
        let id: [u8; 32] = rand::random();
        let doc = Doc::new(id);
        let title = title.trim();
        if !title.is_empty() {
            doc.local_edit(None, &format!("{title}\n"))?;
        }
        self.add(doc.clone());
        Ok(json!({ "ok": true, "id": doc.hex(), "current": doc.detail(&self.me()) }))
    }

    fn me(&self) -> String {
        self.inner.me.lock().unwrap().clone().unwrap_or_default()
    }

    /// Open a document someone shared, by its id (64 hex characters).
    pub fn join(&self, id_hex: &str) -> Result<Value, String> {
        let id = parse_id(id_hex)?;
        let hex = hex::encode(id);
        if let Some(doc) = self.doc(&hex) {
            return Ok(json!({ "ok": true, "id": hex, "current": doc.detail(&self.me()) }));
        }
        let doc = Doc::new(id);
        self.add(doc.clone());
        Ok(json!({ "ok": true, "id": hex, "current": doc.detail(&self.me()) }))
    }

    /// The user's text, edited from version `base` of the document. See `Doc::local_edit`.
    pub fn set_text(&self, doc_id: &str, base: &str, text: &str) -> Result<Value, String> {
        if text.len() > MAX_TEXT_BYTES {
            return Err(format!(
                "The note is too long ({} KB, at most {} KB)",
                text.len() / 1024,
                MAX_TEXT_BYTES / 1024
            ));
        }
        let doc = self.doc(doc_id).ok_or("No such note")?;
        let base = base.trim().trim_matches('"').parse::<u64>().ok();
        let edit = doc.local_edit(base, text)?;
        if edit.changed {
            let interval = self.inner.spend().publish_wait();
            let doc = doc.clone();
            self.rt.spawn(async move { doc.request_publish(interval) });
        }
        let mut detail = doc.detail(&self.me());
        detail["editVersion"] = json!(edit.edit_version.to_string());
        Ok(json!({ "ok": true, "current": detail }))
    }

    /// Stop syncing a document and forget it on this device.
    pub fn remove(&self, doc_id: &str) -> Result<Value, String> {
        let doc = self
            .inner
            .docs
            .lock()
            .unwrap()
            .remove(doc_id)
            .ok_or("No such note")?;
        {
            let mut active = self.inner.active.lock().unwrap();
            if active.as_deref() == Some(doc.hex().as_str()) {
                *active = None;
            }
        }
        let inner = self.inner.clone();
        let deactivate = inner.clone();
        self.rt
            .spawn(async move { deactivate.apply_active().await });
        self.rt.spawn(async move {
            let stream = doc.stream.lock().unwrap().take();
            if let Some(stream) = stream {
                if let Ok(stream) = Arc::try_unwrap(stream) {
                    let _ = stream.unsubscribe().await;
                }
            }
            if let Some(node) = inner.node() {
                if let Err(error) = node.delete_topic(Topic::from(doc.id)).await {
                    warn!("can't delete note: {error}");
                }
            }
        });
        Ok(json!({ "ok": true }))
    }

    fn doc(&self, doc_id: &str) -> Option<Arc<Doc>> {
        let key = doc_id.trim().trim_matches('"').to_lowercase();
        self.inner.docs.lock().unwrap().get(&key).cloned()
    }

    fn add(&self, doc: Arc<Doc>) {
        self.inner
            .docs
            .lock()
            .unwrap()
            .insert(doc.hex(), doc.clone());
        let inner = self.inner.clone();
        self.rt.spawn(async move {
            if let Some(node) = inner.node() {
                inner.subscribe(&node, doc).await;
            }
            // Otherwise start_node subscribes every document once the node is up.
        });
    }

    #[cfg(test)]
    pub(crate) fn wait_ready(&self) {
        for _ in 0..200 {
            if self.inner.node().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!(
            "node did not start: {:?}",
            self.inner.node_error.lock().unwrap()
        );
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        // Leave the runtime's threads to finish on their own; the host is unloading us.
        let rt = std::mem::replace(
            &mut self.rt,
            tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap(),
        );
        rt.shutdown_background();
    }
}

impl Inner {
    fn node(&self) -> Option<Arc<Node>> {
        self.node.lock().unwrap().clone()
    }

    /// Start the node for the chosen sync mode and subscribe every note.
    async fn start_node(self: Arc<Self>) -> Result<(), String> {
        let _switching = self.switching.lock().await;
        let persist_dir = self.dir.clone();
        let bus = self.bus.clone();
        std::fs::create_dir_all(&persist_dir).map_err(|e| e.to_string())?;
        let key = load_or_create_key(&persist_dir)?;
        *self.me.lock().unwrap() = Some(key.verifying_key().to_hex());

        let mode = self.settings.lock().unwrap().mode().to_owned();
        let node = match (mode.as_str(), &self.panda) {
            ("p2panda", Some(panda)) => {
                let network = Hash::digest(NETWORK_NAME);
                panda
                    .network_open(network.to_hex(), key.to_hex())
                    .await
                    .map_err(|e| format!("p2panda_core: {e}"))?;
                info!("syncing through p2panda_core");
                Node::with_p2panda_bus(key, network, Some(&persist_dir), panda.clone())
                    .await
                    .map_err(|e| e.to_string())?
            }
            ("p2panda", None) => return Err("p2panda mode needs the p2panda_core module".into()),
            _ => Node::with_delivery_bus(key, Hash::digest(NETWORK_NAME), Some(&persist_dir), bus)
                .await
                .map_err(|e| e.to_string())?,
        };
        *self.running_mode.lock().unwrap() = mode;

        // Documents known from earlier runs. Their content comes back from the node's
        // operation store when we subscribe.
        let known = node.topics().await.map_err(|e| e.to_string())?;
        for topic in known {
            let id = topic.topic.to_bytes();
            let hex = hex::encode(id);
            let mut docs = self.docs.lock().unwrap();
            docs.entry(hex.clone()).or_insert_with(|| {
                let doc = Doc::new(id);
                if let Some(name) = topic.name.as_ref().filter(|n| !n.is_empty()) {
                    *doc.name.lock().unwrap() = name.clone();
                }
                // What we had last time, whichever network it came from.
                if let Ok(bytes) = std::fs::read(self.dir.join("notes").join(format!("{hex}.loro")))
                {
                    doc.import_remote(&bytes);
                }
                doc
            });
        }

        node.set_message_budget(self.settings.lock().unwrap().budget);
        let node = Arc::new(node);
        *self.node.lock().unwrap() = Some(node.clone());
        *self.node_error.lock().unwrap() = None;
        info!("reflection node ready");

        let docs: Vec<Arc<Doc>> = self.docs.lock().unwrap().values().cloned().collect();
        for doc in docs {
            self.subscribe(&node, doc).await;
        }
        self.publish_to_new_network().await;
        Ok(())
    }

    /// Switch to the sync mode in the settings: close every note's subscription, stop the
    /// node, and start one for the new mode. The notes stay; their content is in memory and in
    /// the local snapshots.
    async fn switch_node(self: Arc<Self>) {
        {
            let _switching = self.switching.lock().await;
            if self.settings.lock().unwrap().mode() == self.running_mode.lock().unwrap().as_str() {
                return;
            }
            info!("switching sync network");
            self.save_snapshots();
            let old = self.node.lock().unwrap().take();
            *self.applied.lock().await = None;
            let docs: Vec<Arc<Doc>> = self.docs.lock().unwrap().values().cloned().collect();
            for doc in docs {
                let stream = doc.stream.lock().unwrap().take();
                doc.subscribing.store(false, Ordering::SeqCst);
                if let Some(stream) = stream {
                    if let Ok(stream) = Arc::try_unwrap(stream) {
                        let _ = stream.unsubscribe().await;
                    }
                }
            }
            if let Some(old) = old {
                let _ = old.shutdown().await;
            }
            self.bridge_states.lock().unwrap().clear();
        }
        if let Err(error) = self.clone().start_node().await {
            error!("can't start the node for the new sync network: {error}");
            *self.node_error.lock().unwrap() = Some(error);
        }
    }

    /// In p2panda mode, notes this device has never published on p2panda (they were on Logos)
    /// are published there once, as a snapshot, so the others get their content.
    async fn publish_to_new_network(&self) {
        if self.running_mode.lock().unwrap().as_str() != "p2panda" {
            return;
        }
        // Give the replay from p2panda_core a moment, so a snapshot includes what it has.
        tokio::time::sleep(Duration::from_secs(3)).await;
        let path = self.dir.join("published-p2panda.json");
        let mut done: BTreeSet<String> = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let docs: Vec<Arc<Doc>> = self.docs.lock().unwrap().values().cloned().collect();
        for doc in docs {
            if done.contains(&doc.hex()) {
                continue;
            }
            if !Doc::text(&doc.loro.lock().unwrap()).is_empty() {
                doc.clone().publish_snapshot().await;
            }
            done.insert(doc.hex());
        }
        let _ = std::fs::write(&path, serde_json::to_vec(&done).unwrap_or_default());
    }

    /// Write the snapshot of every note that changed since the last time.
    fn save_snapshots(&self) {
        let docs: Vec<Arc<Doc>> = self.docs.lock().unwrap().values().cloned().collect();
        let dir = self.dir.join("notes");
        for doc in docs {
            if !doc.dirty.swap(false, Ordering::SeqCst) {
                continue;
            }
            let Ok(bytes) = doc.loro.lock().unwrap().export(ExportMode::Snapshot) else {
                continue;
            };
            let _ = std::fs::create_dir_all(&dir);
            let tmp = dir.join(format!("{}.tmp", doc.hex()));
            if std::fs::write(&tmp, bytes).is_ok() {
                let _ = std::fs::rename(&tmp, dir.join(format!("{}.loro", doc.hex())));
            }
        }
    }

    async fn refresh_bridges(&self) {
        let Some(panda) = &self.panda else { return };
        if self.running_mode.lock().unwrap().as_str() != "p2panda" {
            return;
        }
        if let Ok(states) = panda.bridges().await {
            *self.bridge_states.lock().unwrap() = states;
        }
    }

    /// The budget, and how much of it this device has used this epoch.
    fn spend(&self) -> Spend {
        let budget = self.settings.lock().unwrap().budget;
        let stats = self.node().and_then(|node| node.transport_stats());
        Spend {
            budget,
            used: stats
                .as_ref()
                .map(|s| s.sent_this_epoch.messages())
                .unwrap_or(0),
            seconds_left: stats
                .as_ref()
                .map(|s| s.epoch_seconds.saturating_sub(s.epoch_elapsed))
                .unwrap_or(600),
        }
    }

    /// Make the streams match `active`, the note the view has open. Calls are serialized and
    /// each applies the latest wish, so quick switches can't leave the wrong note active.
    async fn apply_active(&self) {
        let mut applied = self.applied.lock().await;
        let wanted = self.active.lock().unwrap().clone();
        if *applied == wanted {
            return;
        }
        let find = |id: &Option<String>| {
            id.as_ref()
                .and_then(|id| self.docs.lock().unwrap().get(id).cloned())
                .and_then(|doc| doc.stream.lock().unwrap().clone())
        };
        if let Some(stream) = find(&applied) {
            stream.set_active(false).await;
        }
        // A note that isn't subscribed yet is activated once it is (see `subscribe`).
        *applied = match find(&wanted) {
            Some(stream) => {
                stream.set_active(true).await;
                wanted
            }
            None => None,
        };
    }

    async fn subscribe(&self, node: &Node, doc: Arc<Doc>) {
        if doc.subscribing.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Some(panda) = &self.panda {
            panda.set_label(Topic::from(doc.id), doc.display_name());
        }
        let subscription = DocSubscription {
            doc: Arc::downgrade(&doc),
        };
        match node
            .stream_in_background(Topic::from(doc.id), subscription)
            .await
        {
            Ok(stream) => {
                *doc.stream.lock().unwrap() = Some(Arc::new(stream));
                info!(doc = %doc.hex(), "note subscribed");
                self.apply_active().await;
                // Changes made before the node was up have not been published yet.
                if doc.unpublished.swap(false, Ordering::SeqCst) {
                    doc.clone().publish_snapshot().await;
                }
            }
            Err(error) => {
                doc.subscribing.store(false, Ordering::SeqCst);
                error!(doc = %doc.hex(), "can't subscribe to note: {error}");
            }
        }
    }
}

/// One Reflection document.
pub struct Doc {
    id: [u8; 32],
    loro: Mutex<LoroDoc>,
    versions: Mutex<VecDeque<(u64, Frontiers)>>,
    next_version: AtomicU64,
    stream: Mutex<Option<Arc<TopicStream<DocSubscription>>>>,
    subscribing: AtomicBool,
    unpublished: AtomicBool,
    /// Our own Loro changes up to this counter have been published.
    published_counter: Mutex<i32>,
    publish_scheduled: AtomicBool,
    last_publish: Mutex<Option<Instant>>,
    /// Bumped on every publish; a snapshot is due when it stops changing.
    edit_generation: AtomicU64,
    /// Our cursor and selection bound, and its sending.
    own_cursor: Mutex<(Option<loro::cursor::Cursor>, Option<loro::cursor::Cursor>)>,
    cursor_scheduled: AtomicBool,
    last_cursor_sent: Mutex<Option<Instant>>,
    /// Other people's cursors: author -> (timestamp, cursor, selection bound).
    cursors: Mutex<BTreeMap<String, (u64, loro::cursor::Cursor, Option<loro::cursor::Cursor>)>>,
    online: Mutex<BTreeSet<String>>,
    name: Mutex<String>,
    updated: AtomicU64,
    /// Changed since its snapshot was last saved locally.
    dirty: AtomicBool,
}

pub struct LocalEdit {
    /// Whether the text changed (and there is something to publish).
    changed: bool,
    /// A version whose text is exactly what the view sent.
    edit_version: u64,
}

impl Doc {
    fn new(id: [u8; 32]) -> Arc<Self> {
        let doc = Arc::new(Self {
            id,
            loro: Mutex::new(LoroDoc::new()),
            versions: Mutex::new(VecDeque::new()),
            next_version: AtomicU64::new(1),
            stream: Mutex::new(None),
            subscribing: AtomicBool::new(false),
            unpublished: AtomicBool::new(false),
            published_counter: Mutex::new(0),
            publish_scheduled: AtomicBool::new(false),
            last_publish: Mutex::new(None),
            edit_generation: AtomicU64::new(0),
            own_cursor: Mutex::new((None, None)),
            cursor_scheduled: AtomicBool::new(false),
            last_cursor_sent: Mutex::new(None),
            cursors: Mutex::new(BTreeMap::new()),
            online: Mutex::new(BTreeSet::new()),
            name: Mutex::new(String::new()),
            updated: AtomicU64::new(0),
            dirty: AtomicBool::new(false),
        });
        let frontiers = doc.loro.lock().unwrap().oplog_frontiers();
        doc.record(frontiers);
        doc
    }

    fn hex(&self) -> String {
        hex::encode(self.id)
    }

    fn text(loro: &LoroDoc) -> String {
        loro.get_text(&*TEXT_ID).to_string()
    }

    /// Remember a version the view may edit from; returns its number.
    fn record(&self, frontiers: Frontiers) -> u64 {
        let mut versions = self.versions.lock().unwrap();
        if let Some((number, last)) = versions.back() {
            if *last == frontiers {
                return *number;
            }
        }
        let number = self.next_version.fetch_add(1, Ordering::SeqCst);
        versions.push_back((number, frontiers));
        while versions.len() > KEPT_VERSIONS {
            versions.pop_front();
        }
        number
    }

    fn version_frontiers(&self, number: u64) -> Option<Frontiers> {
        self.versions
            .lock()
            .unwrap()
            .iter()
            .find(|(n, _)| *n == number)
            .map(|(_, f)| f.clone())
    }

    fn current_version(&self) -> u64 {
        self.versions
            .lock()
            .unwrap()
            .back()
            .map(|(n, _)| *n)
            .unwrap_or(0)
    }

    /// Apply the user's text, which they edited starting from version `base`.
    ///
    /// The view sends whole texts. Diffing that against the current text would undo every
    /// remote change the user had not seen yet, so the edit is made on a fork of the document
    /// at `base` and merged in, the way two peers' edits merge. The fork writes as this
    /// document's own peer, which is safe when `base` already contains all our earlier
    /// changes (always, unless the view is out of date); otherwise we diff against the
    /// current text.
    fn local_edit(&self, base: Option<u64>, text: &str) -> Result<LocalEdit, String> {
        let loro = self.loro.lock().unwrap();
        let current = loro.oplog_frontiers();
        let base_frontiers = base
            .and_then(|b| self.version_frontiers(b))
            .filter(|f| self.contains_own_changes(&loro, f))
            .unwrap_or_else(|| current.clone());

        let before = loro.oplog_vv();
        let edit_frontiers = if base_frontiers == current {
            let text_container = loro.get_text(&*TEXT_ID);
            if text_container.to_string() == text {
                drop(loro);
                let v = self.current_version();
                return Ok(LocalEdit {
                    changed: false,
                    edit_version: v,
                });
            }
            text_container
                .update(text, UpdateOptions::default())
                .map_err(|e| e.to_string())?;
            loro.commit();
            loro.oplog_frontiers()
        } else {
            let fork = loro.fork_at(&base_frontiers).map_err(|e| e.to_string())?;
            fork.set_peer_id(loro.peer_id())
                .map_err(|e| e.to_string())?;
            let fork_text = fork.get_text(&*TEXT_ID);
            if fork_text.to_string() != text {
                fork_text
                    .update(text, UpdateOptions::default())
                    .map_err(|e| e.to_string())?;
                fork.commit();
            }
            let update = fork
                .export(ExportMode::updates(&loro.oplog_vv()))
                .map_err(|e| e.to_string())?;
            loro.import(&update).map_err(|e| e.to_string())?;
            fork.oplog_frontiers()
        };

        let changed = loro.oplog_vv() != before;
        let merged = loro.oplog_frontiers();
        let new_text = Self::text(&loro);
        drop(loro);

        let edit_version = self.record(edit_frontiers);
        self.record(merged);
        self.changed(&new_text);
        if self.stream.lock().unwrap().is_none() {
            self.unpublished.store(true, Ordering::SeqCst);
        }
        Ok(LocalEdit {
            changed,
            edit_version,
        })
    }

    /// Whether `frontiers` includes every change this peer has made.
    fn contains_own_changes(&self, loro: &LoroDoc, frontiers: &Frontiers) -> bool {
        let me = loro.peer_id();
        let ours = loro.oplog_vv().get(&me).copied().unwrap_or(0);
        if ours == 0 {
            return true;
        }
        match loro.frontiers_to_vv(frontiers) {
            Some(vv) => vv.get(&me).copied().unwrap_or(0) >= ours,
            None => false,
        }
    }

    /// Bytes from another peer: a Loro update or snapshot.
    fn import_remote(&self, bytes: &[u8]) {
        let loro = self.loro.lock().unwrap();
        if let Err(error) = loro.import_with(bytes, "delta") {
            warn!(doc = %self.hex(), "received invalid change: {error}");
            return;
        }
        // Our own changes coming back (replayed from the store) were published before.
        let ours = Self::own_counter(&loro);
        {
            let mut published = self.published_counter.lock().unwrap();
            *published = (*published).max(ours);
        }
        let frontiers = loro.oplog_frontiers();
        let text = Self::text(&loro);
        drop(loro);
        self.record(frontiers);
        self.changed(&text);
    }

    fn changed(&self, text: &str) {
        self.dirty.store(true, Ordering::SeqCst);
        let name = name_of(text);
        *self.name.lock().unwrap() = name;
        self.updated.store(now_secs(), Ordering::SeqCst);
    }

    /// Our cursor, at UTF-16 offsets into the text of version `base`.
    fn set_own_cursor(&self, base: Option<u64>, position: i64, anchor: i64) -> Result<(), String> {
        let loro = self.loro.lock().unwrap();
        let frontiers = base
            .and_then(|b| self.version_frontiers(b))
            .filter(|f| *f != loro.oplog_frontiers());
        let cursor_at = |doc: &LoroDoc| {
            let text = doc.get_text(&*TEXT_ID);
            let content = text.to_string();
            let at =
                |utf16: i64| text.get_cursor(utf16_to_chars(&content, utf16), Default::default());
            let insert = at(position);
            let bound = (anchor != position && anchor >= 0)
                .then(|| at(anchor))
                .flatten();
            (insert, bound)
        };
        let cursors = match frontiers {
            Some(frontiers) => {
                let fork = loro.fork_at(&frontiers).map_err(|e| e.to_string())?;
                cursor_at(&fork)
            }
            None => cursor_at(&loro),
        };
        *self.own_cursor.lock().unwrap() = cursors;
        Ok(())
    }

    /// Send our cursor, at most once per `wait`.
    fn request_cursor_publish(self: Arc<Self>, wait: Duration) {
        if self.cursor_scheduled.swap(true, Ordering::SeqCst) {
            return;
        }
        let wait = match *self.last_cursor_sent.lock().unwrap() {
            Some(at) => wait.saturating_sub(at.elapsed()),
            None => Duration::ZERO,
        };
        tokio::spawn(async move {
            tokio::time::sleep(wait).await;
            self.cursor_scheduled.store(false, Ordering::SeqCst);
            let cursor = self.own_cursor.lock().unwrap().clone();
            self.send_cursor_now(Some(cursor)).await;
        });
    }

    /// Send a cursor (`None`: "no cursor", which takes it off the others' screens).
    async fn send_cursor_now(
        self: Arc<Self>,
        cursor: Option<(Option<loro::cursor::Cursor>, Option<loro::cursor::Cursor>)>,
    ) {
        let stream = self.stream.lock().unwrap().clone();
        let Some(stream) = stream else { return };
        let (insert_cursor, selection_bound) = cursor.unwrap_or((None, None));
        let data = EphemeralData::Cursor {
            insert_cursor,
            selection_bound,
        };
        let bytes = match postcard::to_allocvec(&data) {
            Ok(bytes) => bytes,
            Err(error) => {
                error!("can't encode cursor: {error}");
                return;
            }
        };
        *self.last_cursor_sent.lock().unwrap() = Some(Instant::now());
        if let Err(error) = stream.publish_ephemeral(bytes).await {
            warn!(doc = %self.hex(), "can't send cursor: {error}");
        }
    }

    /// Someone's cursor (an ephemeral message), unless we already have a newer one.
    fn remote_cursor(&self, author: &str, timestamp: u64, data: &[u8]) {
        let Ok(EphemeralData::Cursor {
            insert_cursor,
            selection_bound,
        }) = postcard::from_bytes::<EphemeralData>(data)
        else {
            return;
        };
        let mut cursors = self.cursors.lock().unwrap();
        if cursors
            .get(author)
            .is_some_and(|(at, _, _)| *at > timestamp)
        {
            return;
        }
        match insert_cursor {
            Some(cursor) => {
                cursors.insert(author.to_owned(), (timestamp, cursor, selection_bound));
            }
            None => {
                cursors.remove(author);
            }
        }
    }

    /// Other people's cursors in the current text, as UTF-16 offsets.
    fn cursor_positions(&self, me: &str) -> Vec<Value> {
        let cursors = self.cursors.lock().unwrap().clone();
        if cursors.is_empty() {
            return Vec::new();
        }
        let loro = self.loro.lock().unwrap();
        let text = Self::text(&loro);
        let pos = |cursor: &loro::cursor::Cursor| {
            loro.get_cursor_pos(cursor)
                .ok()
                .map(|result| chars_to_utf16(&text, result.current.pos))
        };
        cursors
            .iter()
            .filter(|(author, _)| author.as_str() != me)
            .filter_map(|(author, (_, insert, bound))| {
                let at = pos(insert)?;
                let anchor = bound.as_ref().and_then(pos).unwrap_or(at);
                Some(json!({ "author": author, "pos": at, "anchor": anchor }))
            })
            .collect()
    }

    /// How far this peer's own changes go (the end of its counter range).
    fn own_counter(loro: &LoroDoc) -> i32 {
        loro.oplog_vv().get(&loro.peer_id()).copied().unwrap_or(0)
    }

    /// There are local changes to publish. They are merged with whatever follows within
    /// `interval` of the last publish and sent as one Loro update.
    fn request_publish(self: Arc<Self>, interval: Duration) {
        if self.publish_scheduled.swap(true, Ordering::SeqCst) {
            return;
        }
        let wait = match *self.last_publish.lock().unwrap() {
            Some(at) => interval.saturating_sub(at.elapsed()),
            None => Duration::ZERO,
        };
        tokio::spawn(async move {
            tokio::time::sleep(wait).await;
            self.publish_scheduled.store(false, Ordering::SeqCst);
            self.publish_pending().await;
        });
    }

    async fn publish_pending(self: Arc<Self>) {
        let stream = self.stream.lock().unwrap().clone();
        let Some(stream) = stream else {
            // Published as part of a snapshot once the note is subscribed.
            self.unpublished.store(true, Ordering::SeqCst);
            return;
        };
        let (update, counter) = {
            let loro = self.loro.lock().unwrap();
            let ours = Self::own_counter(&loro);
            let published = *self.published_counter.lock().unwrap();
            if ours <= published {
                return;
            }
            // Everything of everyone else, and ours up to what was published: the update then
            // holds only our new changes.
            let mut from: VersionVector = loro.oplog_vv();
            from.insert(loro.peer_id(), published);
            match loro.export(ExportMode::updates(&from)) {
                Ok(update) => (update, ours),
                Err(error) => {
                    error!("can't export changes: {error}");
                    return;
                }
            }
        };
        *self.last_publish.lock().unwrap() = Some(Instant::now());
        match stream.publish_delta(update).await {
            Ok(()) => {
                let mut published = self.published_counter.lock().unwrap();
                *published = (*published).max(counter);
            }
            Err(error) => error!(doc = %self.hex(), "can't publish change: {error}"),
        }
        self.schedule_snapshot();
    }

    /// Publish a snapshot once no edit has been published for `SNAPSHOT_QUIET`.
    fn schedule_snapshot(self: &Arc<Self>) {
        let generation = self.edit_generation.fetch_add(1, Ordering::SeqCst) + 1;
        let doc = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(SNAPSHOT_QUIET).await;
            if doc.edit_generation.load(Ordering::SeqCst) == generation
                && !doc.publish_scheduled.load(Ordering::SeqCst)
            {
                doc.publish_snapshot().await;
            }
        });
    }

    async fn publish_snapshot(self: Arc<Self>) {
        let stream = self.stream.lock().unwrap().clone();
        let Some(stream) = stream else { return };
        let (snapshot, counter) = {
            let loro = self.loro.lock().unwrap();
            (loro.export(ExportMode::Snapshot), Self::own_counter(&loro))
        };
        let snapshot = match snapshot {
            Ok(bytes) => bytes,
            Err(error) => {
                error!("can't export snapshot: {error}");
                return;
            }
        };
        // The snapshot carries every change made so far.
        {
            let mut published = self.published_counter.lock().unwrap();
            *published = (*published).max(counter);
        }
        if let Err(error) = stream.publish_snapshot(snapshot).await {
            error!(doc = %self.hex(), "can't publish snapshot: {error}");
        }
        let name = self.name.lock().unwrap().clone();
        if let Err(error) = stream.set_name(Some(name)).await {
            warn!("can't store note name: {error}");
        }
    }

    fn display_name(&self) -> String {
        let name = self.name.lock().unwrap().clone();
        if name.is_empty() {
            "Untitled".to_owned()
        } else {
            name
        }
    }

    /// Other people with the document open (presence includes this device itself).
    fn others_online(&self, me: &str) -> Vec<String> {
        self.online
            .lock()
            .unwrap()
            .iter()
            .filter(|key| key.as_str() != me)
            .cloned()
            .collect()
    }

    fn summary(&self, me: &str) -> Value {
        json!({
            "id": self.hex(),
            "name": self.display_name(),
            "online": self.others_online(me).len(),
            "updated": self.updated.load(Ordering::SeqCst),
            "subscribed": self.stream.lock().unwrap().is_some(),
        })
    }

    fn detail(&self, me: &str) -> Value {
        let text = Self::text(&self.loro.lock().unwrap());
        json!({
            "id": self.hex(),
            "name": self.display_name(),
            "text": text,
            "version": self.current_version().to_string(),
            "online": self.others_online(me),
            "cursors": self.cursor_positions(me),
            "subscribed": self.stream.lock().unwrap().is_some(),
        })
    }
}

/// Receives a document's traffic from `reflection-node`.
struct DocSubscription {
    doc: Weak<Doc>,
}

impl TopicSubscription for DocSubscription {
    fn bytes_received(&self, _author: VerifyingKey, data: Vec<u8>) {
        if let Some(doc) = self.doc.upgrade() {
            doc.import_remote(&data);
        }
    }

    fn ephemeral_bytes_received(&self, author: VerifyingKey, timestamp: u64, data: Vec<u8>) {
        if let Some(doc) = self.doc.upgrade() {
            doc.remote_cursor(&author.to_hex(), timestamp, &data);
        }
    }

    fn author_joined(&self, author: VerifyingKey) {
        if let Some(doc) = self.doc.upgrade() {
            doc.online.lock().unwrap().insert(author.to_hex());
        }
    }

    fn author_left(&self, author: VerifyingKey) {
        if let Some(doc) = self.doc.upgrade() {
            doc.online.lock().unwrap().remove(&author.to_hex());
            doc.cursors.lock().unwrap().remove(&author.to_hex());
        }
    }

    fn error(&self, error: TopicSubscriptionError) {
        error!("note subscription error: {error}");
    }
}

/// A UTF-16 offset (QML) as a character offset (Loro), clamped to the text.
fn utf16_to_chars(text: &str, utf16: i64) -> usize {
    let mut units = 0i64;
    for (chars, c) in text.chars().enumerate() {
        if units >= utf16 {
            return chars;
        }
        units += c.len_utf16() as i64;
    }
    text.chars().count()
}

/// A character offset (Loro) as a UTF-16 offset (QML).
fn chars_to_utf16(text: &str, chars: usize) -> usize {
    text.chars().take(chars).map(char::len_utf16).sum()
}

/// The first non-empty line, without Markdown heading marks.
fn name_of(text: &str) -> String {
    let line = text
        .lines()
        .map(|l| l.trim().trim_start_matches('#').trim())
        .find(|l| !l.is_empty())
        .unwrap_or("");
    line.chars().take(NAME_LENGTH).collect()
}

fn parse_id(id_hex: &str) -> Result<[u8; 32], String> {
    let cleaned: String = id_hex
        .trim()
        .trim_matches('"')
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    let bytes = hex::decode(&cleaned).map_err(|_| "A note id is 64 hex characters".to_owned())?;
    bytes
        .try_into()
        .map_err(|_| "A note id is 64 hex characters".to_owned())
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn load_or_create_key(dir: &Path) -> Result<SigningKey, String> {
    let path = dir.join("identity.key");
    if let Ok(hex_key) = std::fs::read_to_string(&path) {
        let bytes = hex::decode(hex_key.trim()).map_err(|e| e.to_string())?;
        let bytes: [u8; 32] = bytes
            .try_into()
            .map_err(|_| "identity.key is corrupt".to_owned())?;
        return Ok(SigningKey::from_bytes(&bytes));
    }
    let key = SigningKey::generate();
    std::fs::write(&path, key.to_hex()).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use reflection_node::logos::MemoryBus;

    /// A device: an engine on the in-process bus, with its own data directory.
    fn device(dir: &Path) -> Engine {
        // MemoryBus needs a runtime to spawn its relay task on.
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let bus = {
            let _guard = rt.enter();
            Arc::new(MemoryBus::new())
        };
        std::mem::forget(rt);
        let engine = Engine::start(dir.to_owned(), bus, None);
        engine.wait_ready();
        engine
    }

    fn tempdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "reflection-core-{name}-{}",
            hex::encode(rand::random::<[u8; 8]>())
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn text(engine: &Engine, id: &str) -> String {
        engine.state(id)["current"]["text"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    }

    fn version(engine: &Engine, id: &str) -> String {
        engine.state(id)["current"]["version"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    fn wait_for(engine: &Engine, id: &str, expected: &str) {
        for _ in 0..200 {
            if text(engine, id) == expected {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("expected {expected:?}, got {:?}", text(engine, id));
    }

    #[test]
    fn two_devices_edit_one_note() {
        let a = device(&tempdir("a"));
        let b = device(&tempdir("b"));

        let id = a.create("Shopping").unwrap()["id"]
            .as_str()
            .unwrap()
            .to_owned();
        b.join(&id).unwrap();
        wait_for(&b, &id, "Shopping\n");
        assert_eq!(b.state("")["docs"][0]["name"], "Shopping");

        let base = version(&b, &id);
        b.set_text(&id, &base, "Shopping\n- milk\n").unwrap();
        wait_for(&a, &id, "Shopping\n- milk\n");

        // Presence lists the other person, not this device.
        let b_me = b.state(&id)["me"].clone();
        for _ in 0..200 {
            if a.state(&id)["current"]["online"] == json!([b_me]) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("A sees online: {}", a.state(&id)["current"]["online"]);
    }

    #[test]
    fn edits_from_a_stale_base_keep_remote_changes() {
        let a = device(&tempdir("a"));
        let b = device(&tempdir("b"));
        let id = a.create("List").unwrap()["id"].as_str().unwrap().to_owned();
        b.join(&id).unwrap();
        wait_for(&b, &id, "List\n");

        // B's view saw "List\n". Meanwhile A appends a line.
        let b_base = version(&b, &id);
        a.set_text(&id, &version(&a, &id), "List\nfrom a\n")
            .unwrap();
        wait_for(&b, &id, "List\nfrom a\n");

        // B's user, not having seen it, renames the title. A's line must survive.
        let reply = b.set_text(&id, &b_base, "Groceries\n").unwrap();
        assert_eq!(reply["current"]["text"], "Groceries\nfrom a\n");
        wait_for(&a, &id, "Groceries\nfrom a\n");
    }

    #[test]
    fn typing_while_a_reply_is_in_flight() {
        let a = device(&tempdir("a"));
        let id = a.create("").unwrap()["id"].as_str().unwrap().to_owned();
        let v0 = version(&a, &id);
        // The view sends "ab"; while the reply is in flight the user types "c".
        let reply = a.set_text(&id, &v0, "ab").unwrap();
        let edit_version = reply["current"]["editVersion"].as_str().unwrap().to_owned();
        // The next send is based on the text that was sent ("ab").
        a.set_text(&id, &edit_version, "abc").unwrap();
        assert_eq!(text(&a, &id), "abc");
    }

    #[test]
    fn concurrent_edits_converge() {
        let a = device(&tempdir("a"));
        let b = device(&tempdir("b"));
        let id = a.create("x").unwrap()["id"].as_str().unwrap().to_owned();
        b.join(&id).unwrap();
        wait_for(&b, &id, "x\n");
        let (va, vb) = (version(&a, &id), version(&b, &id));
        a.set_text(&id, &va, "x\nA\n").unwrap();
        b.set_text(&id, &vb, "x\nB\n").unwrap();
        for _ in 0..200 {
            let (ta, tb) = (text(&a, &id), text(&b, &id));
            if ta == tb && ta.contains('A') && ta.contains('B') {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!(
            "did not converge: {:?} vs {:?}",
            text(&a, &id),
            text(&b, &id)
        );
    }

    #[test]
    fn notes_survive_a_restart() {
        let dir = tempdir("restart");
        let id = {
            let a = device(&dir);
            let id = a.create("Diary").unwrap()["id"]
                .as_str()
                .unwrap()
                .to_owned();
            // Wait for the subscription, so the change is stored.
            for _ in 0..100 {
                if a.state(&id)["current"]["subscribed"] == true {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            a.set_text(&id, &version(&a, &id), "Diary\nday one\n")
                .unwrap();
            std::thread::sleep(Duration::from_millis(500));
            id
        };
        let a = device(&dir);
        wait_for(&a, &id, "Diary\nday one\n");
        assert_eq!(a.state("")["docs"][0]["name"], "Diary");
    }

    fn sent(engine: &Engine, kind: &str) -> u64 {
        engine.stats()["stats"]["sent"][kind].as_u64().unwrap()
    }

    #[test]
    fn typing_is_merged_into_few_messages() {
        let a = device(&tempdir("a"));
        let b = device(&tempdir("b"));
        let id = a.create("").unwrap()["id"].as_str().unwrap().to_owned();
        b.join(&id).unwrap();
        // Both have the note open.
        wait_for(&b, &id, "");
        std::thread::sleep(Duration::from_millis(500));

        // Thirty keystrokes, one every 50 ms, as the view sends them.
        let mut base = version(&a, &id);
        let mut typed = String::new();
        for c in "the quick brown fox jumps over".chars() {
            typed.push(c);
            let reply = a.set_text(&id, &base, &typed).unwrap();
            base = reply["current"]["editVersion"].as_str().unwrap().to_owned();
            std::thread::sleep(Duration::from_millis(50));
        }
        wait_for(&b, &id, &typed);
        // 30 keystrokes over 1.5 s at one publish per second: the first keystroke, then the
        // rest merged into one or two.
        assert!(sent(&a, "ops") <= 3, "{}", a.stats());

        // One snapshot once the typing stopped.
        for _ in 0..300 {
            if sent(&a, "snapshots") >= 1 {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        std::thread::sleep(Duration::from_secs(1));
        assert_eq!(sent(&a, "snapshots"), 1, "{}", a.stats());
        assert!(sent(&a, "ops") <= 3, "{}", a.stats());
    }

    #[test]
    fn notes_not_open_send_nothing() {
        let a = device(&tempdir("a"));
        for title in ["one", "two", "three"] {
            a.create(title).unwrap();
        }
        // The view shows the list, no note open. Creating published each note once.
        std::thread::sleep(Duration::from_secs(2));
        a.state("");
        let before = a.stats()["stats"]["sent"].clone();
        std::thread::sleep(Duration::from_secs(6));
        a.state("");
        assert_eq!(a.stats()["stats"]["sent"], before);
        assert_eq!(before["heads"], 0);
        assert_eq!(before["presence"], 0);
    }

    #[test]
    fn rejects_bad_ids() {
        let a = device(&tempdir("ids"));
        assert!(a.join("not hex").is_err());
        assert!(a.join("abcd").is_err());
        let id = hex::encode([7u8; 32]);
        assert_eq!(a.join(&id.to_uppercase()).unwrap()["id"], id);
    }

    #[test]
    fn cursors_go_both_ways_when_shared() {
        let a = device(&tempdir("a"));
        let b = device(&tempdir("b"));
        let id = a.create("hello world").unwrap()["id"]
            .as_str()
            .unwrap()
            .to_owned();
        b.join(&id).unwrap();
        wait_for(&b, &id, "hello world\n");
        wait_for(&a, &id, "hello world\n");

        // Not shared: nothing goes out.
        a.set_cursor(&id, &version(&a, &id), 6, 6).unwrap();
        std::thread::sleep(Duration::from_millis(1500));
        assert_eq!(sent(&a, "cursors"), 0);
        assert_eq!(b.state(&id)["current"]["cursors"], json!([]));

        // Shared: B sees A's cursor before "world", and it stays there as B edits before it.
        a.set_settings(r#"{"shareCursor": true}"#).unwrap();
        a.set_cursor(&id, &version(&a, &id), 6, 6).unwrap();
        let a_me = a.state(&id)["me"].clone();
        let mut seen = Value::Null;
        for _ in 0..100 {
            seen = b.state(&id)["current"]["cursors"].clone();
            if seen != json!([]) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(seen, json!([{ "author": a_me, "pos": 6, "anchor": 6 }]));
        b.set_text(&id, &version(&b, &id), "oh, hello world\n")
            .unwrap();
        assert_eq!(b.state(&id)["current"]["cursors"][0]["pos"], 10);
        assert_eq!(sent(&a, "cursors"), 1);

        // Turning sharing off takes it off B's screen.
        a.set_settings(r#"{"shareCursor": false}"#).unwrap();
        for _ in 0..100 {
            if b.state(&id)["current"]["cursors"] == json!([]) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("cursor still shown: {}", b.state(&id)["current"]["cursors"]);
    }

    #[test]
    fn settings_persist_and_apply_the_budget() {
        let dir = tempdir("settings");
        {
            let a = device(&dir);
            assert_eq!(a.settings()["settings"]["budget"], Value::Null);
            a.set_settings(r#"{"budget": "250"}"#).unwrap();
            assert!(a.set_settings(r#"{"budget": "-3"}"#).is_err());
            assert_eq!(a.stats()["stats"]["budget"], 250);
        }
        let a = device(&dir);
        assert_eq!(a.settings()["settings"]["budget"], 250);
        assert_eq!(a.stats()["stats"]["budget"], 250);
        a.set_settings(r#"{"budget": null}"#).unwrap();
        assert_eq!(a.stats()["stats"]["budget"], Value::Null);
    }

    #[test]
    fn pacing_follows_the_budget() {
        let spend = |budget: Option<u64>, used: u64, seconds_left: u64| Spend {
            budget,
            used,
            seconds_left,
        };
        // No limit, or less than half used: the base interval.
        assert_eq!(spend(None, 5000, 300).publish_wait(), PUBLISH_INTERVAL);
        assert_eq!(spend(Some(100), 49, 300).publish_wait(), PUBLISH_INTERVAL);
        // Past half: what is left (less the reserve of 5) spread over the rest of the epoch.
        assert_eq!(
            spend(Some(100), 55, 400).publish_wait(),
            Duration::from_secs(10)
        );
        assert_eq!(spend(Some(600), 300, 100).publish_wait(), PUBLISH_INTERVAL);
        // Only the reserve left: wait for the next epoch.
        assert_eq!(
            spend(Some(100), 95, 120).publish_wait(),
            Duration::from_secs(121)
        );
        // Cursors: half as often as edits, none when short.
        assert_eq!(
            spend(Some(100), 55, 400).cursor_wait(),
            Some(Duration::from_secs(20))
        );
        assert_eq!(spend(Some(100), 86, 400).cursor_wait(), None);
        assert_eq!(spend(None, 0, 600).cursor_wait(), Some(CURSOR_INTERVAL));
    }

    #[test]
    fn utf16_offsets() {
        let text = "a😀b";
        assert_eq!(utf16_to_chars(text, 3), 2);
        assert_eq!(chars_to_utf16(text, 2), 3);
        assert_eq!(utf16_to_chars(text, 99), 3);
    }

    #[test]
    fn names_drop_heading_marks() {
        assert_eq!(name_of("\n## Plan for May\nbody"), "Plan for May");
        assert_eq!(name_of("#\n# \nTitle"), "Title");
    }
}
