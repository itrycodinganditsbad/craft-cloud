//! UUID-based identifiers. All are UUIDv7, so they sort roughly by creation time.
//!
//! Node IDs are made by the client (an offline batch can create a directory and put files in it before
//! the server has seen either). The server keys them by `(space, node)` and rejects duplicates, so an ID
//! never reaches into another space.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

macro_rules! uuid_id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            /// A new, time-ordered ID (UUIDv7).
            pub fn generate() -> Self {
                Self(Uuid::now_v7())
            }

            pub const fn from_uuid(uuid: Uuid) -> Self {
                Self(uuid)
            }

            pub const fn as_uuid(&self) -> &Uuid {
                &self.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                self.0.fmt(f)
            }
        }

        impl std::str::FromStr for $name {
            type Err = uuid::Error;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Uuid::parse_str(s).map(Self)
            }
        }
    };
}

uuid_id!(
    /// A user's tree of files and directories.
    SpaceId
);
uuid_id!(
    /// A file or directory. Never changes, through renames and moves.
    NodeId
);
uuid_id!(
    /// One immutable version of a file.
    VersionId
);
uuid_id!(
    /// A signed-in device (one per agent install).
    DeviceId
);
uuid_id!(
    /// Changes whenever a space's history may have gone backwards (a restore). A client that sees a new
    /// epoch reconciles instead of trusting its change counter.
    Epoch
);
uuid_id!(
    /// A live document (see [`crate::live`]). Keyed by `(space, doc)` on the server, like nodes, so an ID never
    /// reaches into another space. An app's recents document has an ID derived from the space and the app
    /// ([`crate::live::recents_doc`]), so every machine finds the same one without asking.
    DocId
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_ids_are_v7_and_distinct() {
        let a = NodeId::generate();
        let b = NodeId::generate();
        assert_ne!(a, b);
        assert_eq!(a.as_uuid().get_version_num(), 7);
    }

    #[test]
    fn display_and_parse_round_trip() {
        let id = Epoch::generate();
        let parsed: Epoch = id.to_string().parse().unwrap();
        assert_eq!(parsed, id);
    }

    #[test]
    fn json_is_a_plain_string() {
        let uuid = Uuid::parse_str("01a12b0b-9fca-73db-b82d-c7c33683601e").unwrap();
        let id = NodeId::from_uuid(uuid);
        assert_eq!(serde_json::to_string(&id).unwrap(), "\"01a12b0b-9fca-73db-b82d-c7c33683601e\"");
        assert!(serde_json::from_str::<NodeId>("\"not-a-uuid\"").is_err());
    }
}
