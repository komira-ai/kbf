//! `linux render-units`: the converged storage node's four systemd files from the
//! templates in `templates/linux/` and a [`LinuxConfig`].

use std::collections::BTreeMap;

use super::config::LinuxConfig;
use crate::template::{self, TemplateError, Value, Values};

/// What `render-units` writes when `--kbf-uid` is not given: the kbf user's uid (the
/// daemon's `XDG_RUNTIME_DIR`) as a placeholder, which `check-units --installable`
/// refuses.
pub const UID_PLACEHOLDER: &str = "<kbf uid>";

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
pub const SLICE: &str = "kbf.slice";
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

pub(super) fn values(c: &LinuxConfig, kbf_uid: Option<u32>) -> Values {
    let slice_mib =
        (c.minio_reservation_gib + c.server_reservation_gib) * 1024 + c.daemon_memory_min_mib;
    // kbf-daemon --image takes docker://<repo>@sha256:<digest>; KBF_IMAGES holds the
    // bare form podman pulls.
    let images = c
        .images
        .iter()
        .map(|i| format!("    --image=docker://{i} \\"))
        .collect();
    BTreeMap::from([
        ("SLICE_MEMORY_MIN", Value::Text(format!("{slice_mib}M"))),
        (
            "MINIO_MEMORY_MIN",
            Value::Text(format!("{}G", c.minio_reservation_gib)),
        ),
        (
            "SERVER_MEMORY_MIN",
            Value::Text(format!("{}G", c.server_reservation_gib)),
        ),
        (
            "DAEMON_MEMORY_MIN",
            Value::Text(format!("{}M", c.daemon_memory_min_mib)),
        ),
        (
            "ACTIONS_MEMORY_MAX_GIB",
            Value::Text(c.actions_gib.to_string()),
        ),
        (
            "KBF_DAEMON_MEMORY_MIN_MIB",
            Value::Text(c.daemon_memory_min_mib.to_string()),
        ),
        (
            "KBF_UID",
            Value::Text(kbf_uid.map_or(UID_PLACEHOLDER.to_owned(), |u| u.to_string())),
        ),
        ("MINIO_UNIT", Value::Text(c.minio_unit.clone())),
        (
            "MINIO_S3_ENDPOINT",
            Value::Text(c.minio_s3_endpoint.clone()),
        ),
        ("KBF_S3_BUCKET", Value::Text(c.s3_bucket.clone())),
        ("KBF_LISTEN_HOST", Value::Text(c.listen_host())),
        ("KBF_HOST_NAME", Value::Text(c.host_name.clone())),
        ("KBF_NODE_ID", Value::Text(c.node_id.clone())),
        ("KBF_REAPI_PORT", Value::Text(c.reapi_port.to_string())),
        ("KBF_WORKER_PORT", Value::Text(c.worker_port.to_string())),
        ("KBF_API_PORT", Value::Text(c.api_port.to_string())),
        ("KBF_DATA_DIR", Value::Text(c.data_dir.to_string())),
        ("KBF_META_DIR", Value::Text(c.meta_dir.to_string())),
        ("KBF_USER", Value::Text(c.user.clone())),
        ("KBF_SERVER_USER", Value::Text(c.server_user.clone())),
        ("KBF_ETC", Value::Text(c.etc.to_string())),
        ("KBF_BIN_DIR", Value::Text(c.bin_dir.to_string())),
        ("DAEMON_IMAGE_ARGS", Value::Lines(images)),
    ])
}
