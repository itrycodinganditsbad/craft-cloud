# Integrate the app kit

`craft-cloud-kit` makes an app's document model live through the local Craft Cloud agent.

The app keeps ownership of its file format, its model, its UI, and the mapping between the model and the CRDT. The agent owns the local durable journals, reconnecting, and the transport to the server.

Which crate to use depends on what the tool needs:

- `craft-cloud-app` is enough when a tool only needs agent status, sign-in, recents, team-space controls, or direct live bytes.
- Use the kit when you want to map the app's document model and integrate undo, save records, blobs, or presence.

```text
Craft app ── craft-cloud-app ── local HTTP/WebSockets ── Craft Cloud agent ── server
    │                                  │
    ├── craft-cloud-kit ───────────────┘   (LiveDocument, Session, mappings)
    └── craft-cloud-egui                    (optional eframe widgets)
```

See [the kit reference](kit.md) for the full API, and [the protocol reference](protocol-reference.md) for clients that are not written in Rust.

## Dependencies and layering

Link `craft-cloud-app` and `craft-cloud-kit`. Add `craft-cloud-egui` if the app uses eframe and wants the ready-made widgets.

The collaboration features described below need a release that includes them: `people()`, read-only documents for viewers, blobs bound to a document's space, and team spaces. The first release, `v0.1.0`, predates them. Replace `vX.Y.Z` in the example with the tag of a release that contains these features.

```toml
[dependencies]
craft-cloud-app = { git = "https://github.com/itrycodinganditsbad/craft-cloud", tag = "vX.Y.Z" }
craft-cloud-kit = { git = "https://github.com/itrycodinganditsbad/craft-cloud", tag = "vX.Y.Z" }
craft-cloud-egui = { git = "https://github.com/itrycodinganditsbad/craft-cloud", tag = "vX.Y.Z" }

[dev-dependencies]
craft-cloud-kit = { git = "https://github.com/itrycodinganditsbad/craft-cloud", tag = "vX.Y.Z", features = ["fake"] }
```

The app-facing crates depend on `craft-cloud-api`, never on `craft-cloud-proto`. Do not add `proto` to an app.

Run `cargo xtask suite-deps <the apps' Cargo.lock files>` to check that the dependencies are compatible with the other Craft apps.

The kit brings Loro and Loro's own dependency set. Loro requires these named exceptions to the license policy:

- MPL-2.0 for `im`, `bitmaps` and `sized-chunks`, used as published and unmodified.
- BSL-1.0 for `xxhash-rust`.

Read `AGENTS.md` and run `cargo xtask licenses` before you change dependencies.

## Map the model

Implement `LiveDocument` for a zero-sized marker type of the app. The trait asks for the following:

- `APP` and `SCHEMA`. `SCHEMA` is a version number of the layout the mappings write; it only ever increases.
- `mappings`, which exposes the pieces of the model through `Mappings`.
- `apply_remote`, which rebuilds the touched pieces after a remote change.
- `normalise`, which repairs state that a merge made invalid. The repair must be deterministic.
- `refresh_derived`, which recomputes derived state.

This excerpt is from the demo, `apps/craft-cloud-kit-demo/src/model.rs`:

```rust
impl LiveDocument for DemoApp {
    type Doc = DemoDoc;
    const APP: &'static str = "kitdemo";
    const SCHEMA: u32 = 1;

    fn mappings(doc: &DemoDoc) -> Mappings<'_> {
        Mappings::new().tree("layers", LayersSrc::new(&doc.layers))
            .text("story", StorySrc(&doc.story))
    }
}
```

Choose the source type by the shape of the data:

- `TreeSource` for hierarchical objects with stable IDs.
- `TextSource` and `RichText` for text, runs, and paragraph attributes.
- `TableSource` for rows, columns, and cells.
- `TimelineSource` for tracks, items, and keyframes.
- `OpaqueSource` with `OpaqueCodec` for a whole app-owned value where last-writer-wins is acceptable.

The sources are in `crates/kit/src/{tree,text,table,timeline,list,opaque}.rs`. The file `crates/kit/src/list.rs` is an internal helper. It is not a separate top-level mapping in this release.

Identity and values:

- Use stable `ElemId`s as identity, never positions.
- `Value` supports scalar properties and `ChunkRef`.
- Binary data belongs in a `BlobStore`, not in Loro.
- Use `session.blobs()` to get the store for a document's space.
- `MemoryBlobs` is suitable for tests.
- An unbound `AgentBlobs(client.blobs())` puts the content of a team-space document into the personal space.

## Session lifecycle

