//! p2panda for Logos Basecamp: one shared p2panda node per app network on the device, and an
//! opt-in bridge of single topics to Logos Delivery (docs/adr/0001-p2panda-core-module.md).
//!
//! Every method returns a JSON object, `{"ok": true, ...}` or `{"ok": false, "error": "..."}`,
//! and returns at once. Messages are base64 of their CBOR encoding, as p2panda puts them on the
//! wire. Incoming data never travels in events (events reach every module): each subscription is
//! drained by whoever opened it; the `pending` event only says which one has items.

pub mod engine;

/// The module's methods.
pub trait P2pandaCoreModule: Send + 'static {
    /// The API version (an app can refuse a module it doesn't support).
    fn api_version(&mut self) -> String;

    /// Start the p2panda node of a network (`network`: 64 hex), signing with `key` (64 hex, the
    /// app's ed25519 secret key, kept in memory only). One key per network.
    fn network_open(&mut self, app: String, network: String, key: String) -> String;

    /// Subscribe to a topic of an open network. `from`: "start" or "frontier". Returns a
    /// subscription handle (secret: it reaches only the caller) and a public `tag` used in
    /// `pending` events. `label` names the topic in the p2panda view.
    fn open(
        &mut self,
        app: String,
        network: String,
        topic: String,
        from: String,
        label: String,
    ) -> String;

    /// End a subscription.
    fn close(&mut self, sub: String) -> String;

    /// The next items of a subscription: `{items, more, pending}`. Loop while `more`.
    fn drain(&mut self, sub: String, max: String) -> String;

    /// Append a message (base64 of its CBOR) to the topic, signed with the network's key.
    /// `prune` "true": a snapshot; earlier operations of the log may be deleted everywhere.
    fn publish(&mut self, sub: String, message: String, prune: String) -> String;

    /// Import a signed operation from another network (base64 of its encoded header and body,
    /// body "" for none) into the subscription's topic. API 2.
    fn import(&mut self, sub: String, header: String, body: String) -> String;

    /// Send an ephemeral message (base64 of its CBOR): presence, cursors.
    fn ephemeral(&mut self, sub: String, message: String) -> String;

    /// The user has the topic open ("true"), or not. A 60 s lease: renew it.
    fn set_active(&mut self, sub: String, active: String) -> String;

    /// Ask to carry the topic over Logos Delivery as well ("true": pending until the user
    /// confirms it in the p2panda view), or stop ("false": at once).
    fn bridge(&mut self, sub: String, on: String) -> String;

    /// The user confirms, refuses or stops bridging a topic (the p2panda view).
    fn bridge_confirm(&mut self, network: String, topic: String, on: String) -> String;

    /// Bridged topics and requests.
    fn bridges(&mut self) -> String;

    /// Networks, subscriptions, the Logos side of bridges.
    fn status(&mut self) -> String;

    /// Called once the module is wired to the host.
    #[cfg(not(test))]
    fn on_context_ready(&mut self, _ctx: &RustModuleContext) {}
}

/// The module's events.
pub trait P2pandaCoreModuleEvents {
    /// The subscription with this tag has items: drain it.
    fn pending(&self, tag: String);
    /// The module started: open your networks and topics again.
    fn restarted(&self, api_version: u64);
}

#[cfg(not(test))]
include!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/generated/provider_gen.rs"
));

#[cfg(not(test))]
mod loam;
#[cfg(not(test))]
mod module;

#[cfg(not(test))]
#[no_mangle]
pub extern "Rust" fn logos_module_install() {
    install::<module::Core>();
}

#[cfg(test)]
mod tests;
