//! Paths inside the sync root: a list of validated names. Never absolute, never `..`.

use std::fmt;

use craft_cloud_proto::Name;
use serde::{Deserialize, Serialize};

/// A path relative to the sync root. The empty path is the root itself.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
pub struct RelPath(Vec<Name>);

impl RelPath {
    pub fn root() -> Self {
        Self(Vec::new())
    }

    pub fn from_names(names: Vec<Name>) -> Self {
        Self(names)
    }

    pub fn names(&self) -> &[Name] {
        &self.0
    }

    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }

    pub fn depth(&self) -> usize {
        self.0.len()
    }

    pub fn join(&self, name: &Name) -> Self {
        let mut names = self.0.clone();
        names.push(name.clone());
        Self(names)
    }

    pub fn parent(&self) -> Option<Self> {
        let (_, parent) = self.0.split_last()?;
        Some(Self(parent.to_vec()))
    }

    pub fn file_name(&self) -> Option<&Name> {
        self.0.last()
    }

    /// Whether `self` is `other` or lies below it.
    pub fn starts_with(&self, other: &RelPath) -> bool {
        self.0.starts_with(&other.0)
    }
}

impl fmt::Display for RelPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut first = true;
        for name in &self.0 {
            if !first {
                f.write_str("/")?;
            }
            f.write_str(name.as_str())?;
            first = false;
        }
        Ok(())
    }
}
