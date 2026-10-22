//! Embeds what `kbf --version --json` says of this build (`src/version.rs`) and the server
//! crate does not: the Cargo profile as `KBF_BUILD_PROFILE` (`debug` or `release`, as
//! Cargo's `PROFILE` says), so it can say whether a binary is a release build. The commit
//! count `seq` comes from `kbf-server` ([`kbf_server::BUILD_SEQ`]).

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let profile = std::env::var("PROFILE").expect("Cargo sets PROFILE for a build script");
    println!("cargo:rustc-env=KBF_BUILD_PROFILE={profile}");
}
