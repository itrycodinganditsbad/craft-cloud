//! What can go wrong in the kit. Nothing here panics: a Loro error, a hostile or broken document, a lost agent all
//! become a [`KitError`], and the app keeps its model.

/// Why a kit call failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KitError {
    /// The document was made by a newer version of the app (or by another app): it is shown, but nothing is sent.
    #[error("this document needs a newer {app} (made with layout {version}); it opens read-only")]
    ReadOnly { app: String, version: u32 },
    /// More than the queue holds is waiting for the agent: no more changes until it is back.
    #[error("{0} bytes of changes are waiting for Craft Cloud; reconnect before changing more")]
    Stalled(usize),
    /// `old` isn't the model the session holds (a remote change came in between, or the app passed another one).
    #[error("the change was made from an older model than the session's; take the session's model and make it again")]
    Stale,
    /// A value too big for a live document: binary content goes in as a chunk reference instead.
    #[error("{what} is {len} bytes; at most {max} go in a live document (use a chunk reference)")]
    TooLarge { what: &'static str, len: usize, max: usize },
    /// A mapping's key, or a property's, that the kit doesn't take.
    #[error("{0}")]
    Invalid(String),
    /// Loro refused something (the document stays as it was before the change, or as the session rebuilt it).
    #[error("the live document: {0}")]
    Loro(String),
    /// The document's content isn't what the mapping expects (a hostile or broken peer); the part is skipped.
    #[error("the live document doesn't read: {0}")]
    Corrupt(String),
    /// The agent: not running, refused, or the connection broke.
    #[error("Craft Cloud: {0}")]
    Agent(String),
    /// The app's own codec failed.
    #[error("{0}")]
    App(String),
}

impl From<loro::LoroError> for KitError {
    fn from(e: loro::LoroError) -> Self {
        Self::Loro(e.to_string())
    }
}

impl From<loro::LoroEncodeError> for KitError {
    fn from(e: loro::LoroEncodeError) -> Self {
        Self::Loro(e.to_string())
    }
}

impl From<craft_cloud_app::Error> for KitError {
    fn from(e: craft_cloud_app::Error) -> Self {
        Self::Agent(e.to_string())
    }
}
