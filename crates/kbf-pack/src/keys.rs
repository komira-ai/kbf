//! The object keys of packs: `cas/d/<node>/<boot>/<seq>` for a daemon's packs and
//! `cas/f/<frontend>/<boot>/<seq>` for a frontend's.

use kbf_objstore::{KeyError, ObjectKey};

use crate::error::PackError;

/// Which kind of process writes the packs of a key scheme.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriterKind {
    /// A daemon: its keys are `cas/d/<node>/<boot>/<seq>`.
    Daemon,
    /// A frontend: its keys are `cas/f/<frontend>/<boot>/<seq>`.
    Frontend,
}

impl WriterKind {
    fn letter(self) -> char {
        match self {
            WriterKind::Daemon => 'd',
            WriterKind::Frontend => 'f',
        }
    }
}

/// The keys one writer gives its packs. The writer's name and the boot are in every key, and
/// the sequence number counts up from zero within one boot, so two writers, or one writer
/// after a restart, never name the same key. A boot id must therefore be new on every start
/// of the process; a reused one names keys an earlier run may hold, which a create-only write
/// then refuses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackKeys {
    kind: WriterKind,
    name: String,
    boot: String,
}

impl PackKeys {
    /// The keys of the writer `name` (a node or a frontend) in the boot `boot`.
    ///
    /// # Errors
    /// [`PackError::Name`] when `name` or `boot` holds a `/`, which would put a key outside the
    /// writer's prefix; [`PackError::Key`] when they do not make an object key (an empty name,
    /// a character a key cannot hold, a key past the longest the store takes).
    pub fn new(kind: WriterKind, name: &str, boot: &str) -> Result<Self, PackError> {
        for part in [name, boot] {
            if part.contains('/') {
                return Err(PackError::Name(part.to_owned()));
            }
        }
        let keys = Self {
            kind,
            name: name.to_owned(),
            boot: boot.to_owned(),
        };
        keys.make("0")?;
        Ok(keys)
    }

    /// The key of the pack with sequence number `seq`: the number in sixteen hex digits, so
    /// that a listing of the writer's prefix is in sequence order.
    #[must_use]
    pub fn key(&self, seq: u64) -> ObjectKey {
        let last = format!("{seq:016x}");
        self.make(&last)
            .expect("the name and the boot were checked by `PackKeys::new`")
    }

    /// The key the create-only self test writes: in this writer's prefix, and not a sequence
    /// number.
    #[must_use]
    pub fn self_test_key(&self) -> ObjectKey {
        self.make("selftest")
            .expect("the name and the boot were checked by `PackKeys::new`")
    }

    /// The writer as the index records it: the kind and the name.
    #[must_use]
    pub fn writer(&self) -> String {
        format!("{}/{}", self.kind.letter(), self.name)
    }

    fn make(&self, last: &str) -> Result<ObjectKey, KeyError> {
        let letter = self.kind.letter();
        ObjectKey::new(format!("cas/{letter}/{}/{}/{last}", self.name, self.boot))
    }
}
