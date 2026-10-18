//! The `kbf.toml` that `setup` writes, as a typed value rendered to text.
//!
//! This is the minimal stand-in for the W2 schema (the loader and types of `kbf.toml` for every
//! mode): the keys are the ones the designs name (`[frontend] url`, `[enroll] token_file`,
//! `[update] source|url|min_commit|min_protocol`) plus a top-level `role` naming the `kbf` mode.
//! When W2's types land, replace [`KbfToml`] with them and keep [`KbfToml::render`]'s golden
//! test as the check that the file is unchanged. Every value comes from [`super::validate`],
//! whose character sets need no TOML escaping.

use super::validate::{Floor, ReleaseSource, Role};

/// The file, field by field.
pub struct KbfToml<'a> {
    pub role: Role,
    pub frontend_url: &'a str,
    pub token_file: &'a str,
    pub source: &'a ReleaseSource,
    pub floor: &'a Floor,
}

impl KbfToml<'_> {
    pub fn render(&self) -> String {
        let mut out = String::from(
            "# Written by `kbf-installer setup`; run it again to change this file.\n",
        );
        out.push_str(&format!("role = \"{}\"\n\n", self.role));
        out.push_str(&format!("[frontend]\nurl = \"{}\"\n\n", self.frontend_url));
        out.push_str(&format!("[enroll]\ntoken_file = \"{}\"\n\n", self.token_file));
        out.push_str("[update]\n");
        match self.source {
            ReleaseSource::Frontend => out.push_str("source = \"frontend\"\n"),
            ReleaseSource::Url(url) => {
                out.push_str(&format!("source = \"url\"\nurl = \"{url}\"\n"));
            }
        }
        if let Some(commit) = &self.floor.min_commit {
            out.push_str(&format!("min_commit = \"{commit}\"\n"));
        }
        if let Some(protocol) = self.floor.min_protocol {
            out.push_str(&format!("min_protocol = {protocol}\n"));
        }
        out
    }
}
