//! The toolchain pins and the unpacking of a component tarball.

use super::*;

/// A `.tar.xz` of `members`: `(path, kind, mode, bytes or link target)`.
fn tar_xz(members: &[(&str, tar::EntryType, u32, &[u8])]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for (path, kind, mode, data) in members {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(*kind);
        header.set_mode(*mode);
        if kind.is_symlink() {
            header.set_size(0);
            header
                .set_link_name(std::str::from_utf8(data).expect("utf-8"))
                .expect("link");
            builder
                .append_data(&mut header, path, &[][..])
                .expect("append");
        } else {
            header.set_size(data.len() as u64);
            builder
                .append_data(&mut header, path, *data)
                .expect("append");
        }
    }
    let tar = builder.into_inner().expect("tar");
    let mut xz = Vec::new();
    lzma_rs::xz_compress(&mut tar.as_slice(), &mut xz).expect("xz");
    xz
}

fn pin(name: &str, dir: &str, bytes: &[u8]) -> Pin {
    Pin {
        name: name.to_owned(),
        url: format!("https://example.invalid/{name}.tar.xz"),
        sha256: Sha256::of(bytes).to_string(),
        dir: dir.to_owned(),
    }
}

fn one(pin: Pin) -> Pins {
    Pins {
        rust: "1.0.0".to_owned(),
        component: vec![pin],
    }
}

/// Catches a pins file that does not parse, a component pinned to plain HTTP, and the
/// toolchain drifting from the one every other job builds with: the pinned release is
/// rust-toolchain.toml's channel, and every component's URL names it.
#[test]
fn the_pins_parse_and_name_the_release_rust_toolchain_toml_pins() {
    let pins = parse(PINS).expect("the pins");
    let file = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../rust-toolchain.toml"),
    )
    .expect("rust-toolchain.toml");
    let channel: toml::Value = toml::from_str(&file).expect("toml");
    assert_eq!(
        channel["toolchain"]["channel"].as_str(),
        Some(pins.rust.as_str())
    );
    let names: Vec<&str> = pins.component.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["rustc", "cargo", "rust-std"]);
    for c in &pins.component {
        assert!(
            c.url.ends_with(&format!(
                "{}-{}-aarch64-apple-darwin.tar.xz",
                c.name, pins.rust
            )),
            "{}",
            c.url
        );
    }
}

/// Catches pins that are not TOML, an unknown key taken silently, an http URL, and a SHA-256
/// that is not 64 lowercase hex digits.
#[test]
fn pins_that_are_wrong_are_refused() {
    assert!(matches!(parse("rust = "), Err(ToolchainError::Pins(_))));
    assert!(matches!(
        parse("rust = \"1\"\ncomponent = []\nextra = 1\n"),
        Err(ToolchainError::Pins(_))
    ));
    let component = |url: &str, sha: &str| {
        format!(
            "rust = \"1\"\n[[component]]\nname = \"c\"\nurl = \"{url}\"\nsha256 = \"{sha}\"\ndir = \"c\"\n"
        )
    };
    let good = "a".repeat(64);
    assert_eq!(
        parse(&component("http://x/c.tar.xz", &good)),
        Err(ToolchainError::Pin {
            name: "c".to_owned()
        })
    );
    assert_eq!(
        parse(&component("https://x/c.tar.xz", &"A".repeat(64))),
        Err(ToolchainError::Pin {
            name: "c".to_owned()
        })
    );
    assert!(parse(&component("https://x/c.tar.xz", &good)).is_ok());
}

