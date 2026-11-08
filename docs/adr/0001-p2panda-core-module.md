# ADR 0001: `p2panda_core`, a shared p2panda node for Basecamp apps

Status: proposed (2026-11-08). Owner decisions recorded, design review (laptop/reviewer,
2026-11-08) taken in; see "Review" at the end for what changed.

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
one `p2panda::Node` per network id in use, created on the first `open` for it, with the same iroh
configuration (relay, bootstrap, mDNS) but **its own iroh node key and its own SQLite store** per
network id, under the module's data directory. Separate node keys stop p2panda peers from linking
one device across unrelated apps. Apps never see iroh or the store.

### 2. Identity: apps control their keys

The module never creates, stores or chooses a signing key. Two ways to write:

- **App-signed** (`import`): the app builds and signs the p2panda operation itself and hands the
  module the header and body. The module validates and ingests it (`StreamPublisher::import`) and
  syncs it. For apps that link `p2panda-core`.
- **Signed through a session** (`publish`, `prune`, `ephemeral`): the app hands its key over once,
  `signer_open(app, key)`, and gets back an unguessable 128-bit handle; writes then carry the
  handle, and the module builds the operation (log, seq, backlink, prune flag) and signs it. The
  key lives only in the module's memory (zeroized when dropped), is never written anywhere, and is
  dropped on `signer_close`, when the app closes its last topic, and on module restart (apps open
  a new session after `restarted`). The handle is a return value, which reaches only the caller,
  unlike events, so other modules can't learn it. For apps that don't want p2panda's encoding in
  their own code.

Compared with module-held keys under an unauthenticated `app` label, where any module could
`publish(app="reflection")` and forge the user's edits, a hostile module here can't sign as the
user without the key or the handle. Compared with passing the key on every write, the key crosses
IPC once per session instead of with every keystroke.

Apps should give the module as little as possible: a key derived per document from their root
key, rather than the root key that is the user's identity everywhere, so a leaked key affects one
document. (For Reflection this is a decision of its own: the GTK app uses one identity across
documents, and presence shows it.)

`import` is the recommended path: no key crosses IPC. The signer session is the convenience path,
because a key that crosses IPC passes through QString/QVariant buffers in both processes (which
can't be zeroized), the Qt Remote Objects socket, `logos_host` and the generated LIDL glue, and is
visible to `logosctl call/watch` and crash dumps. Rules: key and body arguments are never logged
at any level (the generated glue and `logos_host` trace paths are audited, not just this module),
errors never echo arguments, Rust-side copies are zeroized, and a write that names an author is
refused if the session's key belongs to someone else (to catch app bugs, not as security).

For `import`, the app needs to know where its log stands: `head` gives the latest operation of an
author's log as the module has it (after a restart, or when the same key also wrote through
`publish` or from another device). The module serializes writes per `(network, topic, author)`,
so a `publish` and an `import` with one key can't fork a log; an `import` that doesn't fit the
head is refused with the head in the error.

Where an app keeps its key, and whether it is one key per device, per network or per document, is
the app's decision. `app` stays a label (for status, stats and the bridge view), not a credential.

### 3. API

All methods return `{"ok": true, ...}` or `{"ok": false, "error": "..."}` and return at once;
network and store work happens on the module's runtime, never on the Qt thread. Binary values are
standard base64. `network` is the 32-byte network id (hex), `topic` the 32-byte topic id (hex).

