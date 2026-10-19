//! Which kind of row a `ui/` key is, and which of them a process may still write once
//! another holds the state store's generation (`docs/design/frontend-proxy.md`, section 8).
//!
//! Every row the writer puts is one of:
//!
//! - **a writer's own row**: the key carries the writing process's [`WriterId`], so no
//!   other process writes it (`ichunk` and the per-writer header);
//! - **an insert**: a row that is created and never changed (`Cond::Absent`). A `stream`
//!   or `obj` row is unique by allocation: its key holds a stream id (from a counter
//!   block, or from the writer's own fallback range) or a run id, which no other writer
//!   is given. A list entry (`inv`) has an empty value, so the same entry from either of
//!   two writers is one row;
//! - **a shared row**: every writer reads and writes it, by counter or by
//!   read-modify-write (the stream counter, the shape and status rows and the by-status
//!   index). Two writers racing on one would lose an update, so only the generation
//!   holder writes them, behind the fence.
//!
//! [`IndexRoot::permit`] accepts the first two kinds, as the form the writer writes
//! them, and nothing else.

use kbf_state::{Cond, Op, Permit};

use crate::keys::{IndexRoot, InvKey, WriterId};

/// A `ui/` row's kind, from its key alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowKind {
    /// `ui/streams`: the counter writers reserve stream-id blocks from.
    StreamsCounter,
    /// `ui/invid/<inv>/<writer>`, one writer's header; or `ui/invid/<inv>`, the one row
    /// every writer shared before (`None`).
    Header(Option<WriterId>),
    /// `ui/ichunk/<inv>/<writer>/<seq>`.
    Chunk(WriterId),
    /// `ui/stream/<day>/<stream>/<open ms>`.
    Stream,
    /// `ui/obj/<family>/<day>/<name>`.
    Obj,
    /// `ui/inv/<instance>/<list ms>/<inv>`: a list entry.
    Listed,
    /// `ui/shape/...`: the shape catalog.
    Shape,
    /// `ui/status/...`: a test's status row.
    Status,
    /// `ui/statusx/...`: the by-status index.
    StatusIndex,
}

/// Who may write a kind of row once the state store has a newer generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ownership {
    /// Only the writer whose id the key carries.
    Writer(WriterId),
    /// Any writer: an insert under a key no other writer is given, or with an empty value.
    Insert,
    /// Only the generation holder, behind the fence.
    Fenced,
}

impl RowKind {
    /// Who may write rows of this kind after a takeover.
    #[must_use]
    pub fn ownership(self) -> Ownership {
        match self {
            RowKind::Chunk(writer) | RowKind::Header(Some(writer)) => Ownership::Writer(writer),
            RowKind::Stream | RowKind::Obj | RowKind::Listed => Ownership::Insert,
            RowKind::StreamsCounter
            | RowKind::Header(None)
            | RowKind::Shape
            | RowKind::Status
            | RowKind::StatusIndex => Ownership::Fenced,
        }
    }
}

/// `text` as sixteen lowercase hex digits, the form every key writes a writer id in.
fn hex16(text: &str) -> Option<u64> {
    let digits = text
        .bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if text.len() == 16 && digits {
        u64::from_str_radix(text, 16).ok()
    } else {
        None
    }
}

impl IndexRoot {
    /// The kind of the row `key` names, or `None` for a key that is not one of this
    /// root's rows (or is a malformed one).
    #[must_use]
    pub fn kind(&self, key: &str) -> Option<RowKind> {
        let rest = key.strip_prefix(self.as_str())?;
        let Some((family, tail)) = rest.split_once('/') else {
            return (rest == "streams").then_some(RowKind::StreamsCounter);
        };
        match family {
            "invid" => {
                let (inv, writer) = tail
                    .split_once('/')
                    .map_or((tail, None), |(i, w)| (i, Some(w)));
                InvKey::parse(inv).ok()?;
                match writer {
                    None => Some(RowKind::Header(None)),
                    Some(w) => hex16(w).map(|w| RowKind::Header(Some(WriterId(w)))),
                }
            }
            "ichunk" => {
                let mut parts = tail.split('/');
                let (inv, writer, seq) = (parts.next()?, parts.next()?, parts.next()?);
                InvKey::parse(inv).ok()?;
                let seq_ok = seq.len() == 8 && u32::from_str_radix(seq, 16).is_ok();
                (seq_ok && parts.next().is_none())
                    .then(|| hex16(writer).map(|w| RowKind::Chunk(WriterId(w))))
                    .flatten()
            }
            "stream" => Some(RowKind::Stream),
            "obj" => Some(RowKind::Obj),
            "inv" => Some(RowKind::Listed),
            "shape" => Some(RowKind::Shape),
            "status" => Some(RowKind::Status),
            "statusx" => Some(RowKind::StatusIndex),
            _ => None,
        }
    }

