//! The value of an `opwall/<operation>` row: the longest the operation may run, which the
//! scheduler's whole-node option reads ([`kbf_sched::Scheduler::set_max_wall`]), a version
//! byte (1), the seconds (8 bytes, big-endian) and the nanoseconds within the second (4
//! bytes, big-endian, below a billion). A row of another version is refused, not guessed at.

use std::time::Duration;

const VERSION: u8 = 1;

/// `wall` as a row value.
pub(super) fn encode(wall: Duration) -> Vec<u8> {
    let mut row = vec![VERSION];
    row.extend(wall.as_secs().to_be_bytes());
    row.extend(wall.subsec_nanos().to_be_bytes());
    row
}

/// A row value as a wall time, or what is wrong with it.
pub(super) fn decode(row: &[u8]) -> Result<Duration, &'static str> {
    let [VERSION, rest @ ..] = row else {
        return Err("not a version 1 wall time");
    };
    let (secs, nanos) = rest
        .split_at_checked(8)
        .ok_or("a wall time is not 12 bytes")?;
    let secs = u64::from_be_bytes(<[u8; 8]>::try_from(secs).expect("split at 8"));
    let nanos = <[u8; 4]>::try_from(nanos).map_err(|_| "a wall time is not 12 bytes")?;
    let nanos = u32::from_be_bytes(nanos);
    if nanos >= 1_000_000_000 {
        return Err("a wall time has a billion nanoseconds or more");
    }
    Ok(Duration::new(secs, nanos))
}
