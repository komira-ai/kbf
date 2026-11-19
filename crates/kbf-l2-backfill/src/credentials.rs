//! The key pair of the source store: a JSON file, never a flag (a command line is
//! world-readable), that only its owner may be able to read.

use std::fs::File;
use std::io::Read as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;

use kbf_objstore::s3::Credentials;
use zeroize::Zeroizing;

/// What a file that is not JSON of the key pair is told.
const NOT_KEYS: &str = "not JSON with access_key_id and secret_access_key";

/// What the file holds.
#[derive(serde::Deserialize)]
struct Keys {
    access_key_id: String,
    secret_access_key: String,
}

/// The key pair in the file at `path`: JSON with `access_key_id` and `secret_access_key`, in a
/// file with no permission for group or others.
///
/// # Errors
/// One line saying what is wrong with the file; it never holds a part of the secret.
pub fn read(path: &Path) -> Result<Credentials, String> {
    let read = File::open(path).and_then(|mut file| {
        file.metadata().and_then(|meta| {
            let mut text = String::new();
            let read = file.read_to_string(&mut text);
            read.map(|_| (meta.permissions().mode(), text))
        })
    });
    let (mode, text) = read.map_err(|error| error.to_string())?;
    let text = Zeroizing::new(text);
    if mode & 0o077 != 0 {
        let shown = mode & 0o777;
        let why = format!("mode {shown:04o} lets others read a secret: use 0600");
        return Err(why);
    }
    let parsed = serde_json::from_str::<Keys>(&text);
    let keys = parsed.map_err(|_| NOT_KEYS.to_owned())?;
    Ok(Credentials::new(keys.access_key_id, keys.secret_access_key))
}
