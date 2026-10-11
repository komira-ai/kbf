//! Properties 1 and 6 against a disk: a replica that replays the durable log.
//!
//! What a power cut now would leave of the server's disk is read back with
//! `kbf-store`, and its entries through the server's applied index are applied to a
//! fresh `MetaState` as the server's metadata machine applies them (a metadata command
//! executed at its index, the first `Bootstrap` kept). The server's applied state must
//! equal the replica's, and its log id the replica's `Bootstrap`'s: every entry the
//! server has applied, and so answered from, was durable, and a restart or a follower
//! would apply it alike.

use kbf_log::{FarmLogId, Format, LogCommand};
use kbf_meta::{MetaState, Retention};
use kbf_raft::{Entry, Payload};
use kbf_store::{FaultFs, Options, Store};
use kbf_types::FarmTime;

use crate::check::Property;

/// A property that does not hold, and how.
pub type Broken = (Property, String);

fn equality(detail: String) -> Broken {
    (Property::ReplicaEquality, detail)
}

/// The entries `view` holds, as the store reads them back at open.
pub(crate) fn durable_entries(view: FaultFs) -> Result<Vec<Entry>, Broken> {
    match Store::open(view, Options::default()) {
        Ok((_, stored)) => Ok(stored.entries),
        Err(e) => Err(equality(format!("the durable log does not open: {e}"))),
    }
}

/// Farm time `after` an entry, which must not be before `before` (property 6).
pub(crate) fn monotone(before: FarmTime, after: FarmTime) -> Result<(), Broken> {
    if after < before {
        let d = format!("farm time goes back from {before:?} to {after:?}");
        return Err((Property::MonotoneTime, d));
    }
    Ok(())
}

/// The replica of `entries` through index `applied`: its metadata state and its first
/// `Bootstrap`'s log id.
pub(crate) fn replay(
    entries: &[Entry],
    applied: u64,
) -> Result<(MetaState, Option<FarmLogId>), Broken> {
    let mut replica = MetaState::new(Retention::default());
    let mut log_id = None;
    let mut last = 0;
    for entry in entries.iter().take_while(|e| e.id.index.0 <= applied) {
        last = entry.id.index.0;
        let Payload::Command(bytes) = &entry.payload else {
            continue;
        };
        match kbf_log::decode(Format::V1, bytes) {
            Ok(LogCommand::Meta(command)) => {
                let before = replica.now();
                replica.execute(last, command);
                monotone(before, replica.now())?;
            }
            Ok(LogCommand::Bootstrap(b)) => {
                log_id.get_or_insert(b.log_id);
            }
            other => return Err(equality(format!("entry {last} is {other:?}"))),
        }
    }
    if last < applied {
        let d = format!("entry {applied} is applied, but the durable log ends at {last}");
        return Err(equality(d));
    }
    Ok((replica, log_id))
}

/// Whether the server's `live` state and `log_id` are the replica's.
pub(crate) fn same_as_live(
    replica: &(MetaState, Option<FarmLogId>),
    live: &MetaState,
    log_id: FarmLogId,
) -> Result<(), Broken> {
    if *live != replica.0 {
        return Err(equality(
            "the applied state is not the durable log's".to_owned(),
        ));
    }
    if replica.1 != Some(log_id) {
        let d = format!("the durable log names log {:?}, not {log_id}", replica.1);
        return Err(equality(d));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::num::{NonZeroU32, NonZeroU64};

    use kbf_log::{Bootstrap, MachineVersion};
    use kbf_meta::Command;
    use kbf_raft::{LogId, LogIndex, Term};
    use kbf_store::Fault;

    use super::*;

    fn entry(index: u64, command: &LogCommand) -> Entry {
        let bytes = kbf_log::encode(Format::V1, command).expect("encode");
        Entry {
            id: LogId::new(Term(1), LogIndex(index)),
            payload: Payload::Command(bytes),
        }
    }

    fn id(n: u64) -> FarmLogId {
        FarmLogId::new(NonZeroU64::new(n).expect("non-zero"))
    }

    fn log() -> Vec<Entry> {
        let blank = Entry {
            id: LogId::new(Term(1), LogIndex(1)),
            payload: Payload::Blank,
        };
        let bootstrap = LogCommand::Bootstrap(Bootstrap {
            log_id: id(7),
            store_prefix: "cell/0000000000000007/".to_owned(),
        });
        let tick = LogCommand::Meta(Command::Tick(FarmTime::from_millis(5)));
        vec![blank, entry(2, &bootstrap), entry(3, &tick)]
    }

    /// Catches a replay that skips the metadata commands or the `Bootstrap`, or reads
    /// past the applied index.
    #[test]
    fn a_replay_applies_the_log_through_the_applied_index() {
        let (meta, log_id) = replay(&log(), 3).expect("a replica");
        assert_eq!(meta.now(), FarmTime::from_millis(5));
        assert_eq!(log_id, Some(id(7)));
        let (early, _) = replay(&log(), 2).expect("a replica");
        assert_eq!(early.now(), FarmTime::default());
        assert!(same_as_live(&(meta.clone(), Some(id(7))), &meta, id(7)).is_ok());
    }

    /// Catches each way property 1 can break going unreported: an applied entry the
    /// disk lacks, an entry the metadata machine does not apply, a state or a log id
    /// that differs, a durable log that does not open.
    #[test]
    fn each_difference_from_the_durable_log_breaks_replica_equality() {
        let broken = |r: Result<(), Broken>| r.expect_err("broken").0;
        let short = replay(&log(), 4).map(|_| ());
        assert_eq!(broken(short), Property::ReplicaEquality);
        let version = MachineVersion::new(NonZeroU32::new(2).expect("non-zero"));
        let mut control = log();
        control.push(entry(4, &LogCommand::MachineVersion(version)));
        assert_eq!(
            broken(replay(&control, 4).map(|_| ())),
            Property::ReplicaEquality
        );
        let (meta, _) = replay(&log(), 3).expect("a replica");
        let (other, _) = replay(&log(), 2).expect("a replica");
        assert_eq!(
            broken(same_as_live(&(meta.clone(), Some(id(7))), &other, id(7))),
            Property::ReplicaEquality
        );
        assert_eq!(
            broken(same_as_live(&(meta.clone(), Some(id(7))), &meta, id(8))),
            Property::ReplicaEquality
        );
        let dead = FaultFs::new();
        dead.fail_at(0, Fault::Error);
        assert_eq!(
            broken(durable_entries(dead).map(|_| ())),
            Property::ReplicaEquality
        );
    }

    /// Catches a time check that lets farm time go back (the mutant "`max(now)`
    /// dropped from apply" would need it).
    #[test]
    fn farm_time_going_back_breaks_monotone_time() {
        let (t1, t2) = (FarmTime::from_millis(1), FarmTime::from_millis(2));
        assert!(monotone(t1, t2).is_ok());
        assert_eq!(
            monotone(t2, t1).expect_err("back").0,
            Property::MonotoneTime
        );
    }
}
