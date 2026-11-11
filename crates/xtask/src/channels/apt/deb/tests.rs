use super::*;
use crate::channels::apt::testing::deb;

fn package(version: &str) -> Vec<u8> {
    let files = deb::files(b"binary", b"notice");
    deb::build(version, "amd64", &files)
}

#[test]
fn the_control_file_is_read_back_out_of_the_package() {
    // Catches: an index that lists what the builder meant instead of what the package's bytes say.
    let bytes = package("1.2.3");
    let files = deb::files(b"binary", b"notice");
    assert_eq!(
        control_of(&bytes).expect("control"),
        deb::control("1.2.3", "amd64", &files)
    );
}

/// An `ar` archive of `members`, in the form `release deb` writes.
fn ar(members: &[(&str, &[u8])]) -> Vec<u8> {
    deb::ar(members)
}

fn why(bytes: &[u8]) -> String {
    control_of(bytes).unwrap_err().to_string()
}

#[test]
fn bytes_that_are_not_a_package_are_refused_with_the_reason() {
    // Catches: a truncated or foreign file listed in the index.
    assert!(why(b"hello").contains("no ar magic"));
    let good = package("1.2.3");
    assert!(why(&good[..8 + 30]).contains("a cut header"));
    assert!(why(&good[..8 + 60 + 2]).contains("a member cut short"));
    let mut marked = good.clone();
    marked[8 + 58] = b'x';
    assert!(why(&marked).contains("without its end mark"));
    let mut sized = good.clone();
    sized[8 + 48..8 + 58].copy_from_slice(b"notanumber");
    assert!(why(&sized).contains("not a number"));
    assert!(why(&ar(&[("debian-binary", b"2.0\n")])).contains("no control.tar.gz"));
    assert!(why(&ar(&[("control.tar.gz", b"not gzip")])).contains("not a tar"));
    assert!(why(&ar(&[("control.tar.gz", b"")])).contains("not a tar"));
}

#[test]
fn a_control_tar_without_control_or_with_a_binary_control_is_refused() {
    // Catches: a package whose control.tar.gz has other files only, or a control that is not text.
    let tar_gz = |name: &str, content: &[u8]| {
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, name, content)
            .expect("append");
        let tar = builder.into_inner().expect("tar");
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut gz, &tar).expect("gz");
        gz.finish().expect("finish")
    };
    let other = tar_gz("md5sums", b"x");
    assert!(why(&ar(&[("control.tar.gz", &other)])).contains("no control in"));
    let binary = tar_gz("./control", &[0xff, 0xfe]);
    assert!(why(&ar(&[("control.tar.gz", &binary)])).contains("not UTF-8"));
    // The name `ar` writes with a trailing slash is the same member.
    let text = tar_gz("control", b"Package: x\n");
    assert_eq!(
        control_of(&ar(&[("control.tar.gz/", &text)])).expect("control"),
        "Package: x\n"
    );
}