    /// What `writer` may write unchecked once another process holds the state store's
    /// generation: its own chunk and header rows, and the inserts of [`RowKind`], each
    /// only in the form the writer writes it (a chunk row is created, never replaced; a
    /// list entry has an empty value). Anything else, a delete and a counter included, is
    /// refused.
    #[must_use]
    pub fn permit(&self, writer: WriterId) -> Permit {
        let root = self.clone();
        std::sync::Arc::new(move |op: &Op| {
            let Op::Put {
                key, value, cond, ..
            } = op
            else {
                return false;
            };
            match root.kind(key) {
                Some(RowKind::Chunk(owner)) => owner == writer && *cond == Cond::Absent,
                Some(RowKind::Header(Some(owner))) => {
                    owner == writer && matches!(cond, Cond::Any | Cond::Absent)
                }
                Some(RowKind::Stream | RowKind::Obj) => *cond == Cond::Absent,
                Some(RowKind::Listed) => *cond == Cond::Any && value.is_empty(),
                _ => false,
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use kbf_state::Version;

    use super::*;
    use crate::keys::Day;
    use crate::run_id::{RunId, StreamId};

    const W: WriterId = WriterId(0xa1);
    const OTHER: WriterId = WriterId(0xb2);

    fn inv() -> InvKey {
        InvKey::from_record_id("build")
    }

    fn put(key: &str, cond: Cond, value: &[u8]) -> Op {
        Op::Put {
            key: key.to_owned(),
            value: value.to_vec(),
            ttl: None,
            cond,
        }
    }

    /// Every key the writer writes, with the kind its builder says it is.
    fn rows(root: &IndexRoot) -> Vec<(String, RowKind)> {
        let (day, stream) = (Day::from_number(20_261_019), StreamId(7));
        let inv = inv();
        vec![
            (root.streams_counter(), RowKind::StreamsCounter),
            (root.invid(&inv), RowKind::Header(None)),
            (root.header(&inv, W), RowKind::Header(Some(W))),
            (root.ichunk(&inv, W, 3), RowKind::Chunk(W)),
            (root.stream(day, stream, 5), RowKind::Stream),
            (root.obj_flog(RunId::new(1_000, stream, 1)), RowKind::Obj),
            (root.inv_list("main", 9, &inv), RowKind::Listed),
            (root.shape("main", 1, "cfg"), RowKind::Shape),
            (root.status("main", 1, "cfg"), RowKind::Status),
            (
                root.statusx("failing", "main", 1, "cfg"),
                RowKind::StatusIndex,
            ),
        ]
    }

    // Catches: a key builder that moves a row to a place `kind` does not know (the table
    // of what a superseded writer may write would then miss it and refuse a row it must
    // write, or accept one it must not), under the default root and under a prefix.
    #[test]
    fn every_key_the_writer_builds_is_of_the_kind_it_is() {
        for root in [
            IndexRoot::default(),
            IndexRoot::under("farm-a/").expect("a prefix"),
        ] {
            for (key, kind) in rows(&root) {
                assert_eq!(root.kind(&key), Some(kind), "{key}");
            }
        }
    }

    // Catches: a key of another root or another writer's family taken for ours, and a
    // malformed key taken for a row (a sloppy parse would let a planted key borrow a
    // writer's id).
    #[test]
    fn keys_that_are_not_rows_of_the_root_have_no_kind() {
        let root = IndexRoot::default();
        let other = IndexRoot::under("farm-b/").expect("a prefix");
        let inv = inv();
        let writer = format!("{W}");
        for key in [
            other.ichunk(&inv, W, 1),
            "op/1".to_owned(),
            "ui/".to_owned(),
            "ui/streamsx".to_owned(),
            "ui/unknown/x".to_owned(),
            format!("ui/invid/{inv}x"),
            format!("ui/invid/short/{writer}"),
            format!("ui/invid/{inv}/{}", writer.to_uppercase()),
            format!("ui/invid/{inv}/{}", &writer[1..]),
            format!("ui/invid/{inv}/+{}", &writer[1..]),
            format!("ui/ichunk/{inv}/{writer}"),
            format!("ui/ichunk/{inv}/{writer}/1"),
            format!("ui/ichunk/{inv}/{writer}/zzzzzzzz"),
            format!("ui/ichunk/{inv}/{writer}/00000001/extra"),
            format!("ui/ichunk/{inv}/nothex/00000001"),
            format!("ui/ichunk/bad/{writer}/00000001"),
        ] {
            assert_eq!(root.kind(&key), None, "{key}");
        }
    }

    // Catches: a kind that is fenced when it is shared, or free when it is not: only the
    // rows a single writer owns and the inserts are writable by a superseded process.
    #[test]
    fn ownership_follows_who_can_write_a_row() {
        assert_eq!(RowKind::Chunk(W).ownership(), Ownership::Writer(W));
        assert_eq!(RowKind::Header(Some(W)).ownership(), Ownership::Writer(W));
        for kind in [RowKind::Stream, RowKind::Obj, RowKind::Listed] {
            assert_eq!(kind.ownership(), Ownership::Insert);
        }
        for kind in [
            RowKind::StreamsCounter,
            RowKind::Header(None),
            RowKind::Shape,
            RowKind::Status,
            RowKind::StatusIndex,
        ] {
            assert_eq!(kind.ownership(), Ownership::Fenced);
        }
    }

    // Catches: a permit that lets a superseded writer write what only the generation
    // holder may: a shared row, another writer's row, a delete, a counter, a precondition
    // other than the one the row is written with, a list entry with a value, or a key that
    // is no row at all. Each row of the table is judged as the writer writes it and in the
    // forms it never does.
    #[test]
    fn the_permit_accepts_the_writers_own_rows_and_inserts_only() {
        let root = IndexRoot::default();
        let permit = root.permit(W);
        let (day, inv) = (Day::from_number(20_261_019), inv());
        let version = Cond::Version(Version(4));
        let yes = [
            put(&root.ichunk(&inv, W, 1), Cond::Absent, b"row"),
            put(&root.header(&inv, W), Cond::Any, b"header"),
            put(&root.header(&inv, W), Cond::Absent, b"restored"),
            put(&root.stream(day, StreamId(1), 2), Cond::Absent, b"s"),
            put(
                &root.obj_flog(RunId::new(1, StreamId(1), 1)),
                Cond::Absent,
                b"o",
            ),
            put(&root.inv_list("main", 1, &inv), Cond::Any, b""),
        ];
        for op in &yes {
            assert!(permit(op), "{op:?}");
        }
        let no = [
            put(&root.ichunk(&inv, OTHER, 1), Cond::Absent, b"row"),
            put(&root.ichunk(&inv, W, 1), Cond::Any, b"row"),
            put(&root.header(&inv, OTHER), Cond::Any, b"header"),
            put(&root.header(&inv, W), version, b"header"),
            put(&root.invid(&inv), Cond::Any, b"legacy"),
            put(&root.stream(day, StreamId(1), 2), Cond::Any, b"s"),
            put(
                &root.obj_flog(RunId::new(1, StreamId(1), 1)),
                Cond::Any,
                b"o",
            ),
            put(&root.inv_list("main", 1, &inv), Cond::Any, b"v"),
            put(&root.inv_list("main", 1, &inv), Cond::Absent, b""),
            put(&root.shape("main", 1, "cfg"), Cond::Absent, b"shape"),
            put(&root.status("main", 1, "cfg"), version, b"status"),
            put(&root.statusx("failing", "main", 1, "cfg"), Cond::Any, b""),
            put(&root.streams_counter(), Cond::Any, b""),
            put("op/1", Cond::Absent, b"x"),
            Op::Delete {
                key: root.ichunk(&inv, W, 1),
                cond: Cond::Any,
            },
            Op::Next {
                counter: root.streams_counter(),
            },
            Op::Check {
                key: root.header(&inv, W),
                cond: Cond::Any,
            },
        ];
        for op in &no {
            assert!(!permit(op), "{op:?}");
        }
    }
}
