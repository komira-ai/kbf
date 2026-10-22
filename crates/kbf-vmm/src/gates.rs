//! The feasibility gates. Each function runs one gate and returns its [`Report`].
//!
//! `ok` means the gate ran and `details` holds the answer, favourable or not.
//! `needs-manual` means something the binary cannot create is missing, and says what.

use serde_json::{Value, json};

use crate::bundle::{self, Identity};
use crate::cli::Gate;
use crate::guest::{self, GuestRun, Share};
use crate::probe::{Boot, Ctx, Vm};
use crate::report::Report;
use crate::spec::{LIMIT_EXCEEDED, TaggedShare, VmSpec};

/// A report for a base bundle that is not there, or `None` when it is.
fn base_missing(ctx: &Ctx<'_>, probe: &str) -> Option<Report> {
    bundle::check(&ctx.base).err().map(|e| {
        Report::needs_manual(
            probe,
            format!("{e}; this needs a host with a prepared base bundle and a hypervisor"),
            json!({"base": ctx.base}),
        )
    })
}

/// Stops every VM and removes its clone and share directories.
fn clean(ctx: &Ctx<'_>, vms: &mut [Vm]) {
    for vm in vms.iter_mut() {
        vm.stop(&ctx.timing);
    }
    for vm in vms.iter() {
        for suffix in ["", "-share", "-tagged"] {
            let _ = std::fs::remove_dir_all(ctx.work.join(format!("{}{suffix}", vm.name)));
        }
    }
}

/// Starts `count` clones of `prefix` with `identity` at once and judges each.
fn clone_arm(
    ctx: &Ctx<'_>,
    prefix: &str,
    identity: Identity,
    count: usize,
) -> Result<Value, String> {
    let mut vms = Vec::new();
    let mut clone_ms = Vec::new();
    for i in 0..count {
        match ctx.start_clone(&format!("{prefix}-{i}"), identity, |_| {}, None) {
            Ok((vm, took)) => {
                clone_ms.push(took.as_secs_f64() * 1000.0);
                vms.push(vm);
            }
            Err(e) => {
                clean(ctx, &mut vms);
                return Err(e);
            }
        }
    }
    let boots: Vec<Boot> = vms
        .iter_mut()
        .map(|vm| vm.await_boot(&ctx.timing))
        .collect();
    clean(ctx, &mut vms);
    Ok(json!({
        "booted": boots.iter().filter(|b| b.booted).count(),
        "of": count,
        "clone_ms": clone_ms,
        "vms": boots.iter().map(Boot::json).collect::<Vec<_>>(),
    }))
}

/// G1: clones of one base: the same machine identifier, two at once, and a new identifier
/// on copied auxiliary storage.
#[must_use]
pub fn g1(ctx: &Ctx<'_>) -> Report {
    if let Some(r) = base_missing(ctx, "g1") {
        return r;
    }
    let same = clone_arm(ctx, "g1-same", Identity::Keep, 2);
    let fresh = clone_arm(ctx, "g1-fresh", Identity::Fresh, 2);
    match (same, fresh) {
        (Ok(a), Ok(b)) => Report::ok(
            "g1",
            format!(
                "same identifier: {} of 2 booted at once; new identifier on copied auxiliary storage: {} of 2 booted",
                a["booted"], b["booted"]
            ),
            json!({"same_identifier": a, "new_identifier": b}),
        ),
        (Err(e), _) | (_, Err(e)) => Report::error("g1", e),
    }
}

/// A clone with the automount share, the tagged share, a vsock listener and a console
/// port, booted far enough to look for the guest's files.
fn guest_vm(ctx: &Ctx<'_>, name: &str) -> Result<(Vm, Share, Boot), String> {
    let share = guest::prepare_share(ctx, name)?;
    let (mut vm, _) = ctx.start_clone(
        name,
        Identity::Keep,
        |spec: &mut VmSpec| {
            spec.automount_dir = Some(share.dir.clone());
            spec.tagged = Some(TaggedShare {
                tag: guest::TAG.to_owned(),
                path: share.tagged.clone(),
            });
            spec.vsock_ports = vec![guest::VSOCK_PORT];
            spec.console_ports = vec![guest::CONSOLE_NAME.to_owned()];
        },
        None,
    )?;
    let boot = vm.await_boot(&ctx.timing);
    Ok((vm, share, boot))
}

