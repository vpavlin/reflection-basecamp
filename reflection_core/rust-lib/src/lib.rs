//! Reflection for Logos Basecamp: collaborative notes that sync over Logos Delivery.
//!
//! The engine is p2panda's Reflection (`reflection-node`, Logos backend) with the document
//! layer of the GTK app re-done without glib (see `engine`). Transport is loam_core, the
//! host's shared Logos node (see `loam`).
//!
//! The trait below is the module's contract: the builder derives the `.lidl` from it. Every
//! method returns a JSON object, `{"ok": true, ...}` or `{"ok": false, "error": "..."}`.

mod engine;
mod loam;

/// The methods the Reflection view calls.
pub trait ReflectionCoreModule: Send + 'static {
    /// The list of notes, transport status, and the open note when `doc_id` is not empty.
    fn state(&mut self, doc_id: String) -> String;

    /// A new note whose first line is `title`. Returns its id and content.
    fn create_document(&mut self, title: String) -> String;

    /// Open a note someone shared, by its id (64 hex characters).
    fn join_document(&mut self, doc_id: String) -> String;

    /// The user's text for a note, edited starting from `base_version` (from `state`).
    fn set_text(&mut self, doc_id: String, base_version: String, text: String) -> String;

    /// Stop syncing a note and forget it on this device.
    fn remove_document(&mut self, doc_id: String) -> String;

    /// Debug: messages this device sent and received over Logos Delivery, per RLN epoch and per
    /// note, with the budget the engine plans for.
    fn stats(&mut self) -> String;

    /// Called once the module is wired to the host.
    #[cfg(not(test))]
    fn on_context_ready(&mut self, _ctx: &RustModuleContext) {}
}

#[cfg(not(test))]
include!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/generated/provider_gen.rs"
));

#[cfg(not(test))]
mod module;

#[cfg(not(test))]
#[no_mangle]
pub extern "Rust" fn logos_module_install() {
    install::<module::Core>();
}
