//! Helpers the control-command codec tests share.

use kbf_log::{DecodeError, Format, LogCommand, decode, encode};

pub fn enc(command: &LogCommand) -> Vec<u8> {
    encode(Format::V1, command).expect("encode")
}

pub fn dec(bytes: &[u8]) -> Result<LogCommand, DecodeError> {
    decode(Format::V1, bytes)
}
