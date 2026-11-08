# ADR 0001: `p2panda_core`, a shared p2panda node for Basecamp apps

Status: proposed (2026-11-08), for review

## Context

Reflection in Basecamp (`reflection_core`) syncs over Logos Delivery through `loam_core`, the
device's shared Logos node. Upstream Reflection (GTK) syncs over p2panda (iroh). The fork of
`reflection-node` can bridge the two networks (vpavlin/reflection ADR 0007), but today the bridge
only runs inside one process that embeds both stacks: the headless example or the GTK app with
`REFLECTION_BACKEND=bridge`.

Putting the bridge into `reflection_core` would help Reflection only. A Basecamp module that owns
a p2panda node, the way `loam_core` owns the Logos node, would let any p2panda-based app run in
Basecamp, and would be the natural home of the bridge: the Logos protocol of the fork (spec
`docs/logos/spec.md`) carries signed p2panda operations, encrypted per topic, and depends only on
the network id and the topic, not on what the operations contain.

Facts this design rests on:

- p2panda 0.7 (`p2panda::Node`) gives per-topic eventually consistent streams
  (`stream_from::<T>(topic, StreamFrom::Start | ...)` returning a `StreamPublisher` with
  `publish`, `prune` and `import`, plus `StreamEvent`s: `Processed`, `SyncStarted/Ended`, failures)
  and ephemeral streams (`ephemeral_stream::<T>(topic)`). One node has one `NetworkId`. Nodes
  find each other through iroh (relay `euc1-1.relay.n0.iroh.link`, a bootstrap node, mDNS).
- `StreamPublisher::import` ingests operations signed elsewhere. Upstream does not push imported
  operations to live sync sessions; vpavlin/p2panda `live-push-imports` (dbbecdc) does, and the
  bridge needs it (fork ADR 0007).
- p2panda 0.7.1 changed its API in a patch release. Apps that link p2panda directly break with it.
- Basecamp modules talk over Logos IPC: methods (LIDL) and events. A module does not reliably know
  which module called it. Every call runs on the module's Qt thread; a module must not block it
  (`reflection_core` runs all IPC on its own thread for this reason).
- Logos Delivery is going to meter publishes (RLN). Receiving is free. The fork publishes nothing
  for topics nobody has open, merges writes, and lets the user set a pretend budget (fork ADR 0008).

## Decision

Build `p2panda_core`, a Basecamp core module (Rust cdylib, logos-module-builder 0.3.1,
`codegen.rust`, the same toolchain as `reflection_core`) that runs p2panda for every app on the
device, and can bridge any of its topics to Logos Delivery through `loam_core`.

### 1. One module, one node per network id

Each p2panda app has its own network id (Reflection: `blake3("reflection-v2")`). The module runs
one `p2panda::Node` per network id in use, created on the first `open` for it and sharing one
iroh configuration (relay, bootstrap, mDNS) and one SQLite store per network id under the
module's data directory. Apps never see iroh or the store.

### 2. Identity: apps sign, or the module signs for them

Two ways to write, both supported:

- **App-signed** (`import`): the app builds and signs the p2panda operation itself and hands the
  module the header and body. The module validates and ingests it (`StreamPublisher::import`) and
  syncs it. The module never holds the app's key. For apps that link `p2panda-core` (Rust).
- **Module-signed** (`publish`, `prune`): the module signs with a key it keeps per
  `(app, network id)`, created on first use and stored in the module's data directory. For QML or
  C++ apps.

`app` is a name the caller passes. Basecamp cannot authenticate callers, so any module on the
device could use any app's module-held key. That is the same trust as everything else in one
Basecamp (all modules run with the user's rights), and it is stated, not solved: an app that needs
more signs its own operations.

### 3. API

All methods return `{"ok": true, ...}` or `{"ok": false, "error": "..."}` and return at once;
network and store work happens on the module's runtime, never on the Qt thread. Binary values are
standard base64. `network` is the 32-byte network id (hex), `topic` the 32-byte topic id (hex).

| Method | Does |
|---|---|
| `open(app, network, topic, from)` | Subscribe to a topic. `from`: `"start"` replays every stored operation as events first, then live; `"live"` only new ones. Idempotent per `(app, topic)`. Returns this app's author key on that network. |
| `close(app, network, topic)` | Stop delivering events for it to this app (the node leaves the topic when no app has it open). |
| `publish(app, network, topic, body_b64)` | Append an operation with this body to the app's log, signed with its module-held key. Returns the operation hash and seq. |
| `prune(app, network, topic, body_b64)` | The same with the prune flag (a snapshot; earlier operations of that log may be deleted everywhere). |
| `import(app, network, topic, header_b64, body_b64)` | Ingest an operation the app signed. Refused if invalid or not for this topic's log. |
| `ephemeral(app, network, topic, body_b64)` | Send an ephemeral message (presence, cursors), signed with the module-held key. |
| `set_active(app, network, topic, active)` | Whether the user has the topic open (used by the bridge, section 4). |
| `bridge(network, topic, on)` | Carry this topic over Logos Delivery as well (section 4). |
| `status()` | Per network: node up, iroh relay/direct connections, peers per topic, sync sessions. |
| `stats()` | Operations sent/received per topic; for bridged topics, Logos messages by kind per RLN epoch (as `TransportStats` in the fork). |

Events (one module event each, payload JSON):

| Event | Payload |
|---|---|
| `operation` | `app, network, topic, author, seq, hash, prune, header_b64, body_b64, source` (`"local"`, `"p2panda"`, `"logos"`) |
| `ephemeral` | `app, network, topic, author, timestamp, body_b64` |
| `sync` | `network, topic, remote, state` (`started` / `ended` / `failed`, with error) |

Replays over IPC go in order and in batches of at most 256 events or 1 MiB, so opening a long
topic does not flood the host.

### 4. The bridge lives in `p2panda_core`

`bridge(network, topic, true)` makes the module also subscribe the topic over Logos Delivery,
using `loam_core` as its `DeliveryBus` and the fork's Logos protocol (content topics and key
derived from network id and topic, signed p2panda operations as the envelope, catch-up by heads).
The bridge logic is the fork's `BridgeBackend`, unchanged:

