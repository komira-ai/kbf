//! `linux render-units`: the converged storage node's four systemd files from the
//! templates in `templates/linux/` and a [`LinuxConfig`].

use super::config::LinuxConfig;
use super::shared::{SLICE, values};
use crate::template::{self, TemplateError};

/// The four templates, by role.
#[derive(Clone, Copy, Debug)]
pub struct Templates<'a> {
    pub slice: &'a str,
    pub minio: &'a str,
    pub server: &'a str,
    pub daemon: &'a str,
}

impl Templates<'static> {
    /// The templates compiled into the binary.
    pub const BUILT_IN: Templates<'static> = Templates {
        slice: include_str!("../../templates/linux/kbf.slice"),
        minio: include_str!("../../templates/linux/minio-kbf.conf"),
        server: include_str!("../../templates/linux/kbf-server.service"),
        daemon: include_str!("../../templates/linux/kbf-daemon.service"),
    };
}

/// The rendered files, each a path relative to the output directory (which is
/// `/etc/systemd/system` on the host) and its text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Units {
    pub slice: String,
    pub minio: String,
    pub server: String,
    pub daemon: String,
    /// The MinIO drop-in's directory, `<MINIO_UNIT>.d`.
    pub minio_dir: String,
}

/// The file names under the output directory, in the order the units are listed.
pub const SERVER: &str = "kbf-server.service";
pub const DAEMON: &str = "kbf-daemon.service";
pub const DROP_IN: &str = "kbf.conf";

impl Units {
    /// Each file's path under the output directory and its text.
    pub fn files(&self) -> [(String, &str); 4] {
        [
            (SLICE.to_owned(), self.slice.as_str()),
            (format!("{}/{DROP_IN}", self.minio_dir), self.minio.as_str()),
            (SERVER.to_owned(), self.server.as_str()),
            (DAEMON.to_owned(), self.daemon.as_str()),
        ]
    }
}

/// Render `templates` with `config`; `kbf_uid` is the kbf user's uid, or `None` to
/// leave [`UID_PLACEHOLDER`].
pub fn render(
    config: &LinuxConfig,
    kbf_uid: Option<u32>,
    templates: &Templates<'_>,
) -> Result<Units, TemplateError> {
    let values = values(config, kbf_uid);
    Ok(Units {
        slice: template::render(SLICE, templates.slice, &values)?,
        minio: template::render("minio-kbf.conf", templates.minio, &values)?,
        server: template::render(SERVER, templates.server, &values)?,
        daemon: template::render(DAEMON, templates.daemon, &values)?,
        minio_dir: format!("{}.d", config.minio_unit),
    })
}
