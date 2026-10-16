//! The bytes on disk.
//!
//! A file is a sequence of records. A record is the payload's length (`u32`, little
//! endian), the CRC-32C of those four length bytes followed by the payload (`u32`,
//! little endian), then the payload. Checking the length under the CRC means a damaged
//! length is caught, not followed.
//!
//! An entry's payload is its index and term (`u64` each, little endian), a kind byte
//! (0 blank, 1 command, 2 membership) and, for a command, its bytes, for a membership,
//! the membership. A membership is the voters, then the learners, then the drained
//! members, each as a count (`u32`, little endian) and the servers in order (`u64`
//! each). The hard state's
//! payload is the term (`u64`), a vote byte (0 none, 1 some) and, with a vote, the
//! server (`u64`).
//!
//! A snapshot file is a header record, whose payload is the base's index and term and
//! the state's length in bytes (`u64` each, little endian), then the membership as of
//! the base if the snapshot holds one; followed by the state in records of at most
//! [`SNAPSHOT_CHUNK`] bytes each (none for an empty state).

use kbf_raft::{Entry, HardState, LogId, LogIndex, Membership, Payload, ServerId, Snapshot, Term};

use crate::crc::crc32c;

/// The bytes before a record's payload.
pub const HEADER: usize = 8;

/// The largest payload a record may carry: 64 MiB.
pub const MAX_PAYLOAD: usize = 64 << 20;

/// The fixed part of an entry's payload: index, term, kind.
const ENTRY_FIXED: usize = 17;

/// The most state bytes one record of a snapshot file carries: 1 MiB.
pub const SNAPSHOT_CHUNK: usize = 1 << 20;

/// The fixed part of a snapshot header's payload: base index, base term, state
/// length. The membership follows it, if there is one.
const SNAPSHOT_HEADER: usize = 24;

/// Appends one record carrying `payload` to `out`.
///
/// # Panics
///
/// If `payload` is longer than [`MAX_PAYLOAD`]; callers check first.
pub fn put_record(out: &mut Vec<u8>, payload: &[u8]) {
    assert!(payload.len() <= MAX_PAYLOAD, "payload over MAX_PAYLOAD");
    let len = u32::try_from(payload.len()).expect("MAX_PAYLOAD fits in u32");
    let len = len.to_le_bytes();
    out.extend_from_slice(&len);
    out.extend_from_slice(&crc32c(&[&len, payload]).to_le_bytes());
    out.extend_from_slice(payload);
}

/// The record at the start of `bytes`: its payload and its length on disk, or `None`
/// if the bytes there are not a whole record with a matching CRC.
pub fn get_record(bytes: &[u8]) -> Option<(&[u8], usize)> {
    let len_bytes: [u8; 4] = bytes.get(..4)?.try_into().ok()?;
    let crc_bytes: [u8; 4] = bytes.get(4..HEADER)?.try_into().ok()?;
    let len = usize::try_from(u32::from_le_bytes(len_bytes)).ok()?;
    if len > MAX_PAYLOAD {
        return None;
    }
    let payload = bytes.get(HEADER..HEADER + len)?;
    (crc32c(&[&len_bytes, payload]) == u32::from_le_bytes(crc_bytes))
        .then_some((payload, HEADER + len))
}

/// Whether a whole record with a matching CRC starts at any offset of `bytes`.
pub fn any_record(bytes: &[u8]) -> bool {
    (0..bytes.len()).any(|o| get_record(&bytes[o..]).is_some())
}

/// The payload of `entry`.
pub fn encode_entry(entry: &Entry) -> Vec<u8> {
    let membership;
    let data: &[u8] = match &entry.payload {
        Payload::Blank => &[],
        Payload::Command(c) => c,
        Payload::Membership(m) => {
            membership = encode_membership(m);
            &membership
        }
    };
    let mut out = Vec::with_capacity(ENTRY_FIXED + data.len());
    out.extend_from_slice(&entry.id.index.0.to_le_bytes());
    out.extend_from_slice(&entry.id.term.0.to_le_bytes());
    out.push(match entry.payload {
        Payload::Blank => 0,
        Payload::Command(_) => 1,
        Payload::Membership(_) => 2,
    });
    out.extend_from_slice(data);
    out
}