/// Boots a guest and collects `wanted`; the report details common to G2 and G3.
fn guest_session(
    ctx: &Ctx<'_>,
    probe: &str,
    wanted: &[&str],
) -> Result<(GuestRun, Boot, bool), Box<Report>> {
    if let Some(r) = base_missing(ctx, probe) {
        return Err(Box::new(r));
    }
    let (mut vm, share, boot) =
        guest_vm(ctx, &format!("{probe}-vm")).map_err(|e| Box::new(Report::error(probe, e)))?;
    let have_binary = guest::has_guest_binary(&share);
    if !boot.booted {
        clean(ctx, std::slice::from_mut(&mut vm));
        return Err(Box::new(Report::error(
            probe,
            format!(
                "the guest did not boot: {}",
                boot.failure.clone().unwrap_or_default()
            ),
        )));
    }
    let run = guest::collect(ctx, &mut vm, &share, wanted);
    clean(ctx, std::slice::from_mut(&mut vm));
    Ok((run, boot, have_binary))
}

/// G2: which channel carries bytes from a process inside the guest to the host.
#[must_use]
pub fn g2(ctx: &Ctx<'_>) -> Report {
    let (run, boot, have_binary) = match guest_session(ctx, "g2", &[guest::USER_RESULT]) {
        Ok(x) => x,
        Err(r) => return *r,
    };
    let details = json!({"boot": boot.json(), "guest": run.results, "host_saw": run.traffic});
    if !run.missing.is_empty() {
        return Report::needs_manual(
            "g2",
            guest::no_agent_reason(have_binary, &run.missing),
            details,
        );
    }
    let vsock = !run.traffic["vsock_connections"]
        .as_array()
        .is_none_or(Vec::is_empty);
    let console = !run.traffic["console_writes"]
        .as_array()
        .is_none_or(Vec::is_empty);
    let channel = if vsock {
        "vsock"
    } else if console {
        "console"
    } else {
        "none"
    };
    let mut details = details;
    details["channel"] = json!(channel);
    Report::ok(
        "g2",
        format!("channel from guest to host: {channel}"),
        details,
    )
}

/// The exit code of the custom-tag mount in one guest result, when it was tried.
fn mount_exit(result: &Value) -> Option<i64> {
    result["mount_custom_tag"]["exit_code"].as_i64()
}

/// G3: whether mounting a custom virtiofs tag in the guest needs root, against the
/// automount share.
#[must_use]
pub fn g3(ctx: &Ctx<'_>) -> Report {
    let (run, boot, have_binary) =
        match guest_session(ctx, "g3", &[guest::USER_RESULT, guest::ROOT_RESULT]) {
            Ok(x) => x,
            Err(r) => return *r,
        };
    let details = json!({"boot": boot.json(), "guest": run.results});
    if !run.missing.is_empty() {
        return Report::needs_manual(
            "g3",
            guest::no_agent_reason(have_binary, &run.missing),
            details,
        );
    }
    let user = mount_exit(&run.results[guest::USER_RESULT]);
    let root = mount_exit(&run.results[guest::ROOT_RESULT]);
    let automount = run.results[guest::USER_RESULT]["automount"]["dir_readable"].as_bool();
    let needs_root = match (user, root) {
        (Some(u), Some(r)) => Some(u != 0 && r == 0),
        _ => None,
    };
    let mut details = details;
    details["custom_tag_mount_needs_root"] = json!(needs_root);
    details["automount_readable_without_root"] = json!(automount);
    Report::ok(
        "g3",
        format!(
            "custom tag mount as the unprivileged user: exit {user:?}, as root: exit {root:?}; automount share readable without root: {automount:?}"
        ),
        details,
    )
}

/// The uid G5 runs a helper as when none is given.
pub const THROWAWAY_UID: u32 = 59_001;

