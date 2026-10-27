//! What the simulator probe takes from a command's output: the device id `simctl create`
//! prints and the bundle identifier `plutil` prints. [`crate::sim`] re-exports them.

/// The most bytes of a command's output an error note repeats; the rest is cut and the note
/// ends with [`CUT_MARK`]. A note rides in a step frame, which must stay under
/// [`MAX_BODY_BYTES`](crate::frame::MAX_BODY_BYTES) whatever the command printed.
pub const MAX_ECHO_BYTES: usize = 512;
/// The longest bundle identifier accepted, in bytes.
pub const MAX_BUNDLE_ID_BYTES: usize = 255;
/// What replaces the part of echoed output that was cut.
pub const CUT_MARK: &str = "...[cut]";

/// `text` cut to at most [`MAX_ECHO_BYTES`] bytes (at a character boundary), with
/// [`CUT_MARK`] after what is kept when something was cut.
pub(crate) fn echo(text: &str) -> String {
    if text.len() <= MAX_ECHO_BYTES {
        return text.to_owned();
    }
    let mut end = MAX_ECHO_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{CUT_MARK}", &text[..end])
}

/// The UDID `simctl create` printed: its last non-empty line, as 8-4-4-4-12 hex digits.
///
/// # Errors
/// There is no such line. The error repeats at most [`MAX_ECHO_BYTES`] bytes of it.
pub fn parse_udid(stdout: &str) -> Result<String, String> {
    let line = stdout
        .lines()
        .map(str::trim)
        .rfind(|l| !l.is_empty())
        .ok_or("simctl create printed nothing")?;
    let groups: Vec<&str> = line.split('-').collect();
    let shape = [8usize, 4, 4, 4, 12];
    let ok = groups.len() == shape.len()
        && groups
            .iter()
            .zip(shape)
            .all(|(g, n)| g.len() == n && g.bytes().all(|b| b.is_ascii_hexdigit()));
    if ok {
        Ok(line.to_owned())
    } else {
        Err(format!("not a device id: {:?}", echo(line)))
    }
}

/// The bundle identifier `plutil -extract ... raw` printed.
///
/// # Errors
/// It is empty, longer than [`MAX_BUNDLE_ID_BYTES`], or holds anything but letters, digits,
/// `.`, `-` and `_` (the id is put in the commands that install and launch the app, and in
/// a frame's note). The error repeats at most [`MAX_ECHO_BYTES`] bytes of it.
pub fn parse_bundle_id(stdout: &str) -> Result<String, String> {
    let id = stdout.trim();
    let ok = !id.is_empty()
        && id.len() <= MAX_BUNDLE_ID_BYTES
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'));
    if !ok {
        Err(format!("not a bundle identifier: {:?}", echo(id)))
    } else {
        Ok(id.to_owned())
    }
}
