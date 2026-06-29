//! Version DAG: every commit produces a new immutable document version.
//!
//! Because documents are values, history is just a list of versions with parent
//! links. Undo moves the head to a parent; branching commits from a non-tip
//! version; replay re-applies a transaction sequence. The structural sharing that
//! makes this cheap is a production concern (`im`); here we clone.

use crate::command::{apply, Transaction};
use crate::doc::Doc;
use crate::part::PartLib;

pub struct Version {
    pub doc: Doc,
    pub parent: Option<usize>,
    pub label: String,
}

pub struct History {
    pub versions: Vec<Version>,
    pub head: usize,
    tick: u64,
}

impl History {
    pub fn new(root: Doc) -> History {
        History {
            versions: vec![Version { doc: root, parent: None, label: "root".into() }],
            head: 0,
            tick: 0,
        }
    }

    pub fn doc(&self) -> &Doc {
        &self.versions[self.head].doc
    }

    /// Commit a transaction against the current head. On success a new version is
    /// appended and becomes head. On failure the head is unchanged (atomicity).
    pub fn commit(
        &mut self,
        txn: Transaction,
        lib: &PartLib,
        label: impl Into<String>,
    ) -> Result<usize, String> {
        self.tick += 1;
        let next = apply(self.doc(), &txn, lib, self.tick)?;
        let parent = Some(self.head);
        self.versions.push(Version { doc: next, parent, label: label.into() });
        self.head = self.versions.len() - 1;
        Ok(self.head)
    }

    /// Move head to its parent (undo). Returns false at the root.
    pub fn undo(&mut self) -> bool {
        match self.versions[self.head].parent {
            Some(p) => {
                self.head = p;
                true
            }
            None => false,
        }
    }

    /// Point head at an arbitrary existing version (used to branch).
    pub fn checkout(&mut self, v: usize) {
        assert!(v < self.versions.len());
        self.head = v;
    }
}
