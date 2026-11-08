# The app kit: live documents in a Craft app

`craft-cloud-kit` makes a Craft app's documents live: the same file open on two machines (or in two apps on one)
shows every edit on the other as it is made, works offline, and never makes conflict copies of its own saves. The
app's whole change is one adapter. Everything else (talking to the Craft Cloud agent, the Loro document,
reconnecting, binary content, undo while live) is in the kit. `craft-cloud-egui` adds Craft Cloud's piece of the
window: the account line, the shared recent files, the document's badge and who else is in the document.

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
  [`ChunkRef`]. For documents that may be in a team space build the agent's store with `session.blobs()` (see
  "Team spaces" below).

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
at most 4 KiB less 8); `session.peers()` is theirs, dropped 10 s after they go quiet. Drawing them is the app's.

## Team spaces

A file in a team space's folder is opened as any other: the app passes the path it has (an absolute path inside the
synced team folder; the agent routes a document by its path), and the session is live between the space's members.
Editing, undo and saving need no change. Three things are new and additive; an app that uses none of them behaves as it
did.

- **Who is there.** `session.people()` lists the people of the document's space who are in the document: yourself first,
  then by name, each a `Person { name, colour, is_self, peers }`. `colour` is the account's palette colour
  (`PaletteColour`, with a `light` and a `dark` sRGB value; use the one for the background it is drawn on), the same for
  an account in every window and on every machine. `peers` are the Loro peers of that account's windows in this document
  (the keys of `session.peers()`), so an app can draw a cursor in its owner's colour. A person is here while they send
  presence (`set_presence`, dropped after `PRESENCE_TTL` quiet) or are yourself; your own other windows are you. The list is
  empty while the agent hasn't said who is in the space and for an agent from before team spaces. The kit asks the agent for
  identity itself and still hands the app its peers' own bytes (`peers()` is unchanged); the names come from the agent, and
  an account's presence is its account's word, not the app's bytes. `craft_cloud_egui::people_badge(ui, &session.people())`
  draws them (initials in their colours, at most four, then "+N"; names on hover).
- **Viewers.** A member who may only read a team space's documents (a viewer, or the space is archived) opens them
  read-only: `status().viewer` is true, the badge says "Read-only", `can_undo()` is false, and `local_change`, `undo` and
  `redo` refuse with `KitError::ViewOnly` (the same path as a document of a newer app, `KitError::ReadOnly`, which `viewer`
  does not set). Nothing is sent, and `save_point` refuses (`KitError::NoRecord`), so the app saves its own model without a
  record. A role that changes while the document is open is told by the agent (`role`, both ways): the session flips
  `viewer` at once, and a refusal with `read_only` for a change sent just before also makes it one. The refused change is
  never sent again (unlike a refusal for a full disk, which goes again) and stays queued, unsent; when the role is raised
  the queue goes up at once, with no new open. If the app ends first, dropping the session hands the queue to the agent,
  which keeps it and sends it once the account may write. The session waits up to two seconds for the agent to confirm each
  change before it lets go of the connection. Not covered: when the agent is not there, or no longer follows the document
  (its space stopped syncing, or access was revoked), the changes are in the window and, once saved, in the file only; the
  kit says so on standard error.
- **A space that stops syncing** (turned off, the folder moved, access lost): the agent tells the open document it stopped
  following it (`status().stopped`, badge Stopped), and a change the agent answers with `not_found` means the same. The
  session drops the link and opens the document again by itself, with a pause that grows (200 ms up to 8 s; the agent
  answers `not_live` until the space syncs again); nothing is sent meanwhile, the changes stay queued in the window, and
  they go up once the document is open again. `reconnect_now()` skips the pause. Save the document if the space may not
  come back. (A document the agent stopped following for another reason, as Loro failing on it, is opened again the same
  way, with the same growing pause.)
- **Binary values.** `session.blobs()` is the agent's blob store bound to the document's file, so its blobs go to the
  document's space and every member gets them back. Use it where an app wrote `AgentBlobs(client.blobs())`; that keeps blobs
  in the account's personal space, where the other members get `blob_not_found`. Call `session.check_blobs(&store)` once
  where the app builds its store: in a team space an agent store that isn't bound to the document adds a text to
  `session.warnings()` (once per session) and returns false, as does a store bound to another document's file (an app that
  builds one store and uses it for every document). `check_blobs` asks the agent once whether the space is a team space:
  call it where the store is built, not on the UI thread; if the agent can't answer, the store is not trusted. A team document
  is opened by an absolute path inside the space's synced folder (the agent routes by path); a store bound to a relative path
  keeps blobs in the personal space. A viewer's put into a team space is `KitError::ViewOnly`. Behind it,
  `client.blobs().for_path(path)` is the bound store of `craft-cloud-app`, and `Blobs::put_for`/`get_for` name the path on every
  call.

`KitError` and `Person` are `#[non_exhaustive]` from this release (match `KitError` with a wildcard arm; make a `Person` with
`Person::new`), so later additions are not breaking; `Status` stays exhaustive and gained the field `viewer`.

## Craft Cloud's piece of the window

`craft_cloud_egui::CloudWatch::start(app, connect, wake)` keeps the agent's state and the app's recents current off
the UI thread; `account_line(ui, &watch.state())` and `recents(ui, watch.entries(), now)` draw them (a click returns
the path); `watch.add_recent(path, saved)` puts a file on every machine's list. `badge(ui, session.status())` is the
document's state and `people_badge(ui, &session.people())` who else is in it.

## Testing the adapter

With the `fake` feature, `craft_cloud_kit::testing::{Hub, Fake}` is an agent in the test's own process: machines of
one account on a shared hub, with `set_online`, `restart` and `start`, `drop_connections`, and an old-agent mode.
Two sessions on two fakes test merges, undo and reconnects with no server; the demo's tests drive two windows that
way with kittest. For a team space, `hub.team()` makes the hub's space one, `hub.machine_as(name, account)` adds a machine
of another account (the members), `hub.set_read_only(account, true)` makes an account a viewer, and
`hub.remove_member(account)` takes one out of the space; the fake then lists the members, stamps presence with the sender,
names the space in `opened` and keeps `?for=` blobs in the team space.

## Costs to know

- Loro's undo of one big run of inserted text takes time growing with its square (about 10 s for a 1 MB paste).
  Undo of ordinary edits is instant.
- A remote change to a rich text is patched into the old text; a tree's structure change reads the tree's
  structure again (cheap per node, 5,000 nodes in a few milliseconds); a table or timeline is read back by the app as
  much as it wants.
