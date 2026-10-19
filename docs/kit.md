# The app kit: live documents in a Craft app

`craft-cloud-kit` makes a Craft app's documents live: the same file open on two machines (or in two apps on one)
shows every edit on the other as it is made, works offline, and never makes conflict copies of its own saves. The
app's whole change is one adapter. Everything else (talking to the Craft Cloud agent, the Loro document,
reconnecting, binary content, undo while live) is in the kit. `craft-cloud-egui` adds Craft Cloud's piece of the
window: the account line, the shared recent files and the document's badge.

The demo app in this repository (`apps/craft-cloud-kit-demo`, never packaged) is a complete example: a rich-text
story and a layer tree; its adapter is about 150 lines of `model.rs`, its hook one function of `ui.rs`. The
snippets below are from it.

## Adding it

```toml
[dependencies]
craft-cloud-kit = { git = "https://github.com/itrycodinganditsbad/craft-cloud", tag = "v0.1.0" }
craft-cloud-egui = { git = "https://github.com/itrycodinganditsbad/craft-cloud", tag = "v0.1.0" }

[dev-dependencies]
# The fake agent, for the adapter's tests.
craft-cloud-kit = { git = "https://github.com/itrycodinganditsbad/craft-cloud", tag = "v0.1.0", features = ["fake"] }
```

The kit brings Loro (and Loro's own crates) and nothing a Craft app doesn't already lock: `cargo xtask suite-deps
<the apps' Cargo.lock files>` checks it. There is no async runtime: the kit is blocking where it waits (opening) and
never blocks where it is called each frame.

## The adapter

The app implements [`LiveDocument`] for its model, the `Arc`-shared document its commands already clone
copy-on-write:

```rust
impl LiveDocument for DemoApp {
    type Doc = DemoDoc;
    const APP: &'static str = "kitdemo"; // as in recents
    const SCHEMA: u32 = 1;               // the layout the mappings write

    fn mappings(doc: &DemoDoc) -> Mappings<'_> {
        Mappings::new().tree("layers", LayersSrc::new(&doc.layers)).text("story", StorySrc(&doc.story))
    }

    fn apply_remote(old: &Arc<DemoDoc>, patch: &RemotePatch<'_>) -> Result<Arc<DemoDoc>, KitError> {
        let mut new = (**old).clone();
        if let Some(tree) = patch.tree("layers") {
            new.layers = Arc::new(build(&tree, tree.roots())); // or only the touched nodes
        }
        if let Some(story) = patch.text("story") {
            new.story = Arc::new(story);
        }
        Ok(Arc::new(new))
    }

    fn normalise(doc: &mut DemoDoc) { /* name a layer two machines both cleared */ }
    fn refresh_derived(doc: &mut DemoDoc) { doc.paragraphs = doc.story.paragraphs.len(); }
}
```

- **`mappings`** names the parts of the model the live document holds, each by a key, each a shape: a **tree**
  ([`TreeSource`]: layers, objects, bins), **rich text** ([`TextSource`]: a story; [`RichText`] is one), a **table**
  ([`TableSource`]: rows, columns and cells; one column makes keyed records), a **timeline** ([`TimelineSource`]:
  tracks, items, keyframes) or an **opaque value** ([`OpaqueSource`], last writer wins; [`Opaque`] wraps an
  [`OpaqueCodec`]). Everything else (view state, selection, caches) stays the app's own.
- The sources are the app's own types read in place. Give every source its `key` (the `Arc`'s address) and the tree
  its `subtree_key`: an unchanged part or subtree is skipped without being compared, so a keystroke in a 1 MB story
  costs well under a millisecond with 5,000 layers beside it. A table can say which cells changed
  (`changed_cells`).
- **IDs** come from the app (`ElemId`, a `u128`): the kit never makes them from positions. A model that addresses
  by index (paragraphs, table cells) adds IDs first; `ElemId::new()` makes a random one.
- **`apply_remote`** gets the model before a remote change and what it touched, part by part (`None`: untouched,
  keep it). Rebuild only that: the views say what changed (`touch()`), and read the part as it is now. A rich text
  comes whole, patched from the old one.
- **`normalise`** is required: it repairs what a merge can break (two layers with one name, a cell whose row went, a
  reference to a deleted element), so the result is a model the app could have made itself. The kit writes back what
  it changed (no undo step). It runs after every remote change, then **`refresh_derived`** recomputes what is
  derived and never synced, before the app gets the model.
