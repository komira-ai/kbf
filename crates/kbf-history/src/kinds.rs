//! Which kind of row a `ui/` key is, and who writes it.
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

/// Who writes a kind of row.
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
    /// Who writes rows of this kind.
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::Day;
    use crate::run_id::{RunId, StreamId};

    const W: WriterId = WriterId(0xa1);

    fn inv() -> InvKey {
        InvKey::from_record_id("build")
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

    /// Asserts that every key a builder produces is classified as the kind it names, under
    /// the default root and under a prefix.
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

    /// Asserts that a key of another root or another family, and a malformed key, has no
    /// kind (a sloppy parse would let a crafted key borrow a writer's id).
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

    /// Asserts that a row one writer owns is a writer row, an insert kind is an insert, and
    /// every row several writers share is fenced.
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
}
