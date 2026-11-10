//! The module: the contract implemented on the engine, and loam_core wired in as transport.
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tracing::{error, info, warn};

use crate::engine::Engine;
use crate::loam::{self, LoamApi, LoamBus};
use crate::panda::{self, PandaApi, PandaBus, APP};
use crate::{loam_core, modules, p2panda_core, ReflectionCoreModule, RustModuleContext};

/// Module calls made while the host is still setting the module up are refused.
const STARTUP_DELAY: Duration = Duration::from_millis(1500);

#[derive(Default)]
pub struct Core {
    engine: Option<Arc<Engine>>,
}

fn reply(result: Result<Value, String>) -> String {
    match result {
        Ok(value) => value.to_string(),
        Err(error) => json!({ "ok": false, "error": error }).to_string(),
    }
}

impl Core {
    fn with_engine(&self, f: impl FnOnce(&Engine) -> Result<Value, String>) -> String {
        let Some(engine) = &self.engine else {
            return reply(Err("Reflection is still starting".into()));
        };
        match catch_unwind(AssertUnwindSafe(|| f(engine))) {
            Ok(result) => reply(result),
            Err(_) => reply(Err("Internal error in reflection_core (see the log)".into())),
        }
    }
}

impl ReflectionCoreModule for Core {
    fn state(&mut self, doc_id: String) -> String {
        self.with_engine(|e| Ok(e.state(&doc_id)))
    }

    fn create_document(&mut self, title: String) -> String {
        self.with_engine(|e| e.create(&title))
    }

    fn join_document(&mut self, doc_id: String) -> String {
        self.with_engine(|e| e.join(&doc_id))
    }

    fn set_text(&mut self, doc_id: String, base_version: String, text: String) -> String {
        self.with_engine(|e| e.set_text(&doc_id, &base_version, &text))
    }

    fn stats(&mut self) -> String {
        self.with_engine(|e| Ok(e.stats()))
    }

    fn set_cursor(
        &mut self,
        doc_id: String,
        base_version: String,
        position: String,
        anchor: String,
    ) -> String {
        let number = |s: &str| {
            s.trim()
                .trim_matches('"')
                .parse::<i64>()
                .map_err(|_| "Bad cursor position".to_owned())
        };
        self.with_engine(|e| {
            e.set_cursor(&doc_id, &base_version, number(&position)?, number(&anchor)?)
        })
    }

    fn settings(&mut self) -> String {
        self.with_engine(|e| Ok(e.settings()))
    }

    fn set_settings(&mut self, changes: String) -> String {
        self.with_engine(|e| e.set_settings(&changes))
    }

    fn bridge_note(&mut self, doc_id: String, on: String) -> String {
        let on = matches!(on.trim().trim_matches('"'), "true" | "1" | "on");
        self.with_engine(|e| e.bridge(&doc_id, on))
    }

    fn remove_document(&mut self, doc_id: String) -> String {
        self.with_engine(|e| e.remove(&doc_id))
    }

    fn on_context_ready(&mut self, ctx: &RustModuleContext) {
        init_logging();
        let persist = if ctx.instance_persistence_path.is_empty() {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(std::env::temp_dir)
                .join(".reflection-basecamp")
        } else {
            PathBuf::from(&ctx.instance_persistence_path)
        };
        info!(
            "reflection_core {} starting, data in {}",
            env!("CARGO_PKG_VERSION"),
            persist.display()
        );

        let (bus, commands) = LoamBus::new();
        let (panda_bus, panda_commands) = PandaBus::new();
        let engine = Arc::new(Engine::start(persist, bus.clone(), Some(panda_bus.clone())));
        spawn_panda(panda_bus, panda_commands);
        self.engine = Some(engine.clone());

        let weak_engine = Arc::downgrade(&engine);
        let spawned = std::thread::Builder::new()
            .name("reflection-loam".into())
            .spawn(move || {
                std::thread::sleep(STARTUP_DELAY);
                spawn_receiver(bus);
                loam::run(
                    GeneratedLoam {
                        client: modules().loam_core,
                    },
                    commands,
                    move |transport| {
                        if let Some(engine) = weak_engine.upgrade() {
                            engine.set_transport(transport);
                        }
                    },
                );
            });
        if let Err(error) = spawned {
            error!("can't start the transport thread: {error}");
        }
    }
}

/// The p2panda_core thread, and the listener of its events.
fn spawn_panda(bus: Arc<PandaBus>, commands: std::sync::mpsc::Receiver<panda::Command>) {
    let events = bus.sender();
    let status = bus.status.clone();
    let spawned = std::thread::Builder::new()
        .name("reflection-panda".into())
        .spawn(move || {
            std::thread::sleep(STARTUP_DELAY);
            panda::run(
                GeneratedPanda {
                    client: modules().p2panda_core,
                },
                commands,
                status,
            );
        });
    if let Err(error) = spawned {
        error!("can't start the p2panda thread: {error}");
    }
    let pending = events.clone();
    let spawned = std::thread::Builder::new()
        .name("reflection-panda-rx".into())
        .spawn(move || {
            std::thread::sleep(STARTUP_DELAY);
            loop {
                let mut client = modules().p2panda_core;
                match client.on_pending() {
                    Ok(subscription) => {
                        for event in subscription {
                            if let Some(e) = p2panda_core::P2pandaCoreClient::decode_pending(&event)
                            {
                                if pending.send(panda::Command::Pending(e.tag)).is_err() {
                                    return;
                                }
                            }
                        }
                    }
                    Err(error) => warn!("can't subscribe to p2panda_core events: {error:?}"),
                }
                std::thread::sleep(Duration::from_secs(2));
            }
        });
    if let Err(error) = spawned {
        error!("can't start the p2panda event thread: {error}");
    }
    let _ = std::thread::Builder::new()
        .name("reflection-panda-restart".into())
        .spawn(move || {
            std::thread::sleep(STARTUP_DELAY);
            loop {
                let mut client = modules().p2panda_core;
                if let Ok(subscription) = client.on_restarted() {
                    for _ in subscription {
                        if events.send(panda::Command::Restarted).is_err() {
                            return;
                        }
                    }
                }
                std::thread::sleep(Duration::from_secs(2));
            }
        });
}

