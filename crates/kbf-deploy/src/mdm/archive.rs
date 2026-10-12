//! One member of a pinned archive, read in memory: a zip (deflate or stored) or a
//! gzip-compressed tar. Nothing is unpacked to disk, so no member name can place a file.

use std::io::{Cursor, Read};

use flate2::read::GzDecoder;

use super::pins::Packing;

/// Why a member could not be read.
#[derive(Debug, thiserror::Error)]
pub enum ArchiveError {
    #[error("{archive} is not a readable {packing:?} archive: {why}")]
    Unreadable {
        archive: String,
        packing: Packing,
        why: String,
    },
    #[error("{archive} has no {member}, or it is empty")]
    NoMember { archive: String, member: String },
}

/// The bytes of `member` in `bytes`, an archive called `archive` (for errors).
pub fn member(
    archive: &str,
    packing: Packing,
    bytes: &[u8],
    member: &str,
) -> Result<Vec<u8>, ArchiveError> {
    let unreadable = |why: String| ArchiveError::Unreadable {
        archive: archive.to_owned(),
        packing,
        why,
    };
    let found = match packing {
        Packing::Zip => zip_member(bytes, member),
        Packing::TarGz => tar_member(bytes, member),
    }
    .map_err(unreadable)?;
    found
        .filter(|b| !b.is_empty())
        .ok_or(ArchiveError::NoMember {
            archive: archive.to_owned(),
            member: member.to_owned(),
        })
}

fn zip_member(bytes: &[u8], member: &str) -> Result<Option<Vec<u8>>, String> {
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).map_err(why)?;
    let Ok(mut file) = zip.by_name(member) else {
        return Ok(None);
    };
    let mut out = Vec::new();
    file.read_to_end(&mut out).map_err(why)?;
    Ok(Some(out))
}

/// An archive library's error, as text.
fn why(e: impl std::fmt::Display) -> String {
    e.to_string()
}

fn tar_member(bytes: &[u8], member: &str) -> Result<Option<Vec<u8>>, String> {
    let mut tar = tar::Archive::new(GzDecoder::new(bytes));
    for entry in tar.entries().map_err(why)? {
        let mut entry = entry.map_err(why)?;
        let path = entry.path().map_err(why)?;
        if path.to_str() == Some(member) {
            let mut out = Vec::new();
            entry.read_to_end(&mut out).map_err(why)?;
            return Ok(Some(out));
        }
    }
    Ok(None)
}

#[cfg(test)]
pub(crate) mod tests {
    use std::io::Write;

    use super::*;

    /// A zip of `(name, bytes)` members, deflated.
    pub(crate) fn zip_of(members: &[(&str, &[u8])]) -> Vec<u8> {
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for (name, bytes) in members {
            w.start_file(*name, opts).expect("start");
            w.write_all(bytes).expect("write");
        }
        w.finish().expect("finish").into_inner()
    }

    /// A gzip-compressed tar of `(name, bytes)` members.
    pub(crate) fn tar_gz_of(members: &[(&str, &[u8])]) -> Vec<u8> {
        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        let mut b = tar::Builder::new(gz);
        for (name, bytes) in members {
            let mut h = tar::Header::new_gnu();
            h.set_size(bytes.len() as u64);
            h.set_mode(0o755);
            h.set_cksum();
            b.append_data(&mut h, name, *bytes).expect("append");
        }
        b.into_inner().expect("tar").finish().expect("gzip")
    }

    /// Catches a reader that takes the first member instead of the named one, that
    /// installs an empty member, or that reads a member of the wrong packing.
    #[test]
    fn the_named_member_is_read_from_either_packing() {
        let files: [(&str, &[u8]); 2] = [("README.md", b"docs"), ("dir/caddy", b"\x7fELF")];
        for (packing, bytes) in [
            (Packing::Zip, zip_of(&files)),
            (Packing::TarGz, tar_gz_of(&files)),
        ] {
            let got = member("a", packing, &bytes, "dir/caddy").expect("there");
            assert_eq!(got, b"\x7fELF", "{packing:?}");
            let gone = member("a", packing, &bytes, "caddy")
                .err()
                .map(|e| e.to_string());
            assert_eq!(gone.as_deref(), Some("a has no caddy, or it is empty"));
        }
        let empty = zip_of(&[("caddy", b"")]);
        assert!(matches!(
            member("a", Packing::Zip, &empty, "caddy"),
            Err(ArchiveError::NoMember { .. })
        ));
    }

    /// Catches a damaged or mislabelled archive read as one with no such member (the
    /// message would send the operator after the wrong fault).
    #[test]
    fn a_damaged_archive_is_unreadable() {
        let zip = zip_of(&[("caddy", b"x")]);
        let tar = tar_gz_of(&[("caddy", b"x")]);
        let mut cut = tar.clone();
        cut.truncate(cut.len() / 2);
        let cases = [
            (Packing::Zip, b"not a zip".to_vec()),
            (Packing::TarGz, zip),
            (Packing::TarGz, cut),
        ];
        for (packing, bytes) in cases {
            let got = member("a", packing, &bytes, "caddy");
            assert!(
                matches!(got, Err(ArchiveError::Unreadable { .. })),
                "{packing:?}: {got:?}"
            );
        }
    }
}
