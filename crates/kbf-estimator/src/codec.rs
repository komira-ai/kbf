//! The stored form of a [`Summary`]: one version byte, fixed little-endian fields and
//! an FNV-1a checksum. The same summary always encodes to the same bytes; anything that
//! is not exactly such a record decodes to `None` and the key starts cold.

use crate::key::{Basis, Day};
use crate::summary::{MAX_DAY, MAX_SAMPLES, RAISE_ONLY_DAYS, Summary};

const VERSION: u8 = 1;
const SLOTS: usize = 9;
const RING: usize = 32;
const DAYS: usize = 8;
/// version, basis, n_total, cpu_n, last_seen, oom_floor, floor_anchor, floor_day,
/// last_kill_n, raise_only_until, chunk_max, chunk_held, ring, day_stamp, day_max,
/// cpu_ring, checksum.
pub(crate) const LEN: usize = 1
    + 1
    + 8
    + 8
    + 4
    + 8
    + 8
    + 4
    + 8
    + 4
    + 8 * SLOTS
    + 8 * SLOTS
    + 8 * RING
    + 4 * DAYS
    + 8 * DAYS
    + 4 * RING
    + 8;

pub(crate) fn checksum(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

struct Reader<'a> {
    bytes: &'a [u8; LEN],
    at: usize,
}

impl Reader<'_> {
    fn array<const N: usize>(&mut self) -> [u8; N] {
        // `LEN` is the sum of every read, so `at + N` never passes the end.
        let mut out = [0; N];
        out.copy_from_slice(&self.bytes[self.at..self.at + N]);
        self.at += N;
        out
    }

    fn u8(&mut self) -> u8 {
        self.array::<1>()[0]
    }

    fn u32(&mut self) -> u32 {
        u32::from_le_bytes(self.array())
    }

    fn u64(&mut self) -> u64 {
        u64::from_le_bytes(self.array())
    }
}

impl Summary {
    /// The stored form: `LEN` bytes, deterministic.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(LEN);
        out.push(VERSION);
        out.push(match self.basis {
            Basis::Cgroup => 1,
            Basis::MacFootprint => 2,
        });
        out.extend_from_slice(&self.n_total.to_le_bytes());
        out.extend_from_slice(&self.cpu_n.to_le_bytes());
        out.extend_from_slice(&self.last_seen.0.to_le_bytes());
        out.extend_from_slice(&self.oom_floor.to_le_bytes());
        out.extend_from_slice(&self.floor_anchor.to_le_bytes());
        out.extend_from_slice(&self.floor_day.0.to_le_bytes());
        // A kill is counted after its own sample, so the index is never 0.
        out.extend_from_slice(&self.last_kill_n.unwrap_or(0).to_le_bytes());
        out.extend_from_slice(&self.raise_only_until.0.to_le_bytes());
        for max in self.chunk_max.into_iter().chain(self.chunk_held) {
            out.extend_from_slice(&max.to_le_bytes());
        }
        for sample in self.ring {
            out.extend_from_slice(&sample.to_le_bytes());
        }
        for stamp in self.day_stamp {
            out.extend_from_slice(&stamp.to_le_bytes());
        }
        for max in self.day_max {
            out.extend_from_slice(&max.to_le_bytes());
        }
        for cpu in self.cpu_ring {
            out.extend_from_slice(&cpu.to_le_bytes());
        }
        out.extend_from_slice(&checksum(&out).to_le_bytes());
        out
    }

    /// The summary stored in `bytes`, or `None` if they are not exactly one valid
    /// record: wrong length, unknown version or basis, bad checksum, or counters that
    /// contradict each other.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let bytes: &[u8; LEN] = bytes.try_into().ok()?;
        let mut reader = Reader { bytes, at: 0 };
        let version = reader.u8();
        let basis = match reader.u8() {
            1 => Basis::Cgroup,
            2 => Basis::MacFootprint,
            _ => return None,
        };
        let n_total = reader.u64();
        let cpu_n = reader.u64();
        let last_seen = Day(reader.u32());
        let oom_floor = reader.u64();
        let floor_anchor = reader.u64();
        let floor_day = Day(reader.u32());
        let last_kill = reader.u64();
        let raise_only_until = Day(reader.u32());
        let mut chunk_max = [0; SLOTS];
        let mut chunk_held = [0; SLOTS];
        for max in chunk_max.iter_mut().chain(&mut chunk_held) {
            *max = reader.u64();
        }
        let mut ring = [0; RING];
        for sample in &mut ring {
            *sample = reader.u64();
        }
        let mut day_stamp = [0; DAYS];
        for stamp in &mut day_stamp {
            *stamp = reader.u32();
        }
        let mut day_max = [0; DAYS];
        for max in &mut day_max {
            *max = reader.u64();
        }
        let mut cpu_ring = [0; RING];
        for cpu in &mut cpu_ring {
            *cpu = reader.u32();
        }
        let stored = reader.u64();
        let days = [last_seen, floor_day].iter().all(|day| day.0 <= MAX_DAY)
            && raise_only_until.0 <= MAX_DAY + RAISE_ONLY_DAYS
            && day_stamp.iter().all(|stamp| *stamp <= MAX_DAY);
        let sound = version == VERSION
            && days
            && n_total <= MAX_SAMPLES
            && cpu_n <= MAX_SAMPLES
            && stored == checksum(&bytes[..LEN - 8])
            && floor_anchor <= n_total
            && last_kill <= n_total;
        sound.then_some(Self {
            basis,
            n_total,
            chunk_max,
            chunk_held,
            ring,
            day_stamp,
            day_max,
            cpu_n,
            cpu_ring,
            oom_floor,
            floor_anchor,
            floor_day,
            last_kill_n: (last_kill != 0).then_some(last_kill),
            raise_only_until,
            last_seen,
        })
    }
}
