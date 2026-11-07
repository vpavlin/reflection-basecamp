//! The Reflection engine of the Basecamp app: documents, their Loro text, and sync through
//! `reflection-node` over a Logos `DeliveryBus`.
//!
//! Wire-compatible with Reflection's GTK document layer (`reflection-doc`): one Loro text
//! container `document`, local changes published as Loro updates ("deltas"), and a Loro snapshot
//! published as a prune a few seconds after the last change.
//!
//! Threading: every public method is called on the host's module thread and returns quickly.
//! Anything that touches the network or the database runs on the engine's own tokio runtime.
//! Nothing here waits for the transport, which itself waits for the module thread (Logos IPC).
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use loro::{ExportMode, Frontiers, LoroDoc, UpdateOptions};
use p2panda::{Hash, SigningKey, Topic, VerifyingKey};
use reflection_node::logos::DeliveryBus;
use reflection_node::{Node, TopicStream, TopicSubscription, TopicSubscriptionError};
use serde_json::{json, Value};
use tokio::runtime::Runtime;
use tracing::{error, info, warn};

/// The text container of a Reflection document (same id as `reflection-doc`).
static TEXT_ID: LazyLock<loro::ContainerID> =
    LazyLock::new(|| loro::ContainerID::new_root("document", loro::ContainerType::Text));

/// Reflection's network id (`reflection-doc`: `Hash::digest(b"reflection-v2")`).
const NETWORK_NAME: &[u8] = b"reflection-v2";

/// `reflection-doc` publishes a snapshot this long after the last change.
const SNAPSHOT_DELAY: Duration = Duration::from_secs(5);

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
    node: tokio::sync::OnceCell<Node>,
    node_error: Mutex<Option<String>>,
    docs: Mutex<BTreeMap<String, Arc<Doc>>>,
    me: Mutex<Option<String>>,
    transport: Mutex<Value>,
}