| Method | Does |
|---|---|
| `open(app, network, topic, from, label)` | Subscribe to a topic; returns a **subscription handle** (unguessable, 128-bit). `from`: `"start"`, `"live"`, or a **cursor** from an earlier `drain` to resume after it. `app` and `label` are what the p2panda view shows (e.g. "Reflection", the note's name). |
| `close(sub)` | End the subscription; clears its `set_active` lease. The node leaves the topic when no subscription has it. |
| `forget(app, network, topic)` | The user deleted the document: close it, delete its stored operations and its bridge state. Refused while another app has it open. |
| `head(network, topic, author)` | The latest operation (seq, hash) of an author's log, or none. |
| `heads(network, topic)` | The same for every author of the topic. |
| `signer_open(app, key)` | Start a signing session with this key (hex); returns a handle and the author's public key. |
| `signer_close(signer)` | End it; the key is dropped. |
| `publish(app, network, topic, signer, body_b64)` | Append an operation with this body to the session author's log, signed by the session. Returns the operation hash and seq. |
| `prune(app, network, topic, signer, body_b64)` | The same with the prune flag (a snapshot; earlier operations of that log may be deleted everywhere). |
| `import(app, network, topic, header_b64, body_b64)` | Ingest an operation the app signed. Refused if invalid or not for this topic's log. |
| `ephemeral(app, network, topic, signer, body_b64)` | Send an ephemeral message (presence, cursors), signed by the session. |
| `set_active(app, network, topic, active)` | Whether the user has the topic open (used by the bridge, section 4). A **lease**: it expires after 60 s unless renewed, so a crashed app can't leave a topic active. A topic is active while any app holds a lease on it. |
| `bridge(app, network, topic, on)` | `on`: **ask** to carry this topic over Logos Delivery as well; it stays `pending` until the user confirms it in the p2panda view (section 4). `off`: stop bridging, at once. |
| `status()` | Per network: node up, iroh relay/direct connections, peers per topic, sync sessions. |
| `stats()` | Operations sent/received per topic; for bridged topics, Logos messages by kind per RLN epoch (as `TransportStats` in the fork). |

Incoming data is not pushed as events (see section 6: events reach every module). It waits in a
per-app queue, which the app drains:

| Method | Does |
|---|---|
| `drain(sub, max)` | The next items of this subscription: at most `max` (≤ 256) and about 1 MiB, but **always at least one** if any, however large, so a big snapshot can't wedge it. Returns `{items, more, pending}`; the app loops while `more` is true. Items: `{"kind": "operation", author, seq, hash, prune, header_b64, body_b64, source, cursor}` (`source`: `"local"`, `"p2panda"`, `"logos"`), `{"kind": "ephemeral", author, timestamp, body_b64}`, `{"kind": "sync", remote, state}`. |
| `api_version()` | The API version, so an app can tell an older or newer module apart. |

Events carry no content:

| Event | Payload |
|---|---|
| `pending` | `sub_hint`: a short, non-secret prefix-free tag the subscriber got from `open`, so it knows which of its subscriptions to drain (sent when one goes from empty to non-empty, at most once a second while it stays so, and not at all for abandoned subscriptions, section 6). |
| `restarted` | `api_version`: the module started; apps `open` their topics again (`open` is idempotent). |

**A subscription is a cursor into the store, not a buffer.** Each subscription keeps a read
position over the module's store (per author log) and a small read-ahead window filled from it.
Replay (`from = "start"` or a cursor) is just reading from an earlier position; a full window
stops reading ahead and loses nothing. Within a subscription, live operations go before its own
replay (the topic the user is looking at stays live while history loads), and replays are filled
round-robin across subscriptions and topics. Kinds are treated differently: operations are never
dropped (they are in the store); ephemeral messages are lossy anyway, so they have their own small
bound and the oldest are dropped; for `sync` only the latest state per remote is kept.

**Delivery guarantee:** at least once, in order within each author's log; an app deduplicates by
`hash`. The `cursor` of an operation item (the topic's per-author frontier up to and including it)
is what the app stores to resume after a restart of either side or after its queue overflowed,
instead of replaying from the start. The module also persists each subscription's position, so
after `restarted` an app that re-opens with its last cursor continues without duplicates.

The practical payload size of a Basecamp IPC call (Qt Remote Objects) gets measured before the
drain limits are fixed; the numbers above are a starting point.

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
network status and the list of bridged topics, each with its app, label, spend and an on/off
switch. Because the module can't tell which app calls it, an app can't switch bridging on by
itself: `bridge(on)` from an app (for example from Reflection's per-note "Bridge to p2panda"
switch) only puts the topic in **pending**, and it is bridged once the user confirms it in the
p2panda view. Turning it off needs no confirmation. Nothing is bridged by default. Each app also
has a cap on how many topics it may have bridged (default 20).

What bridging exposes on the p2panda side (checked in p2panda 0.7 source, vpavlin/p2panda
dbbecdc): topic discovery is a private set intersection (`p2panda-discovery/src/psi_hash.rs`):
peers exchange topic ids hashed with a salt both sides contribute per session, so a peer learns
only the topics it already knows, nothing about the others. Gossip (iroh-gossip) uses the raw
topic id as its topic, but joins only nodes the address book lists for that topic, i.e. peers
that proved through discovery that they know it, and over encrypted QUIC connections; iroh relay
servers see ciphertext. So bridging does not leak the topic id (the document secret) to anyone
who doesn't already have it. What it does reveal: that this device takes part in p2panda, its
IP address to the peers it syncs with, and to those peers which of their topics it shares. The
bridge switch says so.

**Remote peers must not be able to spend the user's budget.** On Logos, anyone with the topic id
can send `heads`, and each one can make this device re-send logs (repairs), paid from the user's
membership; an old `heads` frame can also be replayed. So on bridged topics (and in the fork's
Logos backend generally) repairs are capped: at most one answered `heads` per `(topic, sender)`
per 30 s, at most 20 repair messages per topic per epoch, repairs sent as the snapshot plus what
follows rather than whole logs, and the rest counted as `repairs_blocked` in `stats`.

Operations over the Logos size limit (140 KiB) can't be bridged; `publish`/`import` on a bridged
topic says so in its reply, and the view and `stats` show "not bridged: too large".

**One budget for the device.** `reflection_core` (on Logos directly) and this bridge spend the same
`loam_core` membership. Counting per module would let each believe it has the whole budget, so the
budget (real, and the pretend one for trying out tiers) and the spend counter move into
`loam_core`, which every module asks before publishing. Until `loam_core` has that, the bridge
reads the pretend budget from its own settings and the view says it isn't shared.

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
  events: each subscription is drained through its own handle (section 3), which only its caller
  received (return values, unlike events, aren't broadcast). On p2panda, operation bodies are not
  encrypted per app (Reflection's are plain Loro updates), so broadcasting them would hand every
  app's content to every other module, and make every app decode everyone's traffic.
- **Fairness.** Each subscription's read-ahead is bounded (default 4096 items or 16 MiB), and so is
  the module's total (default 64 MiB): the bound is how far ahead the module reads, never a point
  where data is lost. Live operations are always stored and synced; delivery to a slow app just
  waits at its cursor. Replays are filled round-robin across apps and topics.
- **Abandoned subscriptions.** A subscription not drained for 10 minutes (the app crashed, or left
  without `close`) stops getting `pending`, releases its read-ahead and loses its `set_active`
  lease; it continues from its cursor if the app drains it again.
- **Failure isolation.** Every call is guarded (`catch_unwind`, typed JSON reads, size limits): a
  bad request or operation from one app fails that call only.
- **Several instances of one app** (a view and a core, two views) each get their own subscription
  handles, so they don't take each other's items.
- **Per-app limits:** topics open (default 200), bridged topics (20), queue size (above).
- **`pending` is broadcast** with a non-secret tag: other modules can see that some subscription has
  traffic, not what. Apps drain until `more` is false, not once per event.
- **Residual risk.** Anything that can read the user's process memory or the IPC socket gets a
  signing key in a session as it would in any design where another process signs. Only `import`
  (the app signs, possibly through the OS keyring) avoids that.
- **Shared node, separate subscriptions.** Apps on the same network id share its node and store;
  each app only receives topics it opened. An app that knows another app's topic id can open it,
  as anyone on the network can: topic ids are the secret.
- **Restarts and upgrades are for everyone.** The module announces `restarted`; apps open again.
  `api_version` lets an app refuse or adapt to a module it doesn't support.
- **Trust boundary.** All Basecamp modules run as the user. Subscription and signer handles reach
  only their caller, so another module can't drain a subscription or sign with a session it
  wasn't given; but `app` is still an unauthenticated label, and a module that knows a topic id
  can open its own subscription to it, as anyone on the network can. Apps that need more than
  "whoever has the topic id" encrypt their bodies.

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

1. **Keys: apps control them.** The module never stores or chooses a key (section 2): apps import
   operations they signed (recommended), or open a signing session (the key crosses IPC once and
   lives only in memory until the session ends). Review: a session handle instead of the key on
   every write.
2. **One module, one node per network id inside it** (section 1). Users see no difference; one
   module is simpler to run and to show status for.
3. **Replay and live data reach each app through its own queue** (sections 3 and 6). The owner
   first chose pushed events; since one module serves every app and Basecamp events reach every
   module, data goes into per-app queues the app drains, and events only say "you have items".
4. **The bridge is the user's**: a p2panda view lists bridged topics with on/off switches; apps
   may turn bridging on only from a user action in their own UI (section 4).
5. **Ship on the patched p2panda** (vpavlin/p2panda `live-push-imports`), pinned by commit, with
   `bridge_pushes_logos_edits_to_live_p2panda_peers` in CI as its guard and one explicit test
   against upstream GTK Reflection (the patch changes only local push behaviour, so the wire
   stays compatible). Keep offering the patch upstream.

## Plan

1. This ADR, reviewed.
2. `p2panda_core`: node per network id, `open/close/publish/prune/import/ephemeral`, events,
   `status`; two-device test over iroh with `logosctl`.
3. Bridge in the module (`bridge`, `set_active`, `stats`) and the p2panda view, tested against
   Basecamp Reflection on Logos and upstream GTK Reflection on p2panda.
4. `P2pandaBus` in the fork and `reflection_core` on it, with the sync-mode setting in the view.
5. linux-amd64 + linux-arm64 builds, lan03.

Tests, besides the functional ones: a `heads` flood or replay against a bridged topic stays within
the repair cap; an oversized operation on a bridged topic is reported, not silently p2panda-only;
two apps on one topic and network each get every operation; an app that crashes with a topic
active stops costing after its lease; a restart resumes from a cursor without applying anything
twice; a packet capture of discovery and gossip for one topic shows no plain topic id to peers
that don't share it.

## Review (laptop/reviewer, 2026-11-08)

Taken in: repair cap against remote budget draining; bridge requests confirmed in the p2panda
view; `head`/`heads` and per-author write serialization for `import`; cursors and an
at-least-once guarantee instead of full replays; subscriptions as cursors into the store with
subscription handles, `{items, more}` drains, live-before-replay, lossy ephemerals and abandoned
subscriptions (second review of section 6); keys by `(app, network, topic)`; the at-least-one
rule for `drain`; distinct labels per app instance; `forget`; `set_active` leases; `pending`
metadata stated; size check for bridged topics; no logging of arguments and key zeroizing;
`import` as the recommended path and signer sessions instead of per-call keys; per-document keys
recommended; the residual risk stated; an iroh key per network id; one device-wide budget in
`loam_core`; the patch pinned and guarded; the extra tests. The question whether discovery leaks
topic ids was answered from p2panda's source (section 4): it doesn't.