- Property values ([`Value`]) are numbers, strings, bytes and chunk references, at most 256 KiB each. Binary
  content never goes into the document: put it in a [`BlobStore`] ([`AgentBlobs`] is the agent's) and keep the
  [`ChunkRef`].

## Opening

```rust
match Session::<DemoApp>::open(&Agent::local(), "Notes/story.kitdemo", file_model, Some(&file_bytes))? {
    Open::Live(session) => { /* the model is session.model() */ }
    Open::NotLive(why) => { /* a plain file: open and save it as always, with the app's own undo */ }
}
```

The path is relative to the synced folder. A file that has never been live is imported from the app's model (the
one it read from the file); otherwise the model is built from the live document, which may be newer than the file.
Opening waits for the agent (up to 30 s, when it must sync the file first): call it where the app opens files, or on
a thread (a `Session` is `Send`). A document made by a newer version of the app opens **read-only**
(`status().read_only`: show "update the app"); local changes and undo are refused.

## The hook

At the end of each top-level command, with the model before and after it:

```rust
if !Arc::ptr_eq(&before, &self.doc) {
    self.session.local_change(&before, &self.doc)?; // into Loro, out through the agent; one undo step
}
```

- `before` must be the session's model ([`KitError::Stale`] otherwise: a remote change came in between; make the
  change again from `session.model()`).
- A change is cut into commits under 512 KiB (the agent takes updates of up to 1 MiB): a 5 MB paste goes, and
  undoes, whole.
- **A gesture** (a drag, an IME composition) whose `before` must stay put until it ends: `session.hold_remote(true)`
  while it runs; remote changes wait and come at the first poll after.
- Typing: `session.group(|s| …)` (or `begin_group`/`end_group`) makes several changes one undo step.

## Each frame

```rust
let polled = self.session.poll();      // never blocks
if polled.changed { self.doc = self.session.model().clone(); }
craft_cloud_egui::badge(ui, &polled.status); // Live, Waiting 3, Read-only, Offline
```

`session.set_waker(Arc::new(move || ctx.request_repaint()))` wakes the window when something comes from the agent.
When the agent goes away the model stays editable: changes queue in memory (at most 64 MiB; past that
[`KitError::Stalled`]) and go out once the session is back, which it finds by itself with a growing pause, off the
UI thread (`reconnect_now()` skips the pause).

## Undo while live

While a document is live, undo and redo go through the session: `session.undo()`, `session.redo()`,
`can_undo()`. They take back only this machine's changes (another machine's edit to the same text stays), and the
result comes back to the model as a remote change does (take `session.model()`). The app's own snapshot undo would
silently revert other people's edits: use it only while the document isn't live. `session.undo_kind()` says which
applies.

## Saving

Save and autosave work as before. After writing the file, tell the session what was written:

```rust
write_atomic(&path, &bytes)?;
self.session.saved(&bytes)?; // the save record: content hash and the document's version
```

The agent records it with the version it uploads, so two machines saving one live file never make a conflict copy:
the save that includes the other's wins, or else the newer upload (the live document has both sides' edits, and the
next save writes them all). A file edited outside Craft has no record: it gets a conflict copy as always, and the
kit says so on open (`status().changed_outside`), so the app can bring the file in as an ordinary local change
(`local_change(model, file_model)`) or keep the live content.

## Presence

`session.set_presence(Some(bytes))` shows this app's cursor or selection to the others (the bytes are the app's,
at most 4 KiB less 8); `session.peers()` is theirs, dropped 10 s after they go quiet. Drawing them is the app's.

## Craft Cloud's piece of the window

`craft_cloud_egui::CloudWatch::start(app, connect, wake)` keeps the agent's state and the app's recents current off
the UI thread; `account_line(ui, &watch.state())` and `recents(ui, watch.entries(), now)` draw them (a click returns
the path); `watch.add_recent(path, saved)` puts a file on every machine's list.

## Testing the adapter

With the `fake` feature, `craft_cloud_kit::testing::{Hub, Fake}` is an agent in the test's own process: machines of
one account on a shared hub, with `set_online`, `restart` and `start`, `drop_connections`, and an old-agent mode.
Two sessions on two fakes test merges, undo and reconnects with no server; the demo's tests drive two windows that
way with kittest.

## Costs to know

- Loro's undo of one big run of inserted text takes time growing with its square (about 10 s for a 1 MB paste).
  Undo of ordinary edits is instant.
- A remote change to a rich text is patched into the old text; a tree's structure change reads the tree's
  structure again (cheap per node, 5,000 nodes in a few milliseconds); a table or timeline is read back by the app as
  much as it wants.
