//! `linux image-pull`: the pinned build images, pulled ahead into the kbf user's
//! rootless Podman store.
//!
//! The images are `KBF_IMAGES` (or each `--image`), each `<repo>@sha256:<64 hex>`: the
//! ones `kbf-daemon --image` checks at start, where an image missing from the store
//! stops the daemon. For each image not in the store the plan pulls it, as the kbf
//! user (its uid, its groups, `HOME` and `XDG_RUNTIME_DIR=/run/user/<uid>`):
//!
//! ```text
//! podman pull --quiet <repo>@sha256:<digest>
//! ```
//!
//! Every image already in the store is checked: the store's copy must have the pinned
//! digest (`podman image inspect`'s `.Digest`; an image index digest names several
//! architectures, and the pin is one manifest's) and be built for `--platform`
//! (`linux/amd64` by default). After an apply the plan is made again: each pulled
//! image is then checked, and one still missing fails the verb. A re-run pulls
//! nothing.
//!
//! The reads run as the kbf user too, so even a dry run needs root.

use std::io::Write;

use kbf_types::ImageRef;

use super::config::LinuxConfig;
use super::host::{self, HostError, PODMAN, note, tool};
use crate::exec::Exec;
use crate::plan::{Mode, Plan, Planner, RunAs, execute};
use crate::step::{Cmd, Step};
use crate::user::RunUser;

/// The farm's build platform.
pub const PLATFORM: &str = "linux/amd64";

/// What `image-pull` was asked for.
pub struct Pull {
    pub config: LinuxConfig,
    /// From `--image`; `KBF_IMAGES` when empty.
    pub images: Vec<ImageRef>,
    pub platform: String,
}

/// Plan the pulls, printing what is already in the store.
pub fn plan<E: Exec>(exec: &E, a: &Pull, out: &mut dyn Write) -> Result<Plan, HostError> {
    let images = if a.images.is_empty() {
        &a.config.images
    } else {
        &a.images
    };
    if images.is_empty() {
        return Err(HostError::NoImages);
    }
    let name = &a.config.user;
    if exec.euid() != 0 {
        return Err(HostError::PullNeedsRoot(name.clone()));
    }
    let user = host::user_named(exec, name)?;
    let mut planner = Planner::new(exec);
    let mut pending = 0;
    for image in images {
        let image = image.to_string();
        let exists = podman(&user).arg("image").arg("exists").arg(&image);
        if host::exits_0(exec, &exists)? {
            check(exec, &user, &image, &a.platform)?;
            note(
                out,
                format_args!(
                    "{image}: in {name}'s store, {} and digest checked",
                    a.platform
                ),
            )?;
        } else {
            planner.run(podman(&user).arg("pull").arg("--quiet").arg(&image));
            pending += 1;
        }
    }
    if pending > 0 {
        note(
            out,
            format_args!("{pending} image(s) to pull; each is checked after the pull"),
        )?;
    }
    Ok(planner.finish())
}

/// Plan, print or carry the plan out, and after an apply check every image again.
pub fn run<E: Exec>(
    exec: &mut E,
    a: &Pull,
    mode: Mode,
    mut out: &mut dyn Write,
) -> Result<(), HostError> {
    let pulls = plan(&*exec, a, &mut *out)?;
    execute(&pulls, RunAs::Root, mode, exec, &mut out)?;
    if mode == Mode::Apply && !pulls.is_empty() {
        // Each pull's last argument is its image.
        let again = plan(&*exec, a, &mut *out)?;
        if let Some(Step::Run(pull)) = again.steps().first() {
            return Err(HostError::NotPulled(
                pull.args.last().cloned().unwrap_or_default(),
            ));
        }
    }
    Ok(())
}

/// The store's copy of `image` has its pinned digest and is built for `platform`.
fn check<E: Exec>(exec: &E, user: &RunUser, image: &str, platform: &str) -> Result<(), HostError> {
    let inspect = podman(user)
        .arg("image")
        .arg("inspect")
        .arg("--format")
        .arg("{{.Digest}} {{.Os}}/{{.Architecture}}")
        .arg(image);
    let text = host::checked(exec, &inspect)?;
    let Some((digest, got)) = text.trim().split_once(' ') else {
        return Err(HostError::Output {
            cmd: inspect.to_string(),
            out: text,
        });
    };
    if !image.ends_with(&format!("@{digest}")) {
        return Err(HostError::WrongDigest {
            image: image.to_owned(),
            got: digest.to_owned(),
            platform: platform.to_owned(),
        });
    }
    if got != platform {
        return Err(HostError::WrongPlatform {
            image: image.to_owned(),
            got: got.to_owned(),
            want: platform.to_owned(),
        });
    }
    Ok(())
}

/// `podman`, run as `user` with the environment rootless Podman needs.
fn podman(user: &RunUser) -> Cmd {
    Cmd::new(tool(PODMAN))
        .user(user.clone())
        .env("HOME", &user.home)
        .env("USER", &user.name)
        .env("LOGNAME", &user.name)
        .env("XDG_RUNTIME_DIR", format!("/run/user/{}", user.uid))
        // Podman finds its helpers (newuidmap, conmon, the OCI runtime) on PATH.
        .env(
            "PATH",
            "/usr/local/bin:/usr/bin:/bin:/usr/local/sbin:/usr/sbin:/sbin",
        )
}

#[cfg(test)]
#[path = "images_tests.rs"]
mod tests;
