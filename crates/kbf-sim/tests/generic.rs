//! The kernel depends on the scheduler's vocabulary and on nothing the build adds to it.

/// The names the kernel must not use: the build's content digest, its hash function, its
/// action key, and the protocol they come from.
const BUILD_NAMES: [&str; 4] = ["Digest", "DigestFunction", "ActionKey", "REAPI"];

/// The lines of `source` that are not comments and name a build type. The `sha2` crate's
/// own `Digest` trait, which the trace hash uses, is not one.
fn offenders(source: &str) -> Vec<&str> {
    source
        .lines()
        .map(str::trim)
        .filter(|line| !line.starts_with("//"))
        .filter(|line| !line.contains("sha2::"))
        .filter(|line| BUILD_NAMES.iter().any(|name| line.contains(name)))
        .collect()
}

/// The names of the entries of the `[dependencies]` table of a manifest.
fn dependencies(manifest: &str) -> Vec<&str> {
    manifest
        .lines()
        .map(str::trim)
        .skip_while(|line| *line != "[dependencies]")
        .skip(1)
        .take_while(|line| !line.starts_with('['))
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| line.split(['.', ' ', '=']).next())
        .collect()
}

/// Catches: a build type (a digest, an action key) creeping back into a file of the
/// kernel, which would tie it to the build and keep it from moving to a repository
/// that has none.
#[test]
fn the_kernel_names_no_build_type() {
    let files = [
        ("src/golden.rs", include_str!("../src/golden.rs")),
        ("src/lib.rs", include_str!("../src/lib.rs")),
        ("src/net.rs", include_str!("../src/net.rs")),
        ("src/node.rs", include_str!("../src/node.rs")),
        ("src/rng.rs", include_str!("../src/rng.rs")),
        ("src/sim.rs", include_str!("../src/sim.rs")),
        ("src/text.rs", include_str!("../src/text.rs")),
        ("src/trace.rs", include_str!("../src/trace.rs")),
        ("tests/bus.rs", include_str!("bus.rs")),
        ("tests/determinism.rs", include_str!("determinism.rs")),
        ("tests/toy_counter.rs", include_str!("toy_counter.rs")),
    ];
    for (file, source) in files {
        assert_eq!(offenders(source), Vec::<&str>::new(), "{file}");
    }
}

/// Catches: the scan above passing because it cannot see a name: it must flag a build
/// type in code, skip it in a comment, and skip the hash crate's `Digest` trait.
#[test]
fn the_scan_sees_a_build_name_in_code_and_skips_comments() {
    let source = "// the Digest\nuse sha2::{Digest, Sha256};\nlet a: ActionKey;\nlet d: Digest;\n";
    assert_eq!(
        offenders(source),
        vec!["let a: ActionKey;", "let d: Digest;"]
    );
}

/// Catches: a second workspace crate in the kernel's dependencies, which would make it
/// depend on more than the scheduler's vocabulary.
#[test]
fn the_kernel_depends_on_no_crate_of_the_workspace_but_kbf_types() {
    let manifest = include_str!("../Cargo.toml");
    let names = dependencies(manifest);
    assert_eq!(names, vec!["kbf-types", "rand_chacha", "sha2"]);
}

/// Catches: the manifest reader returning nothing and the test above comparing against
/// the wrong thing; it must stop at the next table and skip comments.
#[test]
fn the_manifest_reader_reads_one_table() {
    let manifest = "[package]\nname = \"x\"\n\n[dependencies]\n# why\na.workspace = true\nb = { version = \"1\" }\n\n[dev-dependencies]\nc.workspace = true\n";
    assert_eq!(dependencies(manifest), vec!["a", "b"]);
}
