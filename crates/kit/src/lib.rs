//! The Craft Cloud app kit: what a Craft app adds to make its documents live.
//!
//! An app's whole change is one adapter:
//! 1. implement [`LiveDocument`] for its model: which parts are shared ([`Mappings`]: a tree, rich text, a table, a
//!    timeline, opaque values), how to rebuild the parts a remote change touched ([`LiveDocument::apply_remote`]), and
//!    how to repair what a merge can break ([`LiveDocument::normalise`]);
//! 2. open the file through a [`Session`], and call [`Session::local_change`] at the end of each top-level command
//!    (old and new model);
//! 3. call [`Session::poll`] each frame, and take [`Session::model`] when it says the model changed;
//! 4. route undo and redo through the session while the document is live ([`Session::undo`]).
//!
//! Binary content never goes into the live document: it goes through a [`BlobStore`] (the agent's, [`AgentBlobs`])
//! and is referenced by [`ChunkRef`]. Nothing here starts an async runtime, and nothing panics: errors are
//! [`KitError`]s, and the app keeps its model.
//!
//! See `docs/kit.md` for the walk-through with the demo app.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

mod error;
pub(crate) mod list;
mod mapping;
pub(crate) mod opaque;
mod props;
mod session;
pub(crate) mod table;
pub(crate) mod text;
pub(crate) mod timeline;
pub(crate) mod tree;
mod value;
pub(crate) mod writer;

pub use error::KitError;
pub use mapping::{Attrs, Mappings, OpaqueSource, RemotePatch, Span, TableSource, TextSource, TimelineSource, Touched, TreeSource};
pub use opaque::Opaque;
pub use props::{Props, PropsRead};
pub use session::{
    Agent, Badge, CATCH_UP_EVERY, LiveDocument, MAX_BACKOFF, MAX_CHANGE_BYTES, MAX_QUEUE_BYTES, MAX_UNDO_TEXT_BYTES, NotLive, Open, PRESENCE_EVERY,
    PRESENCE_TTL, Peer, Polled, REPLACE_WITH_FILE, ReadOnly, SavePoint, SaveRecord, Session, Status, UndoKind,
};
pub use table::{TableTouch, TableView};
pub use text::RichText;
pub use timeline::{ItemRead, TimelineTouch, TimelineView};
pub use tree::{TreeTouch, TreeView};
pub use value::{AgentBlobs, BlobStore, ChunkRef, ElemId, MAX_VALUE_BYTES, MemoryBlobs, OpaqueCodec, Value};
pub use writer::OP_OVERHEAD;

/// For an app's adapter tests: the fake agent of `craft-cloud-app` (feature `fake`).
#[cfg(feature = "fake")]
pub mod testing {
    pub use craft_cloud_app::fake::{Fake, FragmentFaults, Hub};
    pub use craft_cloud_app::live::set_fragment_patience_for_tests;
}

/// For the kit's own tests and simulations: the mappings without a session.
#[doc(hidden)]
pub mod raw {
    use loro::LoroDoc;

    use crate::session::{Ctx, LiveDocument, write_diff};
    use crate::writer::{LOCAL_ORIGIN, Writer};
    use crate::{KitError, RemotePatch, Touched};

    /// A Loro document with the mappings' state, as a session keeps it.
    pub struct Replica {
        pub doc: LoroDoc,
        ctx: Ctx,
    }

    impl Default for Replica {
        fn default() -> Self {
            let doc = LoroDoc::new();
            doc.config_default_text_style(Some(loro::StyleConfig { expand: loro::ExpandType::After }));
            Self { doc, ctx: Ctx::default() }
        }
    }

    impl Replica {
        /// Writes `old` → `new`; the update (none if nothing changed, else one, whole) and how many commits (0 or 1).
        pub fn write<D: LiveDocument>(&mut self, old: Option<&D::Doc>, new: &D::Doc) -> Result<(Vec<Vec<u8>>, usize), KitError> {
            let mut w = Writer::new(&self.doc, LOCAL_ORIGIN);
            write_diff::<D>(&mut w, old, new, &mut self.ctx)?;
            w.finish()
        }

        /// Imports updates (a bundle's slices one after the other) and indexes the trees again.
        pub fn import(&mut self, updates: &[Vec<u8>], trees: &[&str]) -> Result<(), KitError> {
            for u in updates {
                let parts = craft_cloud_api::fragments::parts(u).ok_or_else(|| KitError::Invalid("a bundle cut short".into()))?;
                for part in parts {
                    self.doc.import(part)?;
                }
            }
            self.reindex(trees);
            Ok(())
        }

        pub fn reindex(&mut self, trees: &[&str]) {
            for key in trees {
                self.ctx.trees.insert((*key).to_owned(), crate::tree::TreeIndex::build(&self.doc.get_tree(*key)));
            }
        }

        /// The whole document read back through `D::apply_remote`, from `base`.
        pub fn read<D: LiveDocument>(&self, base: &std::sync::Arc<D::Doc>) -> Result<std::sync::Arc<D::Doc>, KitError> {
            let touched = Touched::everything();
            let patch = RemotePatch { doc: &self.doc, touched: &touched, trees: &self.ctx.trees, old: None };
            D::apply_remote(base, &patch)
        }

        /// How many operations this document's oplog holds.
        pub fn ops(&self) -> i64 {
            self.doc.oplog_vv().values().map(|c| i64::from(*c)).sum()
        }
    }
}
