//! What the raft directory records about its log beside the store's files: the log's
//! id (the cluster id) and how this node holds it, written once when the log is created
//! or joined (docs/design/ha.md, section 4.11; docs/design/cluster.md, section 5.3).
//! Every later start reads them from here, never from flags.
//!
//! A created log's record is four lines of text: a header, the log id as 16 hex
//! digits, `server 1` and `voters 1` (a node creates a single-voter cluster, as server
//! 1). A joined log's record is the header and the log id, then `server N`, the voters
//! and the learners of the membership the leader answered (each line the word and the
//! ids in order, space separated), and `joined`. Anything else is refused.

use std::num::NonZeroU64;

use kbf_log::FarmLogId;
use kbf_raft::{Membership, ServerId};
use kbf_store::{Fs, FsFile};

use super::RaftLogError;

/// The file that holds the record.
pub const RECORD: &str = "log-record";

/// The file a new record is written to before it is renamed over [`RECORD`].
pub const RECORD_TMP: &str = "log-record.tmp";

const HEADER: &str = "kbf raft log record 1\nlog_id ";
const MEMBERS: &str = "server 1\nvoters 1\n";

/// How this node holds the log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Formed {
    /// It created the cluster, as its single voter, server 1.
    Created,
    /// It joined the cluster as `server`, with the membership the leader answered.
    Joined {
        /// This node's Raft id.
        server: ServerId,
        /// The membership the leader answered: the membership the node opens with
        /// until its log holds a later one.
        membership: Membership,
    },
}

/// A record: the cluster and how this node holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recorded {
    /// The log id: the cluster id.
    pub log_id: FarmLogId,
    /// Created here, or joined.
    pub formed: Formed,
}

/// The record, or `None` for a directory that holds nothing yet besides
/// [`super::LOCK`]. A leftover [`RECORD_TMP`] (a create that died before its rename)
/// is removed first.
///
/// # Errors
/// [`RaftLogError::NotEmpty`] if the directory holds files but no record,
/// [`RaftLogError::Record`] if the record does not parse, and [`RaftLogError::Io`].
pub fn read<F: Fs>(fs: &F) -> Result<Option<Recorded>, RaftLogError> {
    let mut names = fs.list()?;
    // The lock `open_dir` holds is not part of the log.
    names.retain(|n| n != super::LOCK);
    if names.iter().any(|n| n == RECORD_TMP) {
        fs.remove(RECORD_TMP)?;
        names.retain(|n| n != RECORD_TMP);
    }
    if !names.iter().any(|n| n == RECORD) {
        return if names.is_empty() {
            Ok(None)
        } else {
            Err(RaftLogError::NotEmpty)
        };
    }
    let bytes = fs.read(RECORD)?;
    let id = std::str::from_utf8(&bytes).ok().and_then(parse);
    id.map(Some).ok_or(RaftLogError::Record)
}

/// Records `record` durably: written to [`RECORD_TMP`], synced, renamed over
/// [`RECORD`], and the directory synced.
///
/// # Errors
/// [`RaftLogError::Io`].
pub fn write<F: Fs>(fs: &F, record: &Recorded) -> Result<(), RaftLogError> {
    let mut file = fs.create(RECORD_TMP)?;
    file.append(render(record).as_bytes())?;
    file.sync()?;
    drop(file);
    fs.rename(RECORD_TMP, RECORD)?;
    fs.sync_dir()?;
    Ok(())
}

/// The text of `record`.
fn render(record: &Recorded) -> String {
    let log_id = record.log_id;
    match &record.formed {
        Formed::Created => format!("{HEADER}{log_id}\n{MEMBERS}"),
        Formed::Joined { server, membership } => {
            let ids = |it: &mut dyn Iterator<Item = ServerId>| {
                it.map(|s| format!(" {}", s.0)).collect::<String>()
            };
            let voters = ids(&mut membership.voters());
            let learners = ids(&mut membership.learners());
            let server = server.0;
            format!(
                "{HEADER}{log_id}\nserver {server}\nvoters{voters}\nlearners{learners}\njoined\n"
            )
        }
    }
}

/// The record exactly as [`write`] writes it; anything else is `None`.
fn parse(text: &str) -> Option<Recorded> {
    let (hex, rest) = text.strip_prefix(HEADER)?.split_once('\n')?;
    let n = u64::from_str_radix(hex, 16).ok()?;
    let log_id = FarmLogId::new(NonZeroU64::new(n)?);
    let formed = if rest == MEMBERS {
        Formed::Created
    } else {
        joined(rest)?
    };
    let record = Recorded { log_id, formed };
    (render(&record) == text).then_some(record)
}

/// The lines of a joined record after the log id.
fn joined(rest: &str) -> Option<Formed> {
    let mut lines = rest.split('\n');
    let server = ServerId(lines.next()?.strip_prefix("server ")?.parse().ok()?);
    let list = |line: &str, word: &str| -> Option<Vec<ServerId>> {
        let ids = line.strip_prefix(word)?;
        ids.split(' ')
            .skip(1)
            .map(|id| id.parse().ok().map(ServerId))
            .collect()
    };
    let voters = list(lines.next()?, "voters")?;
    let learners = list(lines.next()?, "learners")?;
    let membership = Membership::new(voters, learners).ok()?;
    membership
        .contains(server)
        .then_some(Formed::Joined { server, membership })
}
