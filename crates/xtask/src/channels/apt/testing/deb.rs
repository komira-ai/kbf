//! A Debian package for the tests of the apt verbs, in the form `release deb` writes: an `ar`
//! archive of `debian-binary`, `control.tar.gz` and `data.tar.gz`. It holds what the verbs read
//! (the control file) and a little data, and is built the same way for the same inputs.

use flate2::Compression;

/// One file of the package: its path from the root with no leading slash, mode and content.
pub type File = (String, u32, Vec<u8>);

/// The files of a package of `binary`, with `notice` as its copyright file.
pub fn files(binary: &[u8], notice: &[u8]) -> Vec<File> {
    vec![
        ("usr/bin/kbf".to_owned(), 0o755, binary.to_vec()),
        (
            "usr/share/doc/kbf/copyright".to_owned(),
            0o644,
            notice.to_vec(),
        ),
    ]
}

/// The `control` file of `version` for `architecture` holding `files`.
pub fn control(version: &str, architecture: &str, files: &[File]) -> String {
    control_for("kbf", version, architecture, files)
}

/// The `control` file of the package `package`.
pub fn control_for(package: &str, version: &str, architecture: &str, files: &[File]) -> String {
    let bytes: usize = files.iter().map(|(_, _, content)| content.len()).sum();
    format!(
        "Package: {package}\nVersion: {version}\nArchitecture: {architecture}\n\
         Maintainer: komira-ai <noreply@komira.com>\nInstalled-Size: {}\nSection: admin\n\
         Priority: optional\nHomepage: https://github.com/komira-ai/kbf\n\
         Description: a synopsis\n a longer description\n .\n and its last line\n",
        bytes.div_ceil(1024)
    )
}

/// The gzip-compressed tar of `entries` `(path, mode, content)`.
fn tar_gz(entries: &[(String, u32, Vec<u8>)]) -> Vec<u8> {
    let gzip = flate2::GzBuilder::new()
        .mtime(0)
        .write(Vec::new(), Compression::best());
    let mut tar = tar::Builder::new(gzip);
    for (path, mode, content) in entries {
        let mut header = tar::Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_mode(*mode);
        header.set_cksum();
        tar.append_data(&mut header, path, content.as_slice())
            .expect("append");
    }
    tar.into_inner()
        .and_then(flate2::write::GzEncoder::finish)
        .expect("finish")
}

/// The `ar` archive of the members `(name, content)`, in order, each padded to an even length.
pub fn ar(members: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = b"!<arch>\n".to_vec();
    for (name, content) in members {
        out.extend_from_slice(
            format!(
                "{name:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n",
                0,
                0,
                0,
                "100644",
                content.len()
            )
            .as_bytes(),
        );
        out.extend_from_slice(content);
        if content.len() % 2 == 1 {
            out.push(b'\n');
        }
    }
    out
}

/// The package file of `version` for `architecture` holding `files`.
pub fn build(version: &str, architecture: &str, files: &[File]) -> Vec<u8> {
    build_for("kbf", version, architecture, files)
}

/// The package file of the package `package`.
pub fn build_for(package: &str, version: &str, architecture: &str, files: &[File]) -> Vec<u8> {
    let control_tar = tar_gz(&[(
        "./control".to_owned(),
        0o644,
        control_for(package, version, architecture, files).into_bytes(),
    )]);
    let data: Vec<(String, u32, Vec<u8>)> = files
        .iter()
        .map(|(path, mode, content)| (format!("./{path}"), *mode, content.clone()))
        .collect();
    ar(&[
        ("debian-binary", b"2.0\n"),
        ("control.tar.gz", &control_tar),
        ("data.tar.gz", &tar_gz(&data)),
    ])
}
