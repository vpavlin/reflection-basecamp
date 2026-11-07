# Reflection for Logos Basecamp

Collaborative notes in [Logos Basecamp](https://github.com/logos-co): the
[Reflection](https://github.com/p2panda/reflection) editor's engine, syncing over Logos Delivery.

Share a note's id with someone; whoever has it can read and edit the note with you. Edits merge
(Loro CRDT), and a person who joins late catches up from whoever is online.

| Module | What |
|---|---|
| `reflection_core` | Core module, written in Rust (`interface: cdylib`). Reflection's `reflection-node` with its Logos Delivery backend ([fork](https://github.com/vpavlin/reflection/tree/feat/logos-delivery), [protocol](https://github.com/vpavlin/reflection/blob/feat/logos-delivery/docs/logos/spec.md)), plus a glib-free re-implementation of Reflection's document layer. Sends and receives through `loam_core`, the shared Logos node of the host. |
| `reflection` | The view: pure QML, polls `reflection_core`. |

Needs `loam_core` (0.7.x) and its `delivery_module` in the same Basecamp.

## Interop with upstream Reflection (p2panda)

Notes are the same p2panda documents on both networks. A **bridge** node joins the two: a
Reflection built from the [fork](https://github.com/vpavlin/reflection/tree/feat/logos-delivery)
with `--features logos-ffi` and started with

```sh
REFLECTION_BACKEND=bridge REFLECTION_LOGOS_LIB=/path/to/liblogosdelivery.so ...
```

is on p2panda (like upstream Reflection) and on the Logos test network (like this app) at once,
and relays the notes it has open in both directions
([ADR 0007](https://github.com/vpavlin/reflection/blob/feat/logos-delivery/docs/logos/adr/0007-bridge.md)).
Tested headless on the real networks: Basecamp <-> bridge <-> p2panda, live edits in 1-2 s.
`REFLECTION_BACKEND=logos` instead makes the GTK app a plain Logos peer of this app.

## Building

```sh
cd reflection_core && nix build .#lgx-portable   # the Rust crate is compiled by the builder
cd reflection && nix build .#lgx-portable
```

Engine tests (no Logos runtime needed, in-memory bus):

```sh
cd reflection_core
nix build github:logos-co/logos-module-builder/16e2f6bd3c06a5119f8c34448933de183205646c#rust-sdk-src -o logos-rust-sdk-src
cd rust-lib && cargo test
```

## License

GPL-3.0-or-later, like Reflection.