- operations the node processes are relayed to Logos, collected per author for 15 s and sent as
  the newest snapshot plus what follows it (fork ADR 0008);
- operations from Logos are imported into p2panda and synced on to iroh peers;
- ephemeral messages are not bridged (they are signed per network).

The RLN rules of the fork apply to the Logos side: nothing is published for a bridged topic no app
has active (`set_active`), except answers to other peers' catch-up requests, and the pretend
budget is honoured. Bridging is opt-in per topic; the module never bridges on its own.

Because the protocol depends only on the network id and the topic, any p2panda app gets Logos
reach this way, without knowing Logos exists.

### 5. `reflection_core` on top

`reflection-node` gets a backend that talks to an external p2panda node through a small trait
(`P2pandaBus`: open, publish, prune, import, ephemeral, events), the way `DeliveryBus` abstracts
Logos. `reflection_core` implements it over IPC to `p2panda_core`. Reflection in Basecamp then
syncs over one of:

- Logos Delivery only (today, through `loam_core`);
- p2panda only (through `p2panda_core`);
- both, bridged (through `p2panda_core` with `bridge` on).

The choice is a setting in the view, per device first. Switching restarts the engine's node; the
documents stay (their operations are re-imported on the other side on first open).

## Alternatives considered

- **Bridge inside `reflection_core`** (`Node::with_bridge` on `loam_core`'s bus). Smallest change,
  and `reflection_core` already links p2panda. Helps Reflection only, and every other p2panda app
  would carry its own p2panda node and its own bridge. Kept as the fallback if this is rejected.
- **Each p2panda app links p2panda itself.** Several iroh endpoints and stores per device, and
  every app breaks with the next p2panda API change.
- **A bridge module without p2panda inside** (Logos to an external p2panda node). There is no
  shared p2panda node on the device to point it at, so this is this ADR minus its useful half.

## Consequences

- A second shared transport next to `loam_core`, with its own lifecycle, `status` and `stats`.
- p2panda version changes are absorbed in one module; apps see a stable IPC API.
- Operations cross a process boundary as base64 JSON: fine for text, a cost for large snapshots
  (the same reason the snapshot design is moving to Logos Storage).
- The module sees every topic id of every p2panda app on the device. Topic ids are secrets in
  Reflection (whoever has one can read and write); they stay on the device, as with `loam_core`.
- Using iroh announces topic interest to p2panda peers (not content). Apps that must not do that
  stay on Logos only.
- Mobile is out of scope: Basecamp modules are desktop.

## Open questions for review

1. Module-held keys keyed by a caller-supplied `app` name: acceptable, or should module-signed
   writes require a per-app secret handed out on first `open`?
2. One node per network id inside one module, or one module instance per network id?
3. Is `open(from="start")` replay over events the right shape, or should apps pull pages
   (`read(topic, after_seq, limit)`) instead?
4. Should the bridge be controlled by apps (`bridge` method) or only by the user (a setting in a
   `p2panda_core` view), since it spends the device's RLN budget?
5. The bridge needs patched p2panda (`live-push-imports`). Ship the module on the patch, or wait
   for it upstream?

## Plan

1. This ADR, reviewed.
2. `p2panda_core`: node per network id, `open/close/publish/prune/import/ephemeral`, events,
   `status`; two-device test over iroh with `logosctl`.
3. Bridge in the module (`bridge`, `set_active`, `stats`), tested against Basecamp Reflection on
   Logos and upstream GTK Reflection on p2panda.
4. `P2pandaBus` in the fork and `reflection_core` on it, with the sync-mode setting in the view.
5. linux-amd64 + linux-arm64 builds, lan03.