1. Open the session where the app opens the synced file: `Session::<App>::open(&Agent::local(), path, model, Some(file_bytes))`. Handle both `Open::Live` and `Open::NotLive`; a plain file keeps the app's ordinary behavior. Opening may wait up to 30 seconds for sync, so run it on a worker thread if the UI must stay responsive.
2. At each top-level command, keep the old model, apply the command, and call `local_change(&old, &new)` if the model changed. The session's own model must be the `old` one; a stale base returns `KitError::Stale`. For a gesture that needs a stable base, call `hold_remote(true)` until the gesture is complete. `group`, `begin_group` and `end_group` combine typing or another action into one undo step.
3. Call the nonblocking `poll()` once per UI frame. If `Polled.changed` is set, replace the app's model with `session.model()`. Register `set_waker` so that the session can request a repaint.
4. While the document is live, use `session.undo()`, `redo()` and `can_undo()` instead of snapshot undo. The kit's undo manager tracks this session's operations and preserves concurrent remote edits.
5. To save, take `save_point()`, serialize that point's model, and call `record(point, bytes)` before the atomic file replacement. If there is no recordable point, save the app's own model without a record. Autosave when the model is dirty or `file_behind()` is true.
6. Reconnecting to the agent is automatic, with a growing pause; `reconnect_now()` skips the pause. Changes queue in memory up to 64 MiB, and exceeding that returns `KitError::Stalled`. The writer emits commits under 512 KiB. Offline edits stay local and are sent when the connection returns. If the agent is absent or too old, live mode is off: keep normal file editing and show the status. An old agent may answer `agent_too_old` to newer calls.

## Collaboration and team spaces

All collaboration features are opt-in and additive. A basic app can keep its existing file behavior and ignore identity, team spaces, blobs, and presence.

### People

`people()` reports the person themself first, then the other active people ordered by name. Each entry has `name`, `colour.light`, `colour.dark`, `is_self`, and `peers`. Choose the light or dark sRGB value to suit the current background.

`peers()` still exposes the peers' presence bytes. Identity data is available only when the agent supplies it; older agents return no people.

### Read-only documents

A document is read-only for one of two reasons:

- A newer schema wrote it: `status().read_only`.
- The account is a viewer, or the space is archived: `status().viewer`.

In both cases, disable the edit, undo and redo controls and refuse local mutation. The kit returns `KitError::ReadOnly` or `KitError::ViewOnly`. Do not write a live save record. A role change updates the open session.

### Team documents and blobs

A team document is opened by its absolute path inside the synced team folder.

- `craft-cloud-app::Client::spaces` lists the spaces, and `set_space_sync` turns local sync on or off for one.
- Use `session.blobs()` or `client.blobs().for_path(path)` to bind blob operations to the document's space.
- Call `check_blobs` when you build a store. It asks the agent about the space binding, so call it off the UI thread.
- A store that is unbound, or bound to the wrong file, can make blobs unavailable to the other members.

## UI integration

In an eframe app, start `CloudWatch::start(app, connect, wake)` away from the UI flow, and pass egui's `Context::request_repaint` as `wake`. Then draw:

- `account_line` from `watch.state()`.
- `recents` from `watch.entries()`.
- `badge` from `session.status()`.
- `people_badge` from `session.people()`.

`add_recent(path, saved)` updates the shared recents.

The widgets take their theme from egui's visuals. The people badge chooses the light or dark palette colour and scales with egui's zoom.

The account line distinguishes these states: not running, signed out, online, offline, and paused. The badge covers Live, Waiting, Read-only, Offline, Stalled, and Stopped.

A tool that does not use egui should show the equivalent information:

- the agent and account state, and the live status;
- the shared recent paths;
- the reason for read-only;
- the waiting or stalled state;
- the active people.

Feed these from `craft-cloud-app::Client::status`, the recents watch and events, `Session::status`, and `Session::people`.

## Test the adapter

Enable the kit's `fake` feature in the dev-dependencies. `craft_cloud_kit::testing::{Hub, Fake}` then provides:

- several machines on one in-process hub;
- online and offline transitions, and restarts;
- dropped connections;
- an old-agent mode.

The demo's tests show how to use it:

- `apps/craft-cloud-kit-demo/tests/two_demos.rs` exercises two windows and undo and redo.
- `two_accounts.rs` exercises two accounts, presence identity, membership removal, and viewer refusal.

Assert that a viewer cannot mutate and that the viewer's attempted changes do not reach the editors.

Use the app's normal UI test harness to snapshot the live badge, the waiting, offline, read-only and stopped states, the people badge, the account line, and the recents interactions.

## Shipping checklist

- No panic paths; propagate errors and display them.
- Never log account names, file names, file contents, or tokens at info level or above.
- Verify offline editing, a missing agent, and an old agent.
- Verify that a viewer is refused and that a role change takes effect.
- Exercise large documents. The kit's writer cuts commits below 512 KiB.
- Verify undo and redo after remote changes and after reconnects.
- Save atomically, and use save points and `file_behind()` correctly.

## Troubleshooting

The codes originate in `crates/api/src/local.rs`; see [the agent's local API errors](agent.md).

| Code or condition | Meaning and response |
| --- | --- |
| `offline` | The server is unreachable. Preserve the edits and retry. |
| `not_signed_in` | Sign in through the agent before an account-scoped operation. |
| `agent_too_old` | The agent lacks the requested API call. Update Craft Cloud. |
| `read_only` | The team role blocks writes. Keep the edits out of the live state and disable mutation. |
| `not_live` / `not_in_folder` | The path is not currently a live synced file. Use the ordinary file flow. |
| `too_large` | A value, update, snapshot, or request exceeds its limit. |
| `blob_not_found` | The blob is absent from the local cache and from the account's storage. Check the document-space binding. |
| `KitError::Stale` | A remote update advanced the model's base. Retry the command from `session.model()`. |
| `KitError::Stalled` | The in-memory queue reached 64 MiB. Stop accepting more live changes until the session recovers. |