/// The entry `payload` encodes, or why it does not encode one.
pub fn decode_entry(payload: &[u8]) -> Result<Entry, &'static str> {
    if payload.len() < ENTRY_FIXED {
        return Err("entry shorter than its fixed part");
    }
    let id = LogId::new(Term(u64_at(payload, 8)), LogIndex(u64_at(payload, 0)));
    let data = &payload[ENTRY_FIXED..];
    let payload = match payload[16] {
        0 if data.is_empty() => Payload::Blank,
        0 => return Err("blank entry with bytes"),
        1 => Payload::Command(data.to_vec()),
        2 => Payload::Membership(decode_membership(data)?),
        _ => return Err("unknown entry kind"),
    };
    Ok(Entry { id, payload })
}

/// The bytes of `m` (see the module docs).
fn encode_membership(m: &Membership) -> Vec<u8> {
    let mut out = Vec::new();
    for servers in [
        m.voters().collect::<Vec<_>>(),
        m.learners().collect(),
        m.drained().collect(),
    ] {
        // A membership's servers are far fewer than `u32::MAX`.
        out.extend_from_slice(&(servers.len() as u32).to_le_bytes());
        for ServerId(s) in servers {
            out.extend_from_slice(&s.to_le_bytes());
        }
    }
    out
}

/// The membership all of `bytes` encode, or why they do not encode one.
fn decode_membership(bytes: &[u8]) -> Result<Membership, &'static str> {
    const MALFORMED: &str = "malformed membership";
    let (voters, rest) = servers(bytes).ok_or(MALFORMED)?;
    let (learners, rest) = servers(rest).ok_or(MALFORMED)?;
    let (drained, rest) = servers(rest).ok_or(MALFORMED)?;
    if !rest.is_empty() {
        return Err(MALFORMED);
    }
    Membership::new(voters, learners)
        .and_then(|m| m.with_drained(drained))
        .map_err(|_| "membership with no voter, a server in both roles or a drained stranger")
}

/// The servers a count and ids at the start of `bytes` name, and the bytes after them.
fn servers(bytes: &[u8]) -> Option<(Vec<ServerId>, &[u8])> {
    let count = u32::from_le_bytes(bytes.get(..4)?.try_into().ok()?);
    let end = usize::try_from(count)
        .ok()?
        .checked_mul(8)?
        .checked_add(4)?;
    let ids = bytes.get(4..end)?;
    let servers = ids.chunks_exact(8).map(|c| ServerId(u64_at(c, 0)));
    Some((servers.collect(), &bytes[end..]))
}

/// The payload of `hard`.
pub fn encode_hard(hard: HardState) -> Vec<u8> {
    let mut out = hard.term.0.to_le_bytes().to_vec();
    match hard.voted_for {
        None => out.push(0),
        Some(ServerId(s)) => {
            out.push(1);
            out.extend_from_slice(&s.to_le_bytes());
        }
    }
    out
}

/// The hard state `payload` encodes, or why it does not encode one.
pub fn decode_hard(payload: &[u8]) -> Result<HardState, &'static str> {
    let voted_for = match (payload.len(), payload.get(8)) {
        (9, Some(0)) => None,
        (17, Some(1)) => Some(ServerId(u64_at(payload, 9))),
        _ => return Err("malformed hard state"),
    };
    Ok(HardState {
        term: Term(u64_at(payload, 0)),
        voted_for,
    })
}

/// The bytes of a snapshot file holding `snapshot`.
pub fn encode_snapshot(snapshot: &Snapshot) -> Vec<u8> {
    let state = &snapshot.state;
    let mut head = Vec::with_capacity(SNAPSHOT_HEADER);
    head.extend_from_slice(&snapshot.base.index.0.to_le_bytes());
    head.extend_from_slice(&snapshot.base.term.0.to_le_bytes());
    head.extend_from_slice(&(state.len() as u64).to_le_bytes());
    if let Some(m) = &snapshot.membership {
        head.extend_from_slice(&encode_membership(m));
    }
    let chunks = state.len().div_ceil(SNAPSHOT_CHUNK);
    let mut out = Vec::with_capacity(HEADER * (chunks + 1) + SNAPSHOT_HEADER + state.len());
    put_record(&mut out, &head);
    for chunk in state.chunks(SNAPSHOT_CHUNK) {
        put_record(&mut out, chunk);
    }
    out
}

