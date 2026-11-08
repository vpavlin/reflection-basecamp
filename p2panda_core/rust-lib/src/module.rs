//! The module: the contract implemented on the engine, and loam_core wired in for bridges.
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tracing::{error, info, warn};

use crate::engine::{Emit, Engine, Event, Options, API_VERSION};
use crate::loam::{self, LoamApi, LoamBus};
use crate::{emit_pending, emit_restarted, loam_core, modules, P2pandaCoreModule, RustModuleContext};

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

fn flag(s: &str) -> bool {
    matches!(s.trim().trim_matches('"'), "true" | "1" | "on" | "yes")
}

impl Core {
    fn with_engine(&self, f: impl FnOnce(&Engine) -> Result<Value, String>) -> String {
        let Some(engine) = &self.engine else {
            return reply(Err("p2panda_core is still starting".into()));
        };
        match catch_unwind(AssertUnwindSafe(|| f(engine))) {
            Ok(result) => reply(result),
            Err(_) => reply(Err("internal error in p2panda_core (see the log)".into())),
        }
    }
}

// Arguments (keys, messages, topic ids) are never logged: they are secrets or content.
impl P2pandaCoreModule for Core {
    fn api_version(&mut self) -> String {
        reply(Ok(json!({ "ok": true, "apiVersion": API_VERSION })))
    }

    fn network_open(&mut self, app: String, network: String, key: String) -> String {
        self.with_engine(|e| e.network_open(&app, &network, &key))
    }

    fn open(&mut self, app: String, network: String, topic: String, from: String, label: String) -> String {
        self.with_engine(|e| e.open(&app, &network, &topic, &from, &label))
    }

    fn close(&mut self, sub: String) -> String {
        self.with_engine(|e| e.close(&sub))
    }

    fn drain(&mut self, sub: String, max: String) -> String {
        let max = max.trim().trim_matches('"').parse::<usize>().unwrap_or(256);
        self.with_engine(|e| e.drain(&sub, max))
    }

    fn publish(&mut self, sub: String, message: String, prune: String) -> String {
        self.with_engine(|e| e.publish(&sub, &message, flag(&prune)))
    }

    fn ephemeral(&mut self, sub: String, message: String) -> String {
        self.with_engine(|e| e.ephemeral(&sub, &message))
    }

    fn set_active(&mut self, sub: String, active: String) -> String {
        self.with_engine(|e| e.set_active(&sub, flag(&active)))
    }

    fn bridge(&mut self, sub: String, on: String) -> String {
        self.with_engine(|e| e.bridge(&sub, flag(&on)))
    }

    fn bridge_confirm(&mut self, network: String, topic: String, on: String) -> String {
        self.with_engine(|e| e.bridge_confirm(&network, &topic, flag(&on)))
    }

    fn bridges(&mut self) -> String {
        self.with_engine(|e| Ok(e.bridges()))
    }

    fn status(&mut self) -> String {
        self.with_engine(|e| Ok(e.status()))
    }

    fn on_context_ready(&mut self, ctx: &RustModuleContext) {
        init_logging();
        let persist = if ctx.instance_persistence_path.is_empty() {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(std::env::temp_dir)
                .join(".p2panda-basecamp")
        } else {
            PathBuf::from(&ctx.instance_persistence_path)
        };
        info!(
            "p2panda_core {} starting, data in {}",
            env!("CARGO_PKG_VERSION"),
            persist.display()
        );

        let emit: Emit = Arc::new(|event| match event {
            Event::Pending(tag) => emit_pending(&tag),
        });
        let (bus, commands) = LoamBus::new();
        let engine = Arc::new(Engine::start(persist, bus.clone(), Options::default(), emit));
        self.engine = Some(engine);

        let spawned = std::thread::Builder::new()
            .name("p2panda-loam".into())
            .spawn(move || {
                std::thread::sleep(STARTUP_DELAY);
                // Apps that were using the module before it (re)started open their topics again.
                emit_restarted(API_VERSION);
                spawn_receiver(bus);
                loam::run(
                    GeneratedLoam {
                        client: modules().loam_core,
                    },
                    commands,
                    |_transport| {},
                );
            });
        if let Err(error) = spawned {
            error!("can't start the transport thread: {error}");
        }
    }
}

/// Forward loam_core's `received` events to the bus, resubscribing if the stream ends.
fn spawn_receiver(bus: Arc<LoamBus>) {
    let spawned = std::thread::Builder::new()
        .name("p2panda-rx".into())
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

fn check(result: Result<String, logos_rust_sdk::LogosError>) -> Result<String, String> {
    result.map_err(|error| format!("{error:?}"))
}

/// loam_core methods return "" on success and an error sentence otherwise.
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
    let filter = tracing_subscriber::EnvFilter::try_from_env("P2PANDA_CORE_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("p2panda_core=info,logos_p2panda=info,p2panda=warn"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();
}
