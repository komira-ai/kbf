//! The token file of a channel that fetches from GitHub: root's alone to write, one line.

use std::path::Path;

use crate::config::refusal;
use crate::outcome::Refusal;
use crate::trust::Trust;

/// The most bytes a token file may hold: a GitHub Actions token is about 520 bytes, with dots.
const MAX_TOKEN_BYTES: usize = 1024;

/// The mark that keeps [`crate::LONG_TOKEN_MARKER`] in the binary: the update rehearsal reads it
/// from the release to know whether to give it the run's token.
#[used]
static LONG_TOKEN_MARKER: &[u8] = crate::LONG_TOKEN_MARKER;

/// The token for the API in `file`, which is root's alone to write: one line of letters,
/// digits, `_`, `-` and `.`, at most [`MAX_TOKEN_BYTES`] bytes.
pub(crate) fn read_token(trust: &Trust, file: &Path) -> Result<String, Refusal> {
    let text = trust.read(file).map_err(refusal)?;
    let token = text.trim();
    let plain = !token.is_empty()
        && token.len() <= MAX_TOKEN_BYTES
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'));
    if plain {
        Ok(token.to_owned())
    } else {
        Err(Refusal::ConfigInvalid)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    use kbf_testkit::dir::TestDir;

    use super::*;

    fn read(name: &str, text: &str) -> Result<String, Refusal> {
        let dir = TestDir::under(std::env::temp_dir(), name);
        fs::set_permissions(&*dir, fs::Permissions::from_mode(0o755)).unwrap();
        let trust = Trust::owned_by(fs::metadata(&*dir).unwrap().uid(), &dir);
        let file = dir.join("github.token");
        fs::write(&file, text).unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
        read_token(&trust, &file)
    }

    /// A token file holds one line of letters, digits, `_`, `-` and `.`: the line is taken
    /// without its end, and a long workflow token of about 520 bytes is taken whole.
    #[test]
    fn a_token_is_one_plain_line_of_up_to_1024_bytes() {
        assert_eq!(
            read("token-ok", "ghp_exampleToken-0\n").as_deref(),
            Ok("ghp_exampleToken-0")
        );
        let long = format!(
            "ghs_123456_{}.{}.{}",
            "aB3-_".repeat(30),
            "xY9".repeat(60),
            "z_".repeat(60)
        );
        assert!((256..=1024).contains(&long.len()), "{}", long.len());
        assert_eq!(read("token-long", &format!("{long}\n")), Ok(long));
        assert_eq!(
            read("token-max", &"a".repeat(MAX_TOKEN_BYTES)).map(|t| t.len()),
            Ok(MAX_TOKEN_BYTES)
        );
    }

    /// A token that is not one line of those bytes, or is past 1024 bytes, is a bad
    /// configuration.
    #[test]
    fn a_token_that_is_not_plain_is_a_bad_configuration() {
        for bad in [
            "".to_owned(),
            "two words".to_owned(),
            "tok\u{7}en".to_owned(),
            "ghs_a/b".to_owned(),
            "ghs_a=b".to_owned(),
            "a".repeat(MAX_TOKEN_BYTES + 1),
        ] {
            assert_eq!(
                read("token-bad", &bad),
                Err(Refusal::ConfigInvalid),
                "{} bytes",
                bad.len()
            );
        }
    }
}
