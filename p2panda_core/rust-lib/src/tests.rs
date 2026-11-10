//! The engine without Basecamp: devices are engines in this process, with p2panda nodes that
//! find each other locally and a shared in-memory Logos bus.
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use ciborium::Value as Cbor;
use logos_p2panda::{Config, DeliveryBus, LogosSync, MemoryBus};
use p2panda::Topic;
use p2panda_core::SigningKey;
use serde_json::Value;

use crate::engine::{Emit, Engine, Event, Options};

fn tempdir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "p2panda-core-{name}-{}",
        hex::encode(rand::random::<[u8; 8]>())
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A tokio runtime kept alive for the in-memory bus's relay task.
fn bus() -> Arc<dyn DeliveryBus> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let bus = {
        let _guard = rt.enter();
        Arc::new(MemoryBus::new())
    };
    std::mem::forget(rt);
    bus
}

fn device(name: &str, events: Arc<Mutex<Vec<Event>>>) -> Engine {
    let emit: Emit = Arc::new(move |event| events.lock().unwrap().push(event));
    Engine::start(
        tempdir(name),
        bus(),
        Options {
            local: true,
            relay_window: Duration::from_millis(500),
        },
        emit,
    )
}

fn key() -> String {
    SigningKey::generate().to_hex()
}

fn random32() -> String {
    hex::encode(rand::random::<[u8; 32]>())
}

/// A message as an app passes it: base64 of the CBOR of a byte string (as Reflection does).
fn message(text: &str) -> String {
    let mut bytes = Vec::new();
    ciborium::into_writer(&Cbor::Bytes(text.as_bytes().to_vec()), &mut bytes).unwrap();
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn text_of(item: &Value) -> Option<String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(item["message"].as_str()?)
        .ok()?;
    match ciborium::from_reader::<Cbor, _>(&bytes[..]).ok()? {
        Cbor::Bytes(b) => String::from_utf8(b).ok(),
        _ => None,
    }
}

/// Drain until an operation with this text arrives.
fn wait_for(engine: &Engine, sub: &str, text: &str) -> Vec<Value> {
    let mut seen = Vec::new();
    for _ in 0..300 {
        let reply = engine.drain(sub, "256".parse().unwrap()).unwrap();
        for item in reply["items"].as_array().unwrap() {
            seen.push(item.clone());
        }
        if seen
            .iter()
            .any(|i| i["kind"] == "operation" && text_of(i).as_deref() == Some(text))
        {
            return seen;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("never got {text:?}; got {seen:?}");
}

#[test]
fn two_devices_share_a_topic() {
    let events_a = Arc::new(Mutex::new(Vec::new()));
    let a = device("a", events_a.clone());
    let b = device("b", Arc::new(Mutex::new(Vec::new())));
    let (network, topic) = (random32(), random32());

    a.network_open("test", &network, &key()).unwrap();
    b.network_open("test", &network, &key()).unwrap();
    let sub_a = a.open("test", &network, &topic, "start", "note").unwrap();
    let sub_b = b.open("test", &network, &topic, "start", "note").unwrap();
    let (sub_a, sub_b) = (
        sub_a["sub"].as_str().unwrap().to_owned(),
        sub_b["sub"].as_str().unwrap().to_owned(),
    );

    a.publish(&sub_a, &message("hello from a"), false).unwrap();
    let items = wait_for(&b, &sub_b, "hello from a");
    assert!(items
        .iter()
        .any(|i| i["kind"] == "operation" && i["seq"] == 0));

    // A's own operation is delivered to A too, and A was told to drain.
    wait_for(&a, &sub_a, "hello from a");
    assert!(!events_a.lock().unwrap().is_empty());
}

#[test]
fn second_app_on_a_network_needs_the_same_key() {
    let a = device("keys", Arc::new(Mutex::new(Vec::new())));
    let network = random32();
    let k = key();
    a.network_open("one", &network, &k).unwrap();
    a.network_open("two", &network, &k).unwrap();
    assert!(a.network_open("three", &network, &key()).is_err());
}

#[test]
fn bridge_needs_the_users_confirmation_and_then_carries_both_ways() {
    let a = device("bridge", Arc::new(Mutex::new(Vec::new())));
    let (network, topic) = (random32(), random32());
    a.network_open("test", &network, &key()).unwrap();
    let sub = a
        .open("test", &network, &topic, "start", "Shopping")
        .unwrap()["sub"]
        .as_str()
        .unwrap()
        .to_owned();

    // A plain Logos peer on the same network and topic.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let (logos, mut from_bridge) = rt.block_on(async {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        let network_id: p2panda::NetworkId = hex::decode(&network)
            .unwrap()
            .try_into()
            .map(|b: [u8; 32]| b.into())
            .unwrap();
        let logos = LogosSync::new(
            Arc::new(MemoryBus::new()),
            SigningKey::generate(),
            network_id,
            pool,
            Config::default(),
        )
        .await
        .unwrap();
        let topic = Topic::from(<[u8; 32]>::try_from(hex::decode(&topic).unwrap()).unwrap());
        let rx = logos.subscribe(topic).await.unwrap();
        logos.set_active(topic, true).await.unwrap();
        (logos, rx)
    });
    let topic_id = Topic::from(<[u8; 32]>::try_from(hex::decode(&topic).unwrap()).unwrap());

    // An app's request alone bridges nothing.
    assert_eq!(a.bridge(&sub, true).unwrap()["state"], "pending");
    a.publish(&sub, &message("before confirming"), false)
        .unwrap();
    std::thread::sleep(Duration::from_secs(2));
    assert!(
        from_bridge.try_recv().is_err(),
        "bridged before the user confirmed"
    );
    assert_eq!(a.bridges()["bridges"][0]["label"], "Shopping");

    // The user confirms: what A has goes to Logos, and Logos edits come to A.
    a.bridge_confirm(&network, &topic, true).unwrap();
    let got = rt.block_on(async {
        tokio::time::timeout(Duration::from_secs(20), from_bridge.recv())
            .await
            .ok()
            .flatten()
    });
    assert!(got.is_some(), "nothing reached Logos through the bridge");

    let mut body = Vec::new();
    ciborium::into_writer(&Cbor::Bytes(b"from logos".to_vec()), &mut body).unwrap();
    rt.block_on(logos.publish(topic_id, body, false)).unwrap();
    wait_for(&a, &sub, "from logos");

    // A new edit on A, after the bridge started, reaches Logos too.
    a.publish(&sub, &message("after confirming"), false)
        .unwrap();
    let mut want = Vec::new();
    ciborium::into_writer(&Cbor::Bytes(b"after confirming".to_vec()), &mut want).unwrap();
    let found = rt.block_on(async {
        tokio::time::timeout(Duration::from_secs(20), async {
            while let Some(op) = from_bridge.recv().await {
                if op.body.as_deref() == Some(&want[..]) {
                    return true;
                }
            }
            false
        })
        .await
        .unwrap_or(false)
    });
    assert!(found, "a live edit was not relayed to Logos");
}

