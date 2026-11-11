//! `--job-store-grant-key-file`, `--job-store-url` and `--job-store-bucket`, removed.
//!
//! The three had `GET /locate` sign tierstore read grants, so that a task read packs and large
//! objects from the store directly. tierstore v0.3.29 removed the read grant (and the store's
//! `--grant-key-file` that checked it), so there is nothing left for them to configure: the answer
//! of `GET /locate` carries no grants and a task reads every blob from the job-facing listener.
//!
//! A start that still gives one of them, as a flag or as its `kbf.toml` key, is refused at parse
//! time with a line that names the flag, the tierstore release and the way out, not with clap's
//! general unknown-argument line. They are hidden from `--help`.

use clap::Arg;
use clap::builder::ValueParser;

/// The flags, by long name.
pub(super) const REMOVED: [&str; 3] = [
    "job-store-grant-key-file",
    "job-store-url",
    "job-store-bucket",
];

/// What a start that gives one of the flags is told, after clap's `invalid value ... for
/// '--<flag> <VALUE>'`, which names the flag.
pub(super) const REMOVED_WHY: &str = "the flag was removed: tierstore v0.3.29 removed the store \
     read grants it configured, so `GET /locate` hands out none and a task reads every blob from \
     the job listener; delete the flag";

/// The three flags, each refusing any value with [`REMOVED_WHY`].
pub(super) fn args() -> [Arg; 3] {
    REMOVED.map(|long| {
        Arg::new(long)
            .long(long)
            .hide(true)
            .value_name("VALUE")
            .help(
                "Removed: tierstore v0.3.29 removed the store read grants this configured. Any \
                 value is refused at start",
            )
            .value_parser(ValueParser::new(refuse))
    })
}

/// The parser of a removed flag: it takes no value.
fn refuse(_: &str) -> Result<String, &'static str> {
    Err(REMOVED_WHY)
}
