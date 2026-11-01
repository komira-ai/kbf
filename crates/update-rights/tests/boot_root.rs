//! The boot entry of a declaration under the real systemd, as root (docs/design/host-updates.md,
//! 3.5): it runs through the same rights path as a step, with the hook-wide capabilities and never
//! a step's.
//!
//! Needs Linux, systemd, root and `nft`, so it is `#[ignore]`d and runs in the
//! `update-rights-linux` workflow through `cargo xtask sudo-test -p update-rights --test
//! boot_root`. No test here decides anything by how long it takes: every run ends by its own exit.

#![cfg(target_os = "linux")]

mod root_rig;

use std::path::Path;
use std::process::Command;

use root_rig::{Rig, Spec, hook_uid, record, refused, said_by};
use update_hook::{Outcome, Step};
use update_rights::{Declaration, Declared, Systemd, run_boot};

/// Where `nft` is, or the test is refused: the workflow installs it.
fn nft() -> &'static str {
    ["/usr/sbin/nft", "/sbin/nft", "/usr/bin/nft"]
        .into_iter()
        .find(|path| Path::new(path).exists())
        .unwrap_or_else(|| refused(1, "no nft on the image"))
}

/// Removes the table the test makes, whatever the test ends in.
struct Table(String);

impl Drop for Table {
    fn drop(&mut self) {
        let _ = Command::new(nft())
            .args(["delete", "table", "inet", &self.0])
            .output();
    }
}

/// The declaration of `spec` with the boot entry `restore`, loaded as the rig loads one.
fn with_boot(rig: &Rig, spec: &Spec, hook_args: &[&str]) -> Declared {
    let text = rig.toml(&rig.service(), spec, hook_args).replacen(
        "\n[hook]\n",
        "\nboot = \"restore\"\n[hook]\n",
        1,
    );
    let declaration = Declaration::parse(&text).unwrap_or_else(|e| panic!("{e}\n{text}"));
    assert_eq!(declaration.boot.as_deref(), Some("restore"));
    Declared::by_hand(declaration, hook_uid(spec), &[&rig.service()])
}

/// Catches a boot entry launched without the hook-wide capabilities, or with a step's: the boot
/// entry of a declaration with `capabilities = ["CAP_NET_ADMIN"]` and every step's entry empty
/// runs `nft` as a non-root account and changes the ruleset; with the hook-wide list empty and
/// `CAP_NET_ADMIN` in `prepare`'s entry only, the same boot entry is refused by the kernel.
#[test]
#[ignore = "needs root and systemd; run by cargo xtask sudo-test"]
fn a_booted_hook_runs_nft_with_the_hook_wide_capability_and_not_without_it() {
    let rig = Rig::new("boot");
    let nft = nft();
    let table = format!("urboot{}", std::process::id());
    let _guard = Table(table.clone());
    let add = ["run", nft, "add", "table", "inet", table.as_str(), "--"];
    let release = rig.dir.join("release");
    let listed = || {
        Command::new(nft)
            .args(["list", "table", "inet", &table])
            .status()
            .unwrap()
            .success()
    };

    let mut narrow = rig.spec();
    narrow.families = vec!["AF_UNIX".to_owned(), "AF_NETLINK".to_owned()];
    narrow
        .steps
        .push((Step::Prepare, vec!["CAP_NET_ADMIN".to_owned()]));
    let denied = run_boot(
        &Systemd::system(),
        &with_boot(&rig, &narrow, &add),
        1,
        &release,
    )
    .expect("the boot entry runs")
    .expect("the declaration names one");
    assert_eq!(denied.outcome, Outcome::Failure, "{}", said_by(&denied));
    let said = denied.report.detail.clone().unwrap_or_default();
    assert!(said.contains("not permitted"), "{}", said_by(&denied));
    assert!(!listed(), "nothing was added without the capability");

    let mut wide = rig.spec();
    wide.families = narrow.families.clone();
    wide.caps = vec!["CAP_NET_ADMIN".to_owned()];
    wide.steps = Step::ALL.iter().map(|step| (*step, Vec::new())).collect();
    let added = run_boot(
        &Systemd::system(),
        &with_boot(&rig, &wide, &add),
        2,
        &release,
    )
    .expect("the boot entry runs")
    .expect("the declaration names one");
    assert_eq!(added.outcome, Outcome::Success, "{}", said_by(&added));
    assert_eq!(added.unit, format!("updater-{}-boot-2", rig.service()));
    assert!(listed(), "the booted hook added the table");
    record(
        "boot-nft",
        "BOOT RAN: the boot entry of a declaration with CAP_NET_ADMIN hook-wide and every step's \
         entry empty ran nft as nobody; with the hook-wide list empty and CAP_NET_ADMIN in \
         prepare's entry only, the same boot entry was refused",
    );
}
