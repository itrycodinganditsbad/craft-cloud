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
  derived and never synced, before the app gets the model. Every machine normalises the same merge and writes its
  repair, so a repair must be deterministic: the same model in, the same repair out, with any new element's ID and
  value derived from what it repairs (never `ElemId::new()` or the clock), so two machines' repairs are one.
- Property values ([`Value`]) are numbers, strings, bytes and chunk references, at most 256 KiB each. Binary
  content never goes into the document: put it in a [`BlobStore`] ([`AgentBlobs`] is the agent's) and keep the
  [`ChunkRef`].

### Text and tables inside elements

A model with one text per element (a shape's text body, a slide's notes, a table cell's text) or a table per element
(a table shape) keeps them inside the element, under a key its codec names:

```rust
fn write(&self, id: ElemId, props: &mut Props<'_, '_>) -> Result<(), KitError> {
    let shape = self.shape(id);
    props.set("xfrm", shape.xfrm_json())?;
    props.text("text", &shape.story())?;          // a rich text, as a root one
    if let Some(table) = &shape.table {
        props.table("table", &TableSrc(table))?;  // rows, columns, cells
    }
    Ok(())
}
```

- A nested story maps exactly as a root one does (marks, paragraph attributes, the diff that writes one insert per
  keystroke). Two machines typing in one shape's text merge character by character, and a shape deleted on one machine
  while the other types in it is deleted on both.
- A table's cells are values, or, when its source says [`TableSource::cell_maps`], property maps written by
  `write_cell` like a node's, with stories of their own. Every cell of every row and column is then one; a row added
  on one machine while a column is added on the other leaves the cell they cross unmade: `normalise` makes it (the
  same plain cell on both, so the repairs are one).
- Both are mergeable children of the element's map: two machines making a shape's text at once make one. Write a
  story whenever the element exists (empty if it has none): an element whose story stops being written loses it,
  and typing that arrives in it then is lost with it.
- Reading back, [`TreeView::text_patched`] (and [`TableView::cell_text`]) patch the old text with the remote change's
  deltas, as a root story is patched; [`TreeTouch::stories`] says which stories changed, [`PropsRead::table`] reads a
  nested table whole.

A change the app makes by itself rather than the user (an upload that finished and now has its chunk reference) goes
through [`Session::background_change`]: sent like a local change, never an undo step.

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
- A change is one commit and goes as one update, whatever its size: a big one as its slices (at most 16,384
  operations each, which Loro imports in milliseconds; one update with much text takes it time growing with the
  square of the text) in one bundle, and one over the agent's 1 MiB frame limit in fragments that every receiver
  joins before anything imports them. A 5 MB paste goes, and undoes, whole: no machine, agent or app ever holds part
  of a change, so an app never sees, repairs or writes back half of one, whatever it does meanwhile (an edit, an
  undo, a save). The agent gives its apps every change whole: one that comes part of the way (its sender, or this
  machine, slept or lost the connection meanwhile) is kept by the agent until the rest comes, and the app sees nothing
  of it until then. A change over `MAX_CHANGE_BYTES` (8 MiB, half a document's log cap on the server) can't go: the
  session stops, as for one the agent refuses for good; the model stays editable, and saving it to the file keeps the
  work.
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

Save and autosave work as before, in this order:

```rust
// 1. Where the save starts: the model to write and the live document's frontiers it matches.
let point = self.session.save_point();
// 2. The bytes, from that model (or, with no save point, from the app's own model: no record then).
let bytes = to_file(point.as_ref().map_or(&self.model, |p| p.model()));
// 3. The record (content hash and frontiers) to the agent, before the file changes on disk.
if let Ok(point) = &point {
    self.session.record(point, &bytes);
}
// 4. The write.
write_atomic(&path, &bytes)?;
```

`save_point()` refuses (`KitError::NoRecord`) when a record would be wrong: the document is read-only here (a newer
app's), the session is stalled or stopped, or the model is behind the live document. The app saves its own model
then, and the file syncs as any file does.

A record names the live document's frontiers (its heads, one or two operation IDs however many machines and sessions
edited it), never a version vector. `record` first announces the save to the agent at once (its content's hash),
before the write: until the record itself comes, the agent's sync takes that content for a Craft save on its way, and
neither uploads it without its record nor makes a conflict copy of it, but waits. A save made while the app has no
agent connection (reconnecting, the agent restarting) can't be announced; the agent waits for those too, for any
change to a file whose document an app had open within the last 60 s (it keeps that on disk), and the session
announces and records them when it is back. Each content is waited for 10 minutes in all; a record that never comes
leaves the version as made outside Craft, as before. (The announcement isn't acknowledged: an agent stalled for longer
than its sync's settle window before it reads the announcement can still miss it.) The session sends the record on the same stream as its changes, after every
change it covers; the agent keeps it once all those operations are in its copy (a record that comes first waits for them, and
is dropped if the window closes before they come: that version then has no record, and its edits are in the file, so
nothing is lost). The record goes before the write so the agent knows the content before its sync pass sees it; while
the agent is away the session keeps the record and tells it once back, and the agent adds it to the version it
uploaded meanwhile (a server call of its own, appended once). Every session writes with a fresh random Loro peer of
its own; nothing is ever written with a peer another session may have had.

The agent records it with the version it uploads, so two machines saving one live file never make a conflict copy:
the save that includes the other's wins, or else the newer upload. Which includes which the agent asks its own copy
of the document; while it can't tell (it lacks the operations), neither side is taken and the next pass asks again,
and after 10 minutes still unknown it is a conflict copy, as for any two edits. A winner is never guessed. The losing
version is no conflict copy, but it stays in the server's history, and nothing of it is lost: the live document has
both sides' edits.

The live document is the truth and the file a copy of it, which may be late. To have it catch up on its own,
autosave when the model is dirty **or** when `session.file_behind()` says the live document is strictly ahead of the
file (another machine's edits came live, or this machine's after another machine's save won over this one's):

```rust
if autosave_due && (self.dirty || self.session.file_behind()) {
    self.save(); // the order above
}
```

The session knows the record of the file as it is on disk from the agent (it looks at the file of every open
document each second), and from its own last save. `file_behind()` is true only when everything the file was saved
from is in the live document and the live document has more. When the file has anything this live document lacks
(another machine's save that came by file sync before its live updates, a save made apart), it is false: this side
waits for the live updates, so a file ahead of the live document is never written over by itself, and two machines
whose live connection is down while files still sync never save back and forth. It is false too when no record of the
file is known (an edit outside Craft; an older agent), when a save couldn't carry a record, and for 5 s after a save
it asked for (a backstop). Once every window's file has the live document's frontiers, nobody saves again. When no
app has the document open, nobody writes the file: it stays behind until the next open, which shows the live document
and saves when it is ahead.

A file edited outside Craft has no record: it gets a conflict copy as always, and the kit says so on open
(`status().changed_outside`). It is decided once, at the open, from the bytes the app read (`Session::open`'s
`file_bytes`): the agent looks their hash up among this machine's records and the record of the file's version on the
server, so any Craft save, however many came after it, is never taken for an outside edit. A Craft save made without
a record (read-only, stopped, a record that never reached the agent) reads as changed outside too; its edits are in
the file.

Bringing the file in is the user's choice, never the kit's: the app shows `changed_outside` and offers it, and on a
yes calls `session.replace_with_file(file_model)`. The file's content then replaces the live content as one ordinary
change on every machine, one undo step named "Replace with the file's content" (`step_name`, for the Undo label). Say
plainly in the prompt that it **replaces the live edits made since the file was written**, and that Undo brings them
back. Saving clears `changed_outside`.

## Presence

`session.set_presence(Some(bytes))` shows this app's cursor or selection to the others (the bytes are the app's,
at most 4 KiB less 16); `session.peers()` is theirs, dropped 10 s after they go quiet. Drawing them is the app's.
Each presence also says the version of the Craft Cloud it runs on: `Peer::older()` (and `Status::older_peer`) is
true for a machine on a Craft Cloud too old for this one's big changes; show "<machine> runs an older Craft Cloud;
update it", with the machine's name from the app's own presence bytes.

A Craft Cloud on this computer older than the kit's live documents (local API below 2) opens a file as
`Open::NotLive(NotLive::AgentTooOld)`: open it as a plain file, and say to update Craft Cloud.

## Craft Cloud's piece of the window

`craft_cloud_egui::CloudWatch::start(app, connect, wake)` keeps the agent's state and the app's recents current off
the UI thread; `account_line(ui, &watch.state())` and `recents(ui, watch.entries(), now)` draw them (a click returns
the path); `watch.add_recent(path, saved)` puts a file on every machine's list.

## Testing the adapter

With the `fake` feature, `craft_cloud_kit::testing::{Hub, Fake}` is an agent in the test's own process: machines of
one account on a shared hub, with `set_online`, `restart` and `start`, `drop_connections`, and an old-agent mode
(`too_old`: before blobs and fragments). A fake machine joins a change's fragments before its apps get it, as the
agent does; `set_fragment_faults` stops a machine's link from the hub inside such a change for a while (`sleep`:
`SLEEP_TICKS` ticks, `delay`: a few; `Hub::tick` moves them on, `set_fragment_faults(None)` wakes it), and
`drop_connections_after` stops the agent part of the way through an app's change.
`craft_cloud_app::live::set_frame_limit_for_tests` makes changes of a few dozen KB go in fragments.
Two sessions on two fakes test merges, undo and reconnects with no server; the demo's tests drive two windows that
way with kittest.

## Known gaps

- **Undo of a deletion makes the elements again as copies.** Loro's undo of a tree deletion creates new nodes (new
  tree IDs, the same element IDs and properties); the old nodes stay deleted. What another machine changed in those
  elements between the deletion and the undo went to the old nodes and is lost, the same on every machine (the models
  stay alike). References other machines' repairs dropped meanwhile (a connector's end to a deleted shape) don't come
  back with the undo either. A soft delete (nodes moved to a hidden root, so undo moves them back) would keep them.

- **Every machine's agent must know fragments and bundles, and so must the server.** A server on live protocol 3
  stores a change in fragments whole and never sends a fragment or a bundle to an agent from before them: it is
  refused the document with "update Craft Cloud on this computer" (and the newer machines show "<machine> runs an
  older Craft Cloud" from its presence, best effort: once it is refused the document its presence stops). An agent
  never sends fragments or bundles to a server before protocol 3: a change that needs them is refused while it is
  the server, and what it holds of them waits on the machine, with every change after it, until it is updated. A
  new app on an older agent isn't live (`NotLive::AgentTooOld`). An older app on a newer agent still connects (the
  agent says its oldest local API version as `api`, and its newest as `api_max`) and gets recents and blobs. But an
  older live-editing app on a new agent can't read bundles: a newer app's big change shows in it as one that doesn't
  read, its model goes apart from the others', and it may save that model. The agent can't tell it apart yet (the
  docs channel names no app version), so quit every older Craft app before the agents are updated, and update the
  apps with them. The
  safe order: the server, then every agent and app.
- **After a server's restore**, a machine that sends up history the log lost sends a big Loro change in pieces (Loro
  merges one peer's commits into one change, so its boundaries aren't an app's): an app open on a machine that also
  lacked that history may see part of a change from before the restore.

## Costs to know

- Loro's undo of one big run of inserted text takes time growing with its square (about 10 s for a 1 MB paste).
  Undo of ordinary edits is instant.
- Loro's import of one update takes time growing with the square of the text in it when the text builds on another
  peer's (1 MiB: 7 s); the kit's slices keep a 4 MiB paste's import at about 10 ms. So does its checkout to an earlier
  version: an agent asked for a compaction while its copy is far past the asked version (its own big change not in the
  log yet) leaves the answer to another device.
- A big paste into a text another machine edits at that very moment is the slow case left: in one order of the two
  peers Loro works the text out again for every slice, so once a slice is slow the kit (and the agent) take the rest
  as one update, which costs those seconds once (about 7 s for 1.2 MB, 0.5 s for 300 KB) on the machine that was
  typing. Edits elsewhere at the same moment (another text, a property) cost nothing.
- A remote change to a rich text is patched into the old text; a tree's structure change reads the tree's
  structure again (cheap per node, 5,000 nodes in a few milliseconds); a table or timeline is read back by the app as
  much as it wants.