/// Catches a member outside the component's `bin`, `lib` and `libexec` sent (its docs, the
/// tarball's own files, another component's directory), a kept member at the wrong path, an
/// executable bit lost, and a symlink not kept as one.
#[test]
fn only_the_components_bin_lib_and_libexec_are_entries_below_the_prefix() {
    let file = tar::EntryType::Regular;
    let xz = tar_xz(&[
        ("rustc-1/components", file, 0o644, b"rustc"),
        ("rustc-1/rustc/bin/rustc", file, 0o755, b"exe"),
        ("rustc-1/rustc/lib/", tar::EntryType::Directory, 0o755, b""),
        ("rustc-1/rustc/lib/libdriver.dylib", file, 0o644, b"lib"),
        (
            "rustc-1/rustc/lib/liblink.dylib",
            tar::EntryType::Symlink,
            0o777,
            b"libdriver.dylib",
        ),
        ("rustc-1/rustc/libexec/helper", file, 0o700, b"h"),
        ("rustc-1/rustc/share/doc/x", file, 0o644, b"doc"),
        ("rustc-1/other/bin/tool", file, 0o755, b"other"),
    ]);
    let pins = one(pin("rustc", "rustc", &xz));
    let mut download = |_: &str| Ok(xz.clone());
    let got = entries(&pins, "tc", &mut download).expect("entries");
    assert_eq!(
        got,
        vec![
            (
                "tc/bin/rustc".to_owned(),
                Entry::File {
                    bytes: b"exe".to_vec(),
                    executable: true
                }
            ),
            (
                "tc/lib/libdriver.dylib".to_owned(),
                Entry::File {
                    bytes: b"lib".to_vec(),
                    executable: false
                }
            ),
            (
                "tc/lib/liblink.dylib".to_owned(),
                Entry::Symlink("libdriver.dylib".to_owned())
            ),
            (
                "tc/libexec/helper".to_owned(),
                Entry::File {
                    bytes: b"h".to_vec(),
                    executable: true
                }
            ),
        ]
    );
}

/// Catches a download whose bytes are not the pin's taken anyway, and a failed download not
/// named by its component.
#[test]
fn a_download_that_is_not_its_pin_is_refused() {
    let xz = tar_xz(&[("c-1/c/bin/x", tar::EntryType::Regular, 0o755, b"x")]);
    let pins = one(pin("c", "c", b"other bytes"));
    let mut download = |_: &str| Ok(xz.clone());
    assert_eq!(
        entries(&pins, "tc", &mut download),
        Err(ToolchainError::Sha256 {
            name: "c".to_owned(),
            got: Sha256::of(&xz).to_string(),
            want: Sha256::of(b"other bytes").to_string(),
        })
    );
    let mut fails = |url: &str| Err(format!("no route to {url}"));
    assert_eq!(
        entries(&pins, "tc", &mut fails),
        Err(ToolchainError::Download {
            name: "c".to_owned(),
            why: "no route to https://example.invalid/c.tar.xz".to_owned(),
        })
    );
}

/// Catches bytes that are not xz, or xz that is not a tar, read as an empty component.
#[test]
fn a_tarball_that_does_not_unpack_is_refused() {
    let not_xz = b"plain".to_vec();
    let pins = one(pin("c", "c", &not_xz));
    let mut download = |_: &str| Ok(not_xz.clone());
    assert!(matches!(
        entries(&pins, "tc", &mut download),
        Err(ToolchainError::Unpack { .. })
    ));
    let mut not_tar = Vec::new();
    lzma_rs::xz_compress(&mut &[1_u8; 700][..], &mut not_tar).expect("xz");
    let pins = one(pin("c", "c", &not_tar));
    let mut download = |_: &str| Ok(not_tar.clone());
    assert!(matches!(
        entries(&pins, "tc", &mut download),
        Err(ToolchainError::Unpack { .. })
    ));
}

/// Catches a component whose directory name is wrong taken as an empty toolchain, and a kept
/// member that is neither a file nor a symlink (a hard link) sent as something else.
#[test]
fn an_empty_component_and_an_odd_member_are_refused() {
    let xz = tar_xz(&[("c-1/c/bin/x", tar::EntryType::Regular, 0o755, b"x")]);
    let pins = one(pin("c", "not-c", &xz));
    let mut download = |_: &str| Ok(xz.clone());
    assert_eq!(
        entries(&pins, "tc", &mut download),
        Err(ToolchainError::Empty {
            name: "c".to_owned(),
            dir: "not-c".to_owned(),
        })
    );
    let xz = tar_xz(&[("c-1/c/bin/x", tar::EntryType::Link, 0o755, b"c-1/c/bin/y")]);
    let pins = one(pin("c", "c", &xz));
    let mut download = |_: &str| Ok(xz.clone());
    assert_eq!(
        entries(&pins, "tc", &mut download),
        Err(ToolchainError::Kind {
            name: "c".to_owned(),
            path: "c-1/c/bin/x".to_owned(),
        })
    );
}