/// The snapshot a snapshot file's `bytes` hold, or where and why they do not hold
/// one. Every record must be whole with a matching CRC and the state as long as the
/// header says: the file was synced before it was renamed into place, so any damage
/// is damage, never a torn tail.
pub fn decode_snapshot(bytes: &[u8]) -> Result<Snapshot, (usize, &'static str)> {
    let damaged = |at| (at, "damaged snapshot record");
    let (head, mut off) = get_record(bytes).ok_or(damaged(0))?;
    if head.len() < SNAPSHOT_HEADER {
        return Err((0, "malformed snapshot header"));
    }
    let membership = match &head[SNAPSHOT_HEADER..] {
        [] => None,
        bytes => Some(decode_membership(bytes).map_err(|e| (0, e))?),
    };
    let base = LogId::new(Term(u64_at(head, 8)), LogIndex(u64_at(head, 0)));
    let mut state = Vec::new();
    while off < bytes.len() {
        let (chunk, used) = get_record(&bytes[off..]).ok_or(damaged(off))?;
        state.extend_from_slice(chunk);
        off += used;
    }
    if state.len() as u64 != u64_at(head, 16) {
        return Err((off, "snapshot state length does not match its header"));
    }
    Ok(Snapshot {
        base,
        state,
        membership,
    })
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&bytes[at..at + 8]);
    u64::from_le_bytes(b)
}

#[cfg(test)]
mod tests {
    use kbf_raft::Membership;

    use super::*;

    fn entry(index: u64, term: u64, payload: Payload) -> Entry {
        Entry {
            id: LogId::new(Term(term), LogIndex(index)),
            payload,
        }
    }

    /// Catches: a change to the on-disk layout (the golden bytes pin field order,
    /// widths and endianness, so an old log stays readable), and a codec that does
    /// not round-trip either kind or the vote.
    #[test]
    fn golden_bytes_and_round_trip() {
        let e = entry(2, 3, Payload::Command(vec![0xAB]));
        let mut rec = Vec::new();
        put_record(&mut rec, &encode_entry(&e));
        assert_eq!(
            hex(&rec),
            "120000009a95fc370200000000000000030000000000000001ab"
        );
        let (payload, used) = get_record(&rec).unwrap();
        assert_eq!(used, rec.len());
        assert_eq!(decode_entry(payload).unwrap(), e);
        let blank = entry(1, 1, Payload::Blank);
        assert_eq!(decode_entry(&encode_entry(&blank)).unwrap(), blank);

        let hard = HardState {
            term: Term(7),
            voted_for: Some(ServerId(2)),
        };
        assert_eq!(
            hex(&encode_hard(hard)),
            "0700000000000000010200000000000000"
        );
        assert_eq!(decode_hard(&encode_hard(hard)).unwrap(), hard);
        let none = HardState::default();
        assert_eq!(decode_hard(&encode_hard(none)).unwrap(), none);
    }

    /// Catches: a membership entry or a snapshot's membership that is not stored (a
    /// restart would forget every membership change), stored in another layout than
    /// the golden bytes pin, or decoded though malformed.
    #[test]
    fn memberships_round_trip_in_entries_and_snapshots() {
        let m = Membership::new([ServerId(1), ServerId(2)], [ServerId(3)])
            .unwrap()
            .with_drained([ServerId(3)])
            .unwrap();
        let e = entry(5, 2, Payload::Membership(m.clone()));
        let bytes = encode_entry(&e);
        assert_eq!(
            hex(&bytes),
            "050000000000000002000000000000000202000000010000000000000002000000\
             00000000010000000300000000000000010000000300000000000000"
        );
        assert_eq!(decode_entry(&bytes), Ok(e));
        for cut in [17, 19, 25, 33, 39, 45, 49, 51, 57] {
            assert!(decode_entry(&bytes[..cut]).is_err(), "cut at {cut}");
        }
        let mut longer = bytes.clone();
        longer.push(0);
        assert!(decode_entry(&longer).is_err(), "a trailing byte");
        let no_voters = Payload::Membership(Membership::new([ServerId(1)], []).unwrap());
        let mut none = encode_entry(&entry(1, 1, no_voters));
        none[17] = 0;
        assert!(decode_entry(&none).is_err(), "no voters");
        let mut stranger = bytes.clone();
        stranger[57] = 9;
        assert!(decode_entry(&stranger).is_err(), "a drained stranger");

        let snapshot = Snapshot {
            base: LogId::new(Term(2), LogIndex(5)),
            state: vec![0xCD],
            membership: Some(m),
        };
        assert_eq!(decode_snapshot(&encode_snapshot(&snapshot)), Ok(snapshot));
        let mut bad_head = Vec::new();
        put_record(&mut bad_head, &[0; SNAPSHOT_HEADER + 1]);
        assert_eq!(decode_snapshot(&bad_head), Err((0, "malformed membership")));
    }

