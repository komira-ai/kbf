//! The value of an `opkey/<operation>` row: the operation's size key
//! ([`kbf_sched::Scheduler::set_size_key`]), a version byte (1) and the key (8 bytes,
//! big-endian). A row of another version is refused, not guessed at.

const VERSION: u8 = 1;

/// `key` as a row value.
pub(super) fn encode(key: u64) -> Vec<u8> {
    let mut row = vec![VERSION];
    row.extend(key.to_be_bytes());
    row
}

/// A row value as a size key, or what is wrong with it.
pub(super) fn decode(row: &[u8]) -> Result<u64, &'static str> {
    match row {
        [VERSION, key @ ..] => <[u8; 8]>::try_from(key)
            .map(u64::from_be_bytes)
            .map_err(|_| "a size key is not 8 bytes"),
        _ => Err("not a version 1 size key"),
    }
}