/// G5/P5: starting a VM from a launch-daemon child with nobody logged in, with and
/// without a graphics device, as a throwaway uid.
#[must_use]
pub fn g5(ctx: &Ctx<'_>, run_as: Option<u32>) -> Report {
    if let Some(r) = base_missing(ctx, "g5") {
        return r;
    }
    let root = ctx.sys.uid() == 0;
    let daemon_child = ctx.sys.ppid() == 1;
    let console = ctx.sys.console_uid();
    let mut missing = Vec::new();
    if !daemon_child {
        missing.push(
            "this process is not a child of launchd (parent pid is not 1): install a launch daemon that runs `kbf-vmm probe g5` and run that"
                .to_owned(),
        );
    }
    if !root {
        missing.push(
            "not root: the throwaway-uid arms need root to start the helper as another user"
                .to_owned(),
        );
    }
    if console != Some(0) {
        missing.push(format!("someone may be logged in at the console (owner of /dev/console: {console:?}); log out first"));
    }
    let uid = run_as.unwrap_or(THROWAWAY_UID);
    let mut variants = vec![(false, None), (true, None)];
    if root {
        variants.extend([(false, Some(uid)), (true, Some(uid))]);
    }
    let mut arms = Vec::new();
    for (graphics, as_uid) in variants {
        let name = format!(
            "g5-{}-{}",
            if graphics { "gfx" } else { "nogfx" },
            as_uid.map_or("self".to_owned(), |u| u.to_string())
        );
        let outcome = ctx
            .start_clone(
                &name,
                Identity::Keep,
                |s: &mut VmSpec| s.graphics = graphics,
                as_uid,
            )
            .map(|(mut vm, _)| {
                let boot = vm.await_boot(&ctx.timing);
                clean(ctx, std::slice::from_mut(&mut vm));
                boot.json()
            });
        arms.push(json!({
            "graphics": graphics,
            "as_uid": as_uid,
            "result": outcome.unwrap_or_else(|e| json!({"booted": false, "failure": e})),
        }));
    }
    let details = json!({
        "parent_pid_is_launchd": daemon_child,
        "root": root,
        "console_uid": console,
        "throwaway_uid": uid,
        "arms": arms,
    });
    if missing.is_empty() {
        Report::ok(
            "g5",
            "VM start from a launch-daemon child as a throwaway uid, per graphics setting, is in arms",
            details,
        )
    } else {
        Report::needs_manual("g5", missing.join("; "), details)
    }
}

/// P2: a third VM fails with the guest-limit error; after SIGKILL of a helper, how soon a
/// new start succeeds.
#[must_use]
pub fn p2(ctx: &Ctx<'_>) -> Report {
    if let Some(r) = base_missing(ctx, "p2") {
        return r;
    }
    let mut vms = Vec::new();
    let mut boots = Vec::new();
    for i in 0..2 {
        match ctx.start_clone(&format!("p2-{i}"), Identity::Keep, |_| {}, None) {
            Ok((mut vm, _)) => {
                boots.push(vm.await_boot(&ctx.timing));
                vms.push(vm);
            }
            Err(e) => {
                clean(ctx, &mut vms);
                return Report::error("p2", e);
            }
        }
    }
    if boots.iter().any(|b| !b.booted) {
        let why = format!(
            "the first two guests did not both boot: {:?}",
            boots.iter().map(Boot::json).collect::<Vec<_>>()
        );
        clean(ctx, &mut vms);
        return Report::error("p2", why);
    }
    let third = match ctx.start_clone("p2-2", Identity::Keep, |_| {}, None) {
        Ok((mut vm, _)) => {
            let boot = vm.await_boot(&ctx.timing);
            vms.push(vm);
            boot
        }
        Err(e) => {
            clean(ctx, &mut vms);
            return Report::error("p2", e);
        }
    };
    let limit = third.code.as_deref() == Some(LIMIT_EXCEEDED);
    // The helper of the first VM dies without stopping its guest.
    let victim_pid = vms[0].proc.pid();
    vms[0].kill9(&ctx.timing);
    // The killed VM's own bundle: its disk is free again only if the slot is.
    let spec = VmSpec::minimal(ctx.work.join("p2-0"));
    let after = ctx.spawn_retrying("p2-after-kill", &spec);
    let after_json = match after {
        Ok(mut r) => {
            let j = json!({
                "started": true,
                "first_attempt_succeeded": r.attempts == 1,
                "attempts": r.attempts,
                "waited_ms": r.waited.as_secs_f64() * 1000.0,
            });
            r.vm.stop(&ctx.timing);
            j
        }
        Err(e) => json!({"started": false, "failure": e}),
    };
    clean(ctx, &mut vms);
    Report::ok(
        "p2",
        format!(
            "third guest refused with the limit error: {limit}; start after SIGKILL of a helper: {after_json}"
        ),
        json!({
            "third": third.json(),
            "third_hit_limit": limit,
            "killed_helper_pid": victim_pid,
            "start_after_sigkill": after_json,
        }),
    )
}