/// API 2: an operation item carries the signed operation, and another node that never synced
/// with its author can import it unchanged (same hash), as reflection_core does with operations
/// from Logos Delivery.
#[test]
fn imported_operation_is_the_same_operation() {
    let a = device("import-a", Arc::new(Mutex::new(Vec::new())));
    let c = device("import-c", Arc::new(Mutex::new(Vec::new())));
    let topic = random32();
    // Different networks: a and c never sync with each other.
    let (net_a, net_c) = (random32(), random32());
    a.network_open("test", &net_a, &key()).unwrap();
    c.network_open("test", &net_c, &key()).unwrap();
    let sub_a = a.open("test", &net_a, &topic, "start", "note").unwrap()["sub"]
        .as_str()
        .unwrap()
        .to_owned();
    let sub_c = c.open("test", &net_c, &topic, "start", "note").unwrap()["sub"]
        .as_str()
        .unwrap()
        .to_owned();

    a.publish(&sub_a, &message("carried over"), false).unwrap();
    let items = wait_for(&a, &sub_a, "carried over");
    let op = items
        .iter()
        .find(|i| i["kind"] == "operation" && text_of(i).as_deref() == Some("carried over"))
        .unwrap();
    let body = op["body"].as_str().unwrap_or("");
    let reply = c
        .import(&sub_c, op["header"].as_str().unwrap(), body)
        .unwrap();
    assert_eq!(reply["hash"], op["hash"]);

    let got = wait_for(&c, &sub_c, "carried over");
    let imported = got
        .iter()
        .find(|i| i["kind"] == "operation" && text_of(i).as_deref() == Some("carried over"))
        .unwrap();
    assert_eq!(imported["hash"], op["hash"]);
    assert_eq!(imported["author"], op["author"]);
    assert_eq!(imported["source"], "logos");
    assert_eq!(imported["header"], op["header"]);

    // Garbage is refused.
    assert!(c.import(&sub_c, "AAAA", "").is_err());
}