struct GeneratedPanda {
    client: p2panda_core::P2pandaCoreClient,
}

/// p2panda_core replies with a JSON object, `{"ok": true, ...}` or `{"ok": false, "error"}`.
fn object(result: Result<String, logos_rust_sdk::LogosError>) -> Result<Value, String> {
    let raw = check(result)?;
    let value: Value = serde_json::from_str(raw.trim()).map_err(|e| e.to_string())?;
    if value["ok"] == true {
        Ok(value)
    } else {
        Err(value["error"]
            .as_str()
            .unwrap_or("p2panda_core error")
            .to_owned())
    }
}

impl PandaApi for GeneratedPanda {
    fn network_open(&mut self, network: &str, key: &str) -> Result<Value, String> {
        object(self.client.network_open(APP, network, key))
    }
    fn open(&mut self, network: &str, topic: &str, label: &str) -> Result<Value, String> {
        object(self.client.open(APP, network, topic, "start", label))
    }
    fn drain(&mut self, sub: &str) -> Result<Value, String> {
        object(self.client.drain(sub, "256"))
    }
    fn publish(&mut self, sub: &str, message_b64: &str, prune: bool) -> Result<Value, String> {
        object(
            self.client
                .publish(sub, message_b64, if prune { "true" } else { "false" }),
        )
    }
    fn ephemeral(&mut self, sub: &str, message_b64: &str) -> Result<Value, String> {
        object(self.client.ephemeral(sub, message_b64))
    }
    fn set_active(&mut self, sub: &str, active: bool) -> Result<Value, String> {
        object(
            self.client
                .set_active(sub, if active { "true" } else { "false" }),
        )
    }
    fn import(&mut self, sub: &str, header_b64: &str, body_b64: &str) -> Result<Value, String> {
        object(self.client.import(sub, header_b64, body_b64))
    }
    fn bridge(&mut self, sub: &str, on: bool) -> Result<Value, String> {
        object(self.client.bridge(sub, if on { "true" } else { "false" }))
    }
    fn bridges(&mut self) -> Result<Value, String> {
        object(self.client.bridges())
    }
}

/// Forward loam_core's `received` events to the bus, resubscribing if the stream ends.
fn spawn_receiver(bus: Arc<LoamBus>) {
    let spawned = std::thread::Builder::new()
        .name("reflection-rx".into())
        .spawn(move || loop {
            let mut client = modules().loam_core;
            match client.on_received() {
                Ok(subscription) => {
                    info!("listening to loam_core");
                    for event in subscription {
                        if let Some(received) = loam_core::LoamCoreClient::decode_received(&event) {
                            bus.deliver(received.topic, &received.payload_b64);
                        }
                    }
                    warn!("loam_core event stream ended, resubscribing");
                }
                Err(error) => warn!("can't subscribe to loam_core events: {error:?}"),
            }
            std::thread::sleep(Duration::from_secs(2));
        });
    if let Err(error) = spawned {
        error!("can't start the receive thread: {error}");
    }
}

struct GeneratedLoam {
    client: loam_core::LoamCoreClient,
}

/// loam_core methods return "" on success and an error sentence otherwise.
fn check(result: Result<String, logos_rust_sdk::LogosError>) -> Result<String, String> {
    result.map_err(|error| format!("{error:?}"))
}

fn empty_ok(result: Result<String, logos_rust_sdk::LogosError>) -> Result<(), String> {
    let value = check(result)?;
    let value = value.trim().trim_matches('"');
    if value.is_empty() {
        Ok(())
    } else {
        Err(value.to_owned())
    }
}

impl LoamApi for GeneratedLoam {
    fn start(&mut self) -> Result<(), String> {
        // The host's first app to start loam_core decides its configuration; "{}" asks for
        // loam_core's default (Logos test network, RLN off).
        check(self.client.start("{}")).map(|_| ())
    }

    fn status(&mut self) -> Result<String, String> {
        check(self.client.status())
    }

    fn metrics(&mut self) -> Result<String, String> {
        check(self.client.metrics_json())
    }

    fn join(&mut self, topic: &str) -> Result<(), String> {
        empty_ok(self.client.join(topic))
    }

    fn send(&mut self, topic: &str, payload_b64: &str) -> Result<(), String> {
        empty_ok(self.client.send_sealed(topic, payload_b64))
    }
}

fn init_logging() {
    let filter =
        tracing_subscriber::EnvFilter::try_from_env("REFLECTION_LOG").unwrap_or_else(|_| {
            tracing_subscriber::EnvFilter::new("reflection_core=info,reflection_node=info")
        });
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();
}