/// P0: whether a root offline write into an attached guest volume works. It writes the
/// payload into a clone (the base stays unchanged), then boots the clone and looks for
/// the daemons' markers.
#[must_use]
pub fn p0(ctx: &Ctx<'_>) -> Report {
    if let Some(r) = base_missing(ctx, "p0") {
        return r;
    }
    if ctx.sys.uid() != 0 {
        return Report::needs_manual(
            "p0",
            "needs root: attaching a guest disk image and writing root-owned files into its Data volume is the question; run with sudo",
            json!({"uid": ctx.sys.uid()}),
        );
    }
    let dest = ctx.work.join("p0-vm");
    if let Err(e) = bundle::clone_bundle(ctx.sys, &ctx.base, &dest, Identity::Keep) {
        return Report::error("p0", e.to_string());
    }
    let written = ctx
        .sys
        .offline_write(&dest.join(bundle::DISK), &guest::offline_payload());
    let _ = std::fs::remove_dir_all(&dest);
    match written {
        Ok(observed) => Report::ok(
            "p0",
            "a root offline write into the guest's Data volume worked",
            json!({"worked": true, "observed": observed}),
        ),
        Err(e) => Report::ok(
            "p0",
            "a root offline write into the guest's Data volume failed",
            json!({"worked": false, "failure": e}),
        ),
    }
}

/// P6: an ad hoc, hardened-runtime signature with the virtualization entitlement runs a VM.
#[must_use]
pub fn p6(ctx: &Ctx<'_>) -> Report {
    if let Some(r) = base_missing(ctx, "p6") {
        return r;
    }
    let sig = match ctx.sys.signature() {
        Ok(s) => s,
        Err(e) => return Report::error("p6", e),
    };
    let sig_json = json!({
        "adhoc": sig.adhoc,
        "hardened_runtime": sig.hardened_runtime,
        "virtualization_entitlement": sig.virtualization_entitlement,
        "codesign": sig.raw,
    });
    if !(sig.adhoc && sig.hardened_runtime && sig.virtualization_entitlement) {
        return Report::needs_manual(
            "p6",
            "this binary is not signed ad hoc with the hardened runtime and the virtualization entitlement; run the binary `cargo xtask sign-darwin` signed (the CI artifact)",
            json!({"signature": sig_json}),
        );
    }
    match ctx.start_clone("p6-vm", Identity::Keep, |_| {}, None) {
        Ok((mut vm, _)) => {
            let boot = vm.await_boot(&ctx.timing);
            clean(ctx, std::slice::from_mut(&mut vm));
            Report::ok(
                "p6",
                if boot.booted {
                    "a VM ran under the ad hoc hardened-runtime signature with the entitlement"
                } else {
                    "a VM did not run under the ad hoc hardened-runtime signature with the entitlement"
                },
                json!({"signature": sig_json, "boot": boot.json()}),
            )
        }
        Err(e) => Report::error("p6", e),
    }
}

/// Runs `gate`.
#[must_use]
pub fn run(ctx: &Ctx<'_>, gate: Gate, run_as: Option<u32>) -> Report {
    match gate {
        Gate::G1 => g1(ctx),
        Gate::G2 => g2(ctx),
        Gate::G3 => g3(ctx),
        Gate::G5 => g5(ctx, run_as),
        Gate::P0 => p0(ctx),
        Gate::P2 => p2(ctx),
        Gate::P6 => p6(ctx),
    }
}