impl Engine {
    /// Start the engine. The node comes up in the background; documents created before that
    /// are kept in memory and published once it is ready.
    pub fn start(persist_dir: PathBuf, bus: Arc<dyn DeliveryBus>) -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("reflection")
            .enable_all()
            .build()
            .expect("tokio runtime");
        let inner = Arc::new(Inner {
            node: tokio::sync::OnceCell::new(),
            node_error: Mutex::new(None),
            docs: Mutex::new(BTreeMap::new()),
            me: Mutex::new(None),
            transport: Mutex::new(json!({ "status": "starting" })),
        });
        let task_inner = inner.clone();
        rt.spawn(async move {
            if let Err(error) = task_inner.clone().start_node(persist_dir, bus).await {
                error!("reflection node failed to start: {error}");
                *task_inner.node_error.lock().unwrap() = Some(error);
            }
        });
        Self { rt, inner }
    }

    /// Transport state shown to the user (set by the module from loam_core).
    pub fn set_transport(&self, transport: Value) {
        *self.inner.transport.lock().unwrap() = transport;
    }

    /// Everything the view shows: the document list, and the open document if `doc_id` is set.
    pub fn state(&self, doc_id: &str) -> Value {
        let docs: Vec<Arc<Doc>> = self.inner.docs.lock().unwrap().values().cloned().collect();
        let mut list: Vec<Value> = docs.iter().map(|doc| doc.summary()).collect();
        list.sort_by(|a, b| b["updated"].as_u64().cmp(&a["updated"].as_u64()));
        let current = if doc_id.is_empty() {
            Value::Null
        } else {
            match self.doc(doc_id) {
                Some(doc) => doc.detail(),
                None => Value::Null,
            }
        };
        let node = match (&*self.inner.node_error.lock().unwrap(), self.inner.node.get()) {
            (Some(error), _) => format!("error: {error}"),
            (None, Some(_)) => "ready".to_owned(),
            (None, None) => "starting".to_owned(),
        };
        json!({
            "ok": true,
            "node": node,
            "me": *self.inner.me.lock().unwrap(),
            "transport": *self.inner.transport.lock().unwrap(),
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
        Ok(json!({ "ok": true, "id": doc.hex(), "current": doc.detail() }))
    }

    /// Open a document someone shared, by its id (64 hex characters).
    pub fn join(&self, id_hex: &str) -> Result<Value, String> {
        let id = parse_id(id_hex)?;
        let hex = hex::encode(id);
        if let Some(doc) = self.doc(&hex) {
            return Ok(json!({ "ok": true, "id": hex, "current": doc.detail() }));
        }
        let doc = Doc::new(id);
        self.add(doc.clone());
        Ok(json!({ "ok": true, "id": hex, "current": doc.detail() }))
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
        if let Some(update) = edit.update {
            let doc = doc.clone();
            self.rt.spawn(async move { doc.publish(update).await });
        }
        let mut detail = doc.detail();
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
        let inner = self.inner.clone();
        self.rt.spawn(async move {
            let stream = doc.stream.lock().unwrap().take();
            if let Some(stream) = stream {
                if let Ok(stream) = Arc::try_unwrap(stream) {
                    let _ = stream.unsubscribe().await;
                }
            }
            if let Some(node) = inner.node.get() {
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
            if let Some(node) = inner.node.get() {
                Inner::subscribe(node, doc).await;
            }
            // Otherwise start_node subscribes every document once the node is up.
        });
    }

    #[cfg(test)]
    pub(crate) fn wait_ready(&self) {
        for _ in 0..200 {
            if self.inner.node.get().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!("node did not start: {:?}", self.inner.node_error.lock().unwrap());
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        // Leave the runtime's threads to finish on their own; the host is unloading us.
        let rt = std::mem::replace(
            &mut self.rt,
            tokio::runtime::Builder::new_current_thread().build().unwrap(),
        );
        rt.shutdown_background();
    }
}

impl Inner {
    async fn start_node(
        self: Arc<Self>,
        persist_dir: PathBuf,
        bus: Arc<dyn DeliveryBus>,
    ) -> Result<(), String> {
        std::fs::create_dir_all(&persist_dir).map_err(|e| e.to_string())?;
        let key = load_or_create_key(&persist_dir)?;
        *self.me.lock().unwrap() = Some(key.verifying_key().to_hex());

        let node = Node::with_delivery_bus(key, Hash::digest(NETWORK_NAME), Some(&persist_dir), bus)
            .await
            .map_err(|e| e.to_string())?;

        // Documents known from earlier runs. Their content comes back from the node's
        // operation store when we subscribe.
        let known = node.topics().await.map_err(|e| e.to_string())?;
        for topic in known {
            let id = topic.topic.to_bytes();
            let hex = hex::encode(id);
            let mut docs = self.docs.lock().unwrap();
            docs.entry(hex).or_insert_with(|| {
                let doc = Doc::new(id);
                if let Some(name) = topic.name.as_ref().filter(|n| !n.is_empty()) {
                    *doc.name.lock().unwrap() = name.clone();
                }
                doc
            });
        }

        let node = match self.node.set(node) {
            Ok(()) => self.node.get().unwrap(),
            Err(_) => return Err("node started twice".into()),
        };
        info!("reflection node ready");

        let docs: Vec<Arc<Doc>> = self.docs.lock().unwrap().values().cloned().collect();
        for doc in docs {
            Self::subscribe(node, doc).await;
        }
        Ok(())
    }

    async fn subscribe(node: &Node, doc: Arc<Doc>) {
        if doc.subscribing.swap(true, Ordering::SeqCst) {
            return;
        }
        let subscription = DocSubscription {
            doc: Arc::downgrade(&doc),
        };
        match node.stream(Topic::from(doc.id), subscription).await {
            Ok(stream) => {
                *doc.stream.lock().unwrap() = Some(Arc::new(stream));
                info!(doc = %doc.hex(), "note subscribed");
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
    snapshot_scheduled: AtomicBool,
    online: Mutex<BTreeSet<String>>,
    name: Mutex<String>,
    updated: AtomicU64,
}

pub struct LocalEdit {
    /// The Loro update to publish, if the text changed.
    update: Option<Vec<u8>>,
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
            snapshot_scheduled: AtomicBool::new(false),
            online: Mutex::new(BTreeSet::new()),
            name: Mutex::new(String::new()),
            updated: AtomicU64::new(0),
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
                    update: None,
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
            fork.set_peer_id(loro.peer_id()).map_err(|e| e.to_string())?;
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

        let update = if loro.oplog_vv() == before {
            None
        } else {
            Some(
                loro.export(ExportMode::updates(&before))
                    .map_err(|e| e.to_string())?,
            )
        };
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
            update,
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
        let frontiers = loro.oplog_frontiers();
        let text = Self::text(&loro);
        drop(loro);
        self.record(frontiers);
        self.changed(&text);
    }

    fn changed(&self, text: &str) {
        let name = name_of(text);
        *self.name.lock().unwrap() = name;
        self.updated.store(now_secs(), Ordering::SeqCst);
    }

    async fn publish(self: Arc<Self>, update: Vec<u8>) {
        let stream = self.stream.lock().unwrap().clone();
        let Some(stream) = stream else {
            // Published as part of a snapshot once the note is subscribed.
            self.unpublished.store(true, Ordering::SeqCst);
            return;
        };
        if let Err(error) = stream.publish_delta(update).await {
            error!(doc = %self.hex(), "can't publish change: {error}");
        }
        self.schedule_snapshot();
    }

    fn schedule_snapshot(self: &Arc<Self>) {
        if self.snapshot_scheduled.swap(true, Ordering::SeqCst) {
            return;
        }
        let doc = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(SNAPSHOT_DELAY).await;
            doc.snapshot_scheduled.store(false, Ordering::SeqCst);
            doc.publish_snapshot().await;
        });
    }

    async fn publish_snapshot(self: Arc<Self>) {
        let stream = self.stream.lock().unwrap().clone();
        let Some(stream) = stream else { return };
        let snapshot = match self.loro.lock().unwrap().export(ExportMode::Snapshot) {
            Ok(bytes) => bytes,
            Err(error) => {
                error!("can't export snapshot: {error}");
                return;
            }
        };
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

    fn summary(&self) -> Value {
        json!({
            "id": self.hex(),
            "name": self.display_name(),
            "online": self.online.lock().unwrap().len(),
            "updated": self.updated.load(Ordering::SeqCst),
            "subscribed": self.stream.lock().unwrap().is_some(),
        })
    }

    fn detail(&self) -> Value {
        let text = Self::text(&self.loro.lock().unwrap());
        json!({
            "id": self.hex(),
            "name": self.display_name(),
            "text": text,
            "version": self.current_version().to_string(),
            "online": self.online.lock().unwrap().iter().cloned().collect::<Vec<_>>(),
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

    fn ephemeral_bytes_received(&self, _author: VerifyingKey, _timestamp: u64, _data: Vec<u8>) {
        // Cursors of other people; the Basecamp view does not show them yet.
    }

    fn author_joined(&self, author: VerifyingKey) {
        if let Some(doc) = self.doc.upgrade() {
            doc.online.lock().unwrap().insert(author.to_hex());
        }
    }

    fn author_left(&self, author: VerifyingKey) {
        if let Some(doc) = self.doc.upgrade() {
            doc.online.lock().unwrap().remove(&author.to_hex());
        }
    }

    fn error(&self, error: TopicSubscriptionError) {
        error!("note subscription error: {error}");
    }
}

fn name_of(text: &str) -> String {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
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
        let engine = Engine::start(dir.to_owned(), bus);
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

        let id = a.create("Shopping").unwrap()["id"].as_str().unwrap().to_owned();
        b.join(&id).unwrap();
        wait_for(&b, &id, "Shopping\n");
        assert_eq!(b.state("")["docs"][0]["name"], "Shopping");

        let base = version(&b, &id);
        b.set_text(&id, &base, "Shopping\n- milk\n").unwrap();
        wait_for(&a, &id, "Shopping\n- milk\n");
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
        a.set_text(&id, &version(&a, &id), "List\nfrom a\n").unwrap();
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
        panic!("did not converge: {:?} vs {:?}", text(&a, &id), text(&b, &id));
    }

    #[test]
    fn notes_survive_a_restart() {
        let dir = tempdir("restart");
        let id = {
            let a = device(&dir);
            let id = a.create("Diary").unwrap()["id"].as_str().unwrap().to_owned();
            // Wait for the subscription, so the change is stored.
            for _ in 0..100 {
                if a.state(&id)["current"]["subscribed"] == true {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            a.set_text(&id, &version(&a, &id), "Diary\nday one\n").unwrap();
            std::thread::sleep(Duration::from_millis(500));
            id
        };
        let a = device(&dir);
        wait_for(&a, &id, "Diary\nday one\n");
        assert_eq!(a.state("")["docs"][0]["name"], "Diary");
    }

    #[test]
    fn rejects_bad_ids() {
        let a = device(&tempdir("ids"));
        assert!(a.join("not hex").is_err());
        assert!(a.join("abcd").is_err());
        let id = hex::encode([7u8; 32]);
        assert_eq!(a.join(&id.to_uppercase()).unwrap()["id"], id);
    }
}
