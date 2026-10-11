//! What is installed: the pinned Xcodes and the default one, the probe scripts, and
//! no simulator runtime on the host.

use sha2::{Digest as _, Sha256};

use crate::items::{Ctx, Outcome, fail, or, pass};

const PLISTBUDDY: &str = "/usr/libexec/PlistBuddy";
const XCODE_SELECT: &str = "/usr/bin/xcode-select";
const CORE_SIMULATOR: &str = "/Library/Developer/CoreSimulator";

/// The `ProductBuildVersion` of the Xcode at `app`, or empty.
fn xcode_build(c: &Ctx<'_>, app: &str) -> String {
    let plist = format!("{app}/Contents/version.plist");
    if !c.mac.is_file(&plist) {
        return String::new();
    }
    c.mac.out(
        PLISTBUDDY,
        &["-c", "Print :ProductBuildVersion", &c.mac.path(&plist)],
    )
}

/// Every `Xcode*.app` directory in `/Applications` the profile does not pin. Links are
/// skipped: they name an Xcode that is already there.
fn unpinned_xcodes(c: &Ctx<'_>) -> Vec<String> {
    c.mac
        .list("/Applications")
        .into_iter()
        .filter(|name| name.starts_with("Xcode") && name.ends_with(".app"))
        .map(|name| format!("/Applications/{name}"))
        .filter(|app| c.mac.is_dir(app) && c.mac.lstat(app).is_some_and(|s| s.kind != 'l'))
        .filter(|app| {
            let dir = format!("{app}/Contents/Developer");
            !c.profile.xcodes.iter().any(|x| x.developer_dir == dir)
        })
        .collect()
}

/// Each pinned Xcode is installed at its path with its build, and no other is.
pub(crate) fn xcode(c: &Ctx<'_>) -> Outcome {
    let mut wrong = Vec::new();
    let mut pinned = String::new();
    for x in &c.profile.xcodes {
        let found = xcode_build(c, x.app());
        if found == x.build {
            pinned.push_str(&format!(" {}", x.build));
        } else {
            wrong.push(format!(
                "{} has {}, want {};",
                x.app(),
                or(&found, "no Xcode"),
                x.build
            ));
        }
    }
    wrong.extend(
        unpinned_xcodes(c)
            .into_iter()
            .map(|app| format!("{app} is not pinned;")),
    );
    if wrong.is_empty() {
        pass(format!("pinned:{}", or(&pinned, " none")))
    } else {
        fail(wrong.join(" "))
    }
}

pub(crate) fn default_developer_dir(c: &Ctx<'_>) -> Outcome {
    if c.profile.xcodes.is_empty() {
        return pass("no Xcode is pinned");
    }
    let found = c.mac.out(XCODE_SELECT, &["-p"]);
    let want = c.value();
    if found == want {
        pass(found)
    } else {
        fail(format!(
            "xcode-select -p is {}, want {want}",
            or(&found, "unset")
        ))
    }
}

/// Each probe script is at its path with its pinned SHA-256.
pub(crate) fn probe(c: &Ctx<'_>) -> Outcome {
    let wrong: Vec<String> = c
        .profile
        .probes
        .iter()
        .filter(|p| {
            c.mac
                .read(&p.path)
                .map(|b| hex::encode(Sha256::digest(b)))
                .as_deref()
                != Some(p.sha256.as_str())
        })
        .map(|p| format!("{}: {} is missing or has another SHA-256;", p.key, p.path))
        .collect();
    if wrong.is_empty() {
        let keys: Vec<&str> = c.profile.probes.iter().map(|p| p.key.as_str()).collect();
        pass(or(&keys.join(" "), "none").to_owned())
    } else {
        fail(wrong.join(" "))
    }
}

/// No runtime image, runtime bundle or mounted runtime volume on the host: simulator
/// runtimes live in VM images.
pub(crate) fn simulator_runtimes(c: &Ctx<'_>) -> Outcome {
    let found: String = [
        ("Profiles/Runtimes", ".simruntime"),
        ("Volumes", ""),
        ("Images", ".dmg"),
    ]
    .into_iter()
    .flat_map(|(dir, suffix)| {
        let dir = format!("{CORE_SIMULATOR}/{dir}");
        c.mac
            .list(&dir)
            .into_iter()
            .filter(move |name| name.ends_with(suffix))
            .map(move |name| format!("{dir}/{name}"))
    })
    .filter(|path| c.mac.exists(path))
    .map(|path| format!(" {path}"))
    .collect();
    if found.is_empty() {
        pass("none")
    } else {
        fail(format!(
            "simulator runtimes on the host:{found} (they belong in VM images; erase the node)"
        ))
    }
}