    /// Catches: a record accepted with a bad CRC, a damaged length, a short body or a
    /// length over the cap, and payloads that decode though malformed (fail-stop needs
    /// every one refused).
    #[test]
    fn damaged_records_and_payloads_are_refused() {
        let mut rec = Vec::new();
        put_record(&mut rec, b"hello");
        for i in 0..rec.len() {
            let mut bad = rec.clone();
            bad[i] ^= 0x01;
            assert!(get_record(&bad).is_none(), "flip at {i}");
        }
        for n in 0..rec.len() {
            assert!(get_record(&rec[..n]).is_none(), "cut at {n}");
        }
        let mut huge = (u32::try_from(MAX_PAYLOAD).unwrap() + 1)
            .to_le_bytes()
            .to_vec();
        huge.extend_from_slice(&[0; 4]);
        assert!(get_record(&huge).is_none());
        assert!(any_record(&[&[0u8; 3][..], &rec].concat()));
        assert!(!any_record(&rec[1..]));

        let mut blank = encode_entry(&entry(1, 1, Payload::Blank));
        assert!(decode_entry(&blank[..16]).is_err());
        blank.push(9);
        assert_eq!(decode_entry(&blank), Err("blank entry with bytes"));
        blank[16] = 3;
        assert_eq!(decode_entry(&blank), Err("unknown entry kind"));
        assert!(decode_hard(&[0; 8]).is_err());
        assert!(decode_hard(&[0; 9].map(|_| 1)).is_err());
        assert!(decode_hard(&[0; 17]).is_err());
    }

    /// Catches: a change to the snapshot file's layout (the golden bytes pin the
    /// header's fields and the chunking), a state split at the wrong size or put back
    /// out of order, and a decoder that accepts a header of the wrong size, a state
    /// shorter or longer than its header says (a file cut at a record boundary), or
    /// a damaged chunk.
    #[test]
    fn snapshot_golden_bytes_chunks_and_refusals() {
        let small = Snapshot {
            base: LogId::new(Term(3), LogIndex(2)),
            state: vec![0xCD],
            membership: None,
        };
        assert_eq!(
            hex(&encode_snapshot(&small)),
            "18000000ef346cdb020000000000000003000000000000000100000000000000010000008e73c601cd"
        );
        assert_eq!(decode_snapshot(&encode_snapshot(&small)), Ok(small));
        let empty = Snapshot::default();
        let bytes = encode_snapshot(&empty);
        assert_eq!(
            bytes.len(),
            HEADER + SNAPSHOT_HEADER,
            "no chunk for no state"
        );
        assert_eq!(decode_snapshot(&bytes), Ok(empty));

        let state: Vec<u8> = (0..2 * SNAPSHOT_CHUNK + 5)
            .map(|i| u8::try_from(i % 251).unwrap())
            .collect();
        let big = Snapshot {
            base: LogId::new(Term(1), LogIndex(9)),
            state,
            membership: None,
        };
        let bytes = encode_snapshot(&big);
        let chunk = HEADER + SNAPSHOT_CHUNK;
        let first = HEADER + SNAPSHOT_HEADER;
        assert_eq!(bytes.len(), first + 2 * chunk + HEADER + 5, "three chunks");
        assert_eq!(decode_snapshot(&bytes), Ok(big));
        assert_eq!(
            decode_snapshot(&bytes[..first + chunk]),
            Err((
                first + chunk,
                "snapshot state length does not match its header"
            ))
        );
        let mut longer = bytes.clone();
        put_record(&mut longer, b"x");
        assert!(decode_snapshot(&longer).is_err(), "a chunk past the length");
        let mut bad = bytes;
        bad[first + chunk + HEADER] ^= 1;
        assert_eq!(
            decode_snapshot(&bad),
            Err((first + chunk, "damaged snapshot record"))
        );
        let mut short = Vec::new();
        put_record(&mut short, &[0; SNAPSHOT_HEADER - 1]);
        assert_eq!(
            decode_snapshot(&short),
            Err((0, "malformed snapshot header"))
        );
        assert_eq!(decode_snapshot(&[]), Err((0, "damaged snapshot record")));
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }
}
