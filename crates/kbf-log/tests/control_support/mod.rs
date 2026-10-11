//! Values and helpers the control-command codec tests share.

#![allow(dead_code, reason = "each test file uses its own subset")]

use std::collections::BTreeSet;
use std::time::{Duration, SystemTime};

use kbf_caps::{Arch, Consumable, CpuCaps, NodeCaps};
use kbf_log::{DecodeError, FarmBatch, FarmInput, Format, LogCommand, decode, encode};
use kbf_types::{Digest, DigestFunction, FarmTime, LeaseId, Resources, WorkerId};

pub fn d(n: u8) -> Digest {
    Digest::new(DigestFunction::Sha256, [n; 32], u64::from(n))
}

/// The bytes of a `Digest` message for `d(n)`, n < 128: function 1 (`08 01`), the
/// 32-byte hash (`12 20 ..`), the size (`18 n`); 38 (0x26) bytes in all.
pub fn digest_hex(n: u8) -> String {
    format!("08011220{}18{n:02x}", format!("{n:02x}").repeat(32))
}

pub fn w(name: &str) -> WorkerId {
    WorkerId::new(name)
}

pub const fn lease(term: u64, seq: u64) -> LeaseId {
    LeaseId::new(term, seq)
}

pub fn wall(nanos: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_nanos(nanos)
}

/// A batch at farm time 1 and wall time 2 ns holding `input` alone.
pub fn batch(input: FarmInput) -> LogCommand {
    LogCommand::Farm(FarmBatch {
        now: FarmTime::from_millis(1),
        wall: wall(2),
        inputs: vec![input],
    })
}

/// CPU 1, memory 2, GPUs 3, VM slots 4.
pub const fn resources() -> Resources {
    Resources::new(1, 2).with_gpus(3).with_vms(4)
}

/// An arm64 node with feature `a`, exact `k=v`, the set `m={x}`, two GPUs and
/// driver `n`.
pub fn caps() -> NodeCaps {
    let mut caps = NodeCaps::new(CpuCaps::new(Arch::Arm64, BTreeSet::from(["a".to_owned()])))
        .with_drivers(["n"]);
    caps.exact.insert("k".to_owned(), "v".to_owned());
    caps.members
        .insert("m".to_owned(), BTreeSet::from(["x".to_owned()]));
    caps.consumables.insert(Consumable::Gpus, 2);
    caps
}

/// `caps()` as its `NodeCaps` message bytes (32, 0x20): cpu (0a) { arch 2, features
/// "a" }, exact (12) { "k", "v" }, members (1a) { "m", ["x"] }, consumables (22)
/// { 4, 2 }, drivers (2a) "n".
pub const CAPS_HEX: &str = concat!(
    "0a05", "0802", "120161", "1206", "0a016b", "120176", "1a06", "0a016d", "120178", "2204",
    "0804", "1002", "2a016e"
);

/// `resources()` as its `Resources` message bytes (8).
pub const RESOURCES_HEX: &str = concat!("0801", "1002", "1803", "2004");

pub fn enc(command: &LogCommand) -> Vec<u8> {
    encode(Format::V1, command).expect("encode")
}

pub fn dec(bytes: &[u8]) -> Result<LogCommand, DecodeError> {
    decode(Format::V1, bytes)
}
