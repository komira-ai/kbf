//! Compiles `kbf.invocation.v1` into Rust messages with `protox` and `prost-build` (no
//! `protoc` binary, and no gRPC service: the record is stored, never served as RPC).
//! Nothing generated is checked in; the code lands in `OUT_DIR`.

use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?).join("proto");
    let descriptors = protox::compile(["kbf/invocation/v1/invocation.proto"], [root.clone()])?;
    prost_build::Config::new().compile_fds(descriptors)?;
    println!("cargo:rerun-if-changed={}", root.display());
    Ok(())
}
