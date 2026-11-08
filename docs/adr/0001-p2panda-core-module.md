# ADR 0001: `p2panda_core`, a shared p2panda node for Basecamp apps

Status: proposed (2026-11-08), for review. Owner decisions on the open questions recorded
2026-11-08 (section "Decisions on the open questions").

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

### 2. Identity: apps control their keys

The module never creates, stores or chooses a signing key. Two ways to write:

- **App-signed** (`import`): the app builds and signs the p2panda operation itself and hands the
  module the header and body. The module validates and ingests it (`StreamPublisher::import`) and
  syncs it. For apps that link `p2panda-core`.
- **Signed with the app's key** (`publish`, `prune`, `ephemeral`): the app passes its signing key
  (hex) with the call; the module builds the operation (log, seq, backlink, prune flag), signs it
  with that key, ingests it and forgets the key. For apps that don't want p2panda's encoding in
  their own code. The key crosses Basecamp IPC on the device, as the app's other data does.

Where an app keeps its key, and whether it is one key per device, per network or per document, is
the app's decision. `app` stays a label (for status, stats and the bridge view), not a credential.

### 3. API

All methods return `{"ok": true, ...}` or `{"ok": false, "error": "..."}` and return at once;
network and store work happens on the module's runtime, never on the Qt thread. Binary values are
standard base64. `network` is the 32-byte network id (hex), `topic` the 32-byte topic id (hex).

| Method | Does |
|---|---|
| `open(app, network, topic, from, label)` | Subscribe to a topic. `from`: `"start"` replays every stored operation as events first, then live; `"live"` only new ones. Idempotent per `(app, topic)`. `label` is what the p2panda view shows for the topic (e.g. the note's name); it can be changed with another `open`. |
| `close(app, network, topic)` | Stop delivering events for it to this app (the node leaves the topic when no app has it open). |
| `publish(app, network, topic, key, body_b64)` | Append an operation with this body to the log of `key`'s author, signed with `key` (not kept). Returns the operation hash and seq. |
| `prune(app, network, topic, key, body_b64)` | The same with the prune flag (a snapshot; earlier operations of that log may be deleted everywhere). |
| `import(app, network, topic, header_b64, body_b64)` | Ingest an operation the app signed. Refused if invalid or not for this topic's log. |
| `ephemeral(app, network, topic, key, body_b64)` | Send an ephemeral message (presence, cursors), signed with `key` (not kept). |
| `set_active(app, network, topic, active)` | Whether the user has the topic open (used by the bridge, section 4). |
| `bridge(app, network, topic, on)` | Carry this topic over Logos Delivery as well (section 4). Only in response to a user action in the app; the user sees and can undo it in the p2panda view. |
| `status()` | Per network: node up, iroh relay/direct connections, peers per topic, sync sessions. |
| `stats()` | Operations sent/received per topic; for bridged topics, Logos messages by kind per RLN epoch (as `TransportStats` in the fork). |

Incoming data is not pushed as events (see section 6: events reach every module). It waits in a
per-app queue, which the app drains:

| Method | Does |
|---|---|
| `drain(app, max)` | The oldest `max` items (at most 256, or 1 MiB) of this app's queue, removed from it. Items: `{"kind": "operation", network, topic, author, seq, hash, prune, header_b64, body_b64, source}` (`source`: `"local"`, `"p2panda"`, `"logos"`), `{"kind": "ephemeral", network, topic, author, timestamp, body_b64}`, `{"kind": "sync", network, topic, remote, state}`. |
| `api_version()` | The API version, so an app can tell an older or newer module apart. |

Events carry no content:

| Event | Payload |
|---|---|
| `pending` | `app, count`: this app's queue has items (sent when it goes from empty to non-empty, and at most once a second while it stays so). |
| `restarted` | `api_version`: the module started; apps `open` their topics again (`open` is idempotent). |

A replay (`open` with `from = "start"`) fills the app's queue in order, batch by batch, as the app
drains it.

### 4. The bridge lives in `p2panda_core`

`bridge(network, topic, true)` makes the module also subscribe the topic over Logos Delivery,
using `loam_core` as its `DeliveryBus` and the fork's Logos protocol (content topics and key
derived from network id and topic, signed p2panda operations as the envelope, catch-up by heads).
The bridge logic is the fork's `BridgeBackend`, unchanged:

- operations the node processes are relayed to Logos, collected per author for 15 s and sent as
  the newest snapshot plus what follows it (fork ADR 0008);
- operations from Logos are imported into p2panda and synced on to iroh peers;
- ephemeral messages are not bridged (they are signed per network).

Bridging spends the device's Logos (RLN) budget, so the user is in charge of it. `p2panda_core`
gets a small view of its own, a "p2panda" app in Basecamp (as Loam has for Logos), with the
network status and the list of bridged topics, each with its app, label and an on/off switch. An
app may switch a topic on (`bridge`) only as the direct result of a user action in its own UI,
such as Reflection's per-note "Bridge to p2panda" switch; the p2panda view always shows it and
can turn it off. Nothing is bridged by default.

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

### 6. One module for every app

Basecamp runs one `p2panda_core` and every p2panda app on the device uses it. So:

- **Isolation.** Basecamp module events reach every module that listens, so data never travels in
  events: each app drains its own queue (section 3). On p2panda, operation bodies are not
  encrypted per app (Reflection's are plain Loro updates), so broadcasting them would hand every
  app's content to every other module, and make every app decode everyone's traffic.
- **Fairness.** Each app's queue is bounded (default 4096 items or 16 MiB). A full queue pauses
  that app's replay and its live delivery for that app (they resume as it drains; live
  operations are still stored and synced, the app reads them later from the store), and nothing
  else. Replays of different apps are filled round-robin.
- **Failure isolation.** Every call is guarded (`catch_unwind`, typed JSON reads, size limits): a
  bad request or operation from one app fails that call only.
- **Shared node, separate subscriptions.** Apps on the same network id share its node and store;
  each app only receives topics it opened. An app that knows another app's topic id can open it,
  as anyone on the network can: topic ids are the secret.
- **Restarts and upgrades are for everyone.** The module announces `restarted`; apps open again.
  `api_version` lets an app refuse or adapt to a module it doesn't support.
- **Trust boundary.** All Basecamp modules run as the user, and `app` is a label the caller
  passes, so a hostile module on the device could drain another app's queue. Per-app queues stop
  accidental leaks and cross-traffic, not a malicious module on the user's own machine; apps
  that need more encrypt their bodies.

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

## Decisions on the open questions (owner, 2026-11-08)

1. **Keys: apps control them.** No module-held keys (section 2): apps import operations they
   signed, or pass their key with each write and the module forgets it.
2. **One module, one node per network id inside it** (section 1). Users see no difference; one
   module is simpler to run and to show status for.
3. **Replay and live data reach each app through its own queue** (sections 3 and 6). The owner
   first chose pushed events; since one module serves every app and Basecamp events reach every
   module, data goes into per-app queues the app drains, and events only say "you have items".
4. **The bridge is the user's**: a p2panda view lists bridged topics with on/off switches; apps
   may turn bridging on only from a user action in their own UI (section 4).
5. **Ship on the patched p2panda** (vpavlin/p2panda `live-push-imports`), and keep offering the
   patch upstream.

## Plan

1. This ADR, reviewed.
2. `p2panda_core`: node per network id, `open/close/publish/prune/import/ephemeral`, events,
   `status`; two-device test over iroh with `logosctl`.
3. Bridge in the module (`bridge`, `set_active`, `stats`) and the p2panda view, tested against
   Basecamp Reflection on Logos and upstream GTK Reflection on p2panda.
4. `P2pandaBus` in the fork and `reflection_core` on it, with the sync-mode setting in the view.
5. linux-amd64 + linux-arm64 builds, lan03.
