//! The static unit files of the kbf-installer package (`packaging/linux/units/`) against
//! the rules `kbf-deploy linux check-units` enforces on the units it renders, and against
//! the kbf-installer contract (docs/design/kbf-installer.md, section 7).
//!
//! The package ships these files disabled, with no rendering: kbf-installer's own unit,
//! each mode's unit, the frontend and Envoy templates, gated on `/etc/kbf/kbf.toml`
//! (`ConditionPathExists=`), on `kbf-installer boot-check` and on `kbf <mode> --check`
//! (both `ExecStartPre=`; `ExecCondition=` would skip a failed check silently).
//! kbf-deploy's rule modules are private to it, so the rules are restated here over the
//! same unit parser (`kbf_deploy::unit`), each with the defect it catches, and `MUTANTS`
//! plants one defect per rule in a copy of the real file and requires that rule to name
//! it. `every_rule_has_a_mutant` fails when a rule is added with no mutant, so no rule
//! can be one that cannot fail.
//!
//! Catches:
//! - a unit missing a rule of `check-units` that its mode needs (`restart`, `stop`,
//!   `delegate`, `handover`, `credential`, user, slice, `LimitNOFILE`), each named below;
//! - a unit that runs a program outside the installer's tree (`/var/lib/kbf-installer/`),
//!   another mode than its name, a `--config` or any flag the contract does not give it;
//! - a unit that starts without its gate (`ConditionPathExists=`, the two checks, in
//!   order), or whose gate is `ExecCondition=` or a `-`-prefixed command that ignores a
//!   failure;
//! - kbf-installer's own unit losing a hardening key or its place before the runtime units;
//! - a kbf-deploy placeholder (`@KEY@`, `<value>`) left in a file that is not rendered;
//! - a file added or removed without this test being told.
//!
//! Dependencies on work not on the branch yet: `kbf --mode=<mode> --check` and
//! `--instance` (P12, P13); `kbf-cli/tests/package_units.rs` holds
//! the parse of each mode's flags by `kbf`'s own parser, ignored until they land.

use std::path::{Path, PathBuf};

use kbf_deploy::unit::{UnitFile, comments_in_continuations};

/// The installer's tree: every program of a kbf unit is under it.
const TREE: &str = "/var/lib/kbf-installer/";
/// The release kbf-installer flipped to.
const CURRENT: &str = "/var/lib/kbf-installer/current/";
/// A frontend instance's slot: the release it runs, until a confirm flips `current`.
const SLOT: &str = "/var/lib/kbf-installer/slots/%i/";
/// The pinned Envoy of instance `%i`.
const ENVOY_BIN: &str = "/usr/bin/envoy";
/// The installer binary the units call.
const INSTALLER: &str = "/usr/bin/kbf-installer";
/// The configuration every runtime unit is gated on and every mode reads.
const CONFIG: &str = "/etc/kbf/kbf.toml";
/// The installer's own configuration, which `setup` writes.
const INSTALLER_CONFIG: &str = "/etc/kbf/installer.toml";
/// Envoy's rendered bootstrap.
const ENVOY_CONFIG: &str = "/etc/kbf/envoy/envoy.json";

/// The files of the package, and nothing else.
const FILES: [&str; 10] = [
    "kbf-all-in-one.service",
    "kbf-backup.service",
    "kbf-backup.timer",
    "kbf-daemon.service",
    "kbf-envoy.service",
    "kbf-frontend@.service",
    "kbf-installer.service",
    "kbf-storage-and-daemon.service",
    "kbf-storage.service",
    "kbf.slice",
];

/// The runtime units kbf-installer starts before (`Before=` of its unit).
const RUNTIME: [&str; 6] = [
    "kbf-all-in-one.service",
    "kbf-daemon.service",
    "kbf-envoy.service",
    "kbf-frontend@.service",
    "kbf-storage-and-daemon.service",
    "kbf-storage.service",
];

/// The rules, by the tag a violation starts with.
const RULES: [&str; 31] = [
    "install",
    "network",
    "positional",
    "placeholder",
    "continuation",
    "exec",
    "exec-prefix",
    "flags",
    "gate-file",
    "gate-skip",
    "gate-check",
    "slice",
    "nofile",
    "user",
    "state-dir",
    "credential",
    "restart",
    "restart-limit",
    "restart-exit",
    "stop",
    "delegate",
    "notify",
    "handover",
    "conflicts",
    "swap",
    "envoy",
    "memory",
    "reload",
    "installer",
    "runtime",
    "backup",
];

fn units_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging/linux/units")
}

fn read(name: &str) -> String {
    let path = units_dir().join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// The mode a unit file starts: the word after `kbf`. Envoy's, the installer's and the
/// backup's units start no mode.
fn mode_of(name: &str) -> Option<&str> {
    let stem = name.strip_suffix(".service")?;
    let mode = stem.strip_prefix("kbf-")?.trim_end_matches('@');
    (!["envoy", "installer", "backup"].contains(&mode)).then_some(mode)
}

fn trim(v: &str) -> &str {
    v.trim()
}

/// Every value of `key` in `[section]`, in file order.
fn all<'a>(unit: &'a UnitFile, section: &str, key: &str) -> Vec<&'a str> {
    unit.directives()
        .iter()
        .filter(|d| d.section == section && d.key == key)
        .map(|d| trim(&d.value))
        .collect()
}

/// systemd's `Delegate=` as `config_parse_delegate` reads it (see check-units rule
/// `delegate`): a boolean sets all or none, a list adds, an empty value starts over.
fn delegated(unit: &UnitFile) -> Vec<String> {
    let mut set: Vec<String> = Vec::new();
    for d in unit.directives().iter().filter(|d| d.key == "Delegate") {
        match trim(&d.value) {
            "yes" | "true" | "1" | "on" => {
                set = ["cpu", "cpuset", "memory", "pids"]
                    .map(String::from)
                    .to_vec();
            }
            "no" | "false" | "0" | "off" | "" => set.clear(),
            list => set.extend(list.split_whitespace().map(String::from)),
        }
    }
    set
}

/// A `@KEY@` kbf-deploy would render, on a line that is not a comment.
fn has_at_placeholder(line: &str) -> bool {
    let mut rest = line;
    while let Some(i) = rest.find('@') {
        let after = &rest[i + 1..];
        if let Some(j) = after.find('@')
            && j > 0
            && after[..j]
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b == b'_' || b.is_ascii_digit())
        {
            return true;
        }
        rest = after;
    }
    false
}

/// The violations of unit file `name` with contents `text`, each `<rule>: <why>`.
#[allow(clippy::too_many_lines)] // one rule after another is the readable form
fn violations(name: &str, text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut v = |rule: &str, why: String| out.push(format!("{rule}: {name}: {why}"));

    // Catches a kbf-deploy placeholder left in a file nothing renders: systemd would
    // run `@KBF_BIN_DIR@/kbf` or hand `<value>` to kbf.
    for (i, line) in text.lines().enumerate() {
        let code = line.trim();
        if code.starts_with('#') || code.starts_with(';') {
            continue;
        }
        if has_at_placeholder(code) || (code.contains('<') && code.contains('>')) {
            v("placeholder", format!("line {}: `{code}`", i + 1));
        }
    }
    // Catches a unit that names Podman, in a flag, an environment or a comment: kbf runs
    // containers with crun only, and a unit that mentions Podman says the host needs it.
    for (i, line) in text.lines().enumerate() {
        if line.to_ascii_lowercase().contains("podman") {
            v("runtime", format!("line {} names Podman", i + 1));
        }
    }
    // Catches a comment inside a continued directive: systemd 239 joins it into the
    // command line.
    for line in comments_in_continuations(text) {
        v("continuation", format!("line {line}"));
    }
    if name == "kbf-backup.service" || name == "kbf-backup.timer" {
        backup_rules(name, text, &mut v);
        return out;
    }
    if name == "kbf.slice" {
        let unit = UnitFile::parse(text).expect("kbf.slice parses");
        // Catches a per-host reservation baked into the shared file: MemoryMin= is the
        // sum of the host's units, kbf-installer writes it into a drop-in.
        if unit.get("MemoryMin").is_some() {
            v(
                "memory",
                "kbf.slice sets MemoryMin=, a per-host value".into(),
            );
        }
        return out;
    }
    let unit = UnitFile::parse(text).unwrap_or_else(|e| panic!("{name}: {e}"));
    let is_envoy = name == "kbf-envoy.service";
    let is_installer = name == "kbf-installer.service";
    let mode = mode_of(name);

    // Catches a unit that starts with no configuration: ConditionPathExists= skips it.
    let conditions = all(&unit, "Unit", "ConditionPathExists");
    let own_config = if is_installer {
        INSTALLER_CONFIG
    } else {
        CONFIG
    };
    if !conditions.contains(&own_config) {
        v(
            "gate-file",
            format!("no ConditionPathExists={own_config} in [Unit]"),
        );
    }
    // Catches an Envoy unit that starts before its configuration is rendered.
    if is_envoy && !conditions.contains(&ENVOY_CONFIG) {
        v("envoy", format!("no ConditionPathExists={ENVOY_CONFIG}"));
    }
    // Catches a gate that skips silently: ExecCondition= treats exit 1 to 254 as "do not
    // start, no failure", so a unit that cannot start would look healthy.
    if !all(&unit, "Service", "ExecCondition").is_empty() {
        v(
            "gate-skip",
            "ExecCondition= in place of ExecStartPre=".into(),
        );
    }
    // Catches a unit that is never started at boot once enabled: without
    // WantedBy=multi-user.target, `systemctl enable` links it nowhere and a reboot leaves
    // the host down (the restart drill's failure).
    if !all(&unit, "Install", "WantedBy").contains(&"multi-user.target") {
        v(
            "install",
            "no WantedBy=multi-user.target in [Install]".into(),
        );
    }
    // Catches a unit that starts before the network: a listener, a peer or a name looked up
    // before network-online.target fails the start (or binds the wrong address).
    if !is_installer {
        for key in ["Wants", "After"] {
            if !all(&unit, "Unit", key)
                .iter()
                .any(|v| v.split_whitespace().any(|u| u == "network-online.target"))
            {
                v(
                    "network",
                    format!("no {key}=network-online.target in [Unit]"),
                );
            }
        }
    }

    // Every program the unit runs is the installer's: the package replaces
    // /usr/bin/kbf-installer, the installer replaces the tree. Only the two root checks
    // carry a `+`; a `-` would ignore a failed gate, and `!`, `@`, `:` change the sandbox.
    let execs: Vec<(&str, &str)> = unit
        .directives()
        .iter()
        .filter(|d| d.section == "Service" && d.key.starts_with("Exec"))
        .map(|d| (d.key.as_str(), trim(&d.value)))
        .collect();
    for (key, value) in &execs {
        let plus_ok = value.starts_with(&format!("+{INSTALLER} boot-check"));
        if value.starts_with(['-', '!', '@', ':']) || (value.starts_with('+') && !plus_ok) {
            v("exec-prefix", format!("{key}={value}"));
        }
        let program = value
            .trim_start_matches('+')
            .split_whitespace()
            .next()
            .unwrap_or("");
        // Envoy is the host's prerequisite (installed by the host's packages, not part of a
        // kbf release), so its unit alone runs a binary outside the installer's tree; the
        // `envoy` rule pins which one.
        let allowed =
            program.starts_with(TREE) || program == INSTALLER || (is_envoy && program == ENVOY_BIN);
        if !allowed || program.contains("..") {
            v(
                "exec",
                format!("{key} runs {program}, not a file under {TREE}"),
            );
        }
    }
    // Catches a reload that restarts: Envoy's hot restart is kbf-installer's, and
    // `systemctl restart` closes every stream the old Envoy holds. No unit has a reload
    // until the hot restart exists.
    for (key, value) in &execs {
        if *key == "ExecReload" || value.contains("systemctl") {
            v("reload", format!("{key}={value}"));
        }
    }
    let start = unit.exec_start();
    let pre = all(&unit, "Service", "ExecStartPre");

    if is_installer {
        installer_rules(&unit, &start, &pre, &mut v);
        return out;
    }

    // Catches a unit outside the slice that holds the host's reservation.
    if unit.get_in("Service", "Slice").map(trim) != Some("kbf.slice") {
        v("slice", "not in Slice=kbf.slice".into());
    }
    // Catches the soft open-file limit of 1024: a build burst ends in EMFILE.
    if unit.get_in("Service", "LimitNOFILE").map(trim) != Some("1048576") {
        v("nofile", "no LimitNOFILE=1048576".into());
    }
    // Catches a server half run as the user builds run as: the frontend (the state store,
    // the operator token) and a storage node (the cold store's key) run as kbf-server, so an
    // action (run as kbf) cannot read what they hold; the modes that run builds run as kbf.
    // As kbf-deploy's units do today.
    let want = if mode.is_some_and(|m| ["frontend", "storage"].contains(&m)) {
        "kbf-server"
    } else {
        "kbf"
    };
    if unit.get_in("Service", "User").map(trim) != Some(want) {
        v("user", format!("User= is not {want}"));
    }
    // Catches a key given to the unit any way but kbf.toml: an EnvironmentFile=, an
    // Environment= with a key, a credential, or a MinIO unit it orders after.
    for d in unit.directives() {
        let value = d.value.to_ascii_lowercase();
        let bad = d.key.contains("EnvironmentFile")
            || d.key.contains("Credential")
            || (d.key == "Environment"
                && ["key", "secret", "token", "password"]
                    .iter()
                    .any(|w| value.contains(w)))
            || (["After", "Requires", "Wants", "BindsTo"].contains(&d.key.as_str())
                && value.contains("minio"));
        if bad {
            v("credential", format!("{}={}", d.key, d.value));
        }
    }
    // Catches a restart policy that gives up, and a restart of a command line or config
    // kbf refused (exit 2). Exit 75 (retryable) must restart, so it is never prevented.
    let restart = unit.get_in("Service", "Restart").map(trim);
    if restart != Some("on-failure") {
        v(
            "restart",
            format!("Restart= is {restart:?}, not on-failure"),
        );
    }
    let prevent = unit.get_in("Service", "RestartPreventExitStatus").map(trim);
    if prevent != Some("2") {
        v(
            "restart-exit",
            format!("RestartPreventExitStatus= is {prevent:?}, not 2"),
        );
    }

    if is_envoy {
        // Catches an Envoy that is not the host's prerequisite one (a binary of kbf's or the
        // installer's tree: Envoy is not part of a kbf release), or another configuration,
        // or a start that runs something before it.
        if !pre.is_empty() {
            v("envoy", format!("ExecStartPre= is {pre:?}, not none"));
        }
        if start.first() != Some(&ENVOY_BIN) || !start.windows(2).any(|w| w == ["-c", ENVOY_CONFIG])
        {
            v(
                "envoy",
                format!("ExecStart= is {start:?}, not {ENVOY_BIN} -c {ENVOY_CONFIG}"),
            );
        }
        if unit.get_in("Service", "RestartSec").map(trim) != Some("5") {
            v("restart", "no RestartSec=5".into());
        }
        if unit.get_in("Unit", "StartLimitIntervalSec").map(trim) != Some("0") {
            v(
                "restart-limit",
                "no StartLimitIntervalSec=0 in [Unit]".into(),
            );
        }
        return out;
    }

    let mode = mode.expect("a mode unit");
    // Catches a unit that starts another mode (or an old binary) under this mode's name,
    // a `--config` (kbf reads /etc/kbf/kbf.toml itself), or a flag the contract does not
    // give the mode: the frontend's instance and control socket only.
    let bin = if mode == "frontend" { SLOT } else { CURRENT };
    let kbf = format!("{bin}kbf");
    let mode_flag = format!("--mode={mode}");
    if start.get(..2) != Some(&[kbf.as_str(), mode_flag.as_str()][..]) {
        v(
            "exec",
            format!("ExecStart= is {start:?}, not {kbf} {mode_flag}"),
        );
    }
    // Catches a positional mode word (`kbf frontend`): kbf takes no subcommand, the mode
    // is the named flag --mode=. Read over every kbf command line of the unit.
    for (key, value) in &execs {
        let mut words = value.trim_start_matches('+').split_whitespace();
        if words.next().is_some_and(|p| p.ends_with("/kbf"))
            && let Some(w) = words.find(|w| !w.starts_with('-'))
        {
            v(
                "positional",
                format!("{key} gives the word `{w}` to kbf: use --mode="),
            );
        }
    }
    let flags: Vec<&str> = start.iter().skip(2).copied().collect();
    let allowed: &[&str] = if mode == "frontend" {
        &[
            "--instance=%i",
            "--handover-socket=/run/kbf-frontend/control.sock",
        ]
    } else {
        &[]
    };
    if flags != allowed {
        v(
            "flags",
            format!("ExecStart= flags are {flags:?}, not {allowed:?}"),
        );
    }
    // Catches a start without the two checks, in this order: boot-check first (root, counts
    // the start of a release on trial), then the mode's own `--check`. Written as ExecStartPre=, so a failure
    // fails the unit.
    let boot = format!("+{INSTALLER} boot-check");
    let check = format!("{kbf} --mode={mode} --check");
    let mut want: Vec<&str> = vec![boot.as_str()];
    want.push(check.as_str());
    // A frontend's check is made with the slot's kbf too.
    if pre != want {
        v(
            "gate-check",
            format!("ExecStartPre= is {pre:?}, not {want:?}"),
        );
    }
    // Catches kbf's own state in the wrong place: /var/lib/kbf is the unit's StateDirectory=,
    // which the installer never touches.
    // One directory per user: systemd chowns a StateDirectory= to the unit's user at every
    // start, so a host running a kbf-server unit and a kbf unit would hand one directory
    // back and forth.
    let dir = if ["frontend", "storage"].contains(&mode) {
        "kbf-server"
    } else {
        "kbf"
    };
    if unit.get_in("Service", "StateDirectory").map(trim) != Some(dir) {
        v("state-dir", format!("no StateDirectory={dir}"));
    }

    // Catches start-limit-hit on a failure that repeats quickly: every mode unit, the
    // frontend too, restarts at 5 s and has no start limit (a drained frontend instance
    // exits 0 and stays down regardless: Restart=on-failure never restarts a clean exit).
    {
        let sec = unit.get_in("Service", "RestartSec").map(trim);
        if !matches!(sec, Some("5" | "5s")) {
            v("restart", format!("RestartSec= is {sec:?}, not 5"));
        }
        if unit.get_in("Unit", "StartLimitIntervalSec").map(trim) != Some("0") {
            v(
                "restart-limit",
                "no StartLimitIntervalSec=0 in [Unit]".into(),
            );
        }
    }

    // Catches a stop that SIGKILLs a drain (every build in flight fails), and a daemon
    // that holds a stop for ever (it drains only to its deadline) or stops before it.
    let timeout = unit
        .directives()
        .iter()
        .rev()
        .find(|d| {
            d.section == "Service" && ["TimeoutSec", "TimeoutStopSec"].contains(&d.key.as_str())
        })
        .map(|d| trim(&d.value));
    let signal = unit.get_in("Service", "KillSignal").map(trim);
    if mode == "daemon" {
        // The default of `--drain-deadline-secs` (kbf-node, `Cli::drain_deadline_secs`);
        // kbf-it does not depend on kbf-node, so the number is repeated here.
        const DRAIN_DEADLINE_SECS: u64 = 600;
        match timeout.map(|t| t.parse::<u64>()) {
            Some(Ok(secs)) if secs > DRAIN_DEADLINE_SECS => {}
            _ => v(
                "stop",
                format!(
                    "TimeoutStopSec= is {timeout:?} on a daemon, not a finite number of \
                     seconds above its {DRAIN_DEADLINE_SECS} s drain deadline (0 and \
                     infinity mean no timeout, a smaller one SIGKILLs the drain)"
                ),
            ),
        }
        if !matches!(signal, Some("SIGTERM" | "TERM" | "15")) {
            v("stop", format!("KillSignal= is {signal:?}, not SIGTERM"));
        }
    } else {
        if timeout != Some("infinity") {
            v(
                "stop",
                format!("TimeoutStopSec= is {timeout:?}, not infinity"),
            );
        }
        if !matches!(signal, Some("SIGTERM" | "TERM" | "15")) {
            v("stop", format!("KillSignal= is {signal:?}, not SIGTERM"));
        }
    }

    // Catches a build node without the controllers the daemon's cgroups and
    // --lease-placement=numa-node need (cpuset above all), or one that disables them.
    if matches!(mode, "daemon" | "storage-and-daemon" | "all-in-one") {
        let set = delegated(&unit);
        for c in ["cpu", "cpuset", "memory", "pids"] {
            if !set.iter().any(|s| s == c) {
                v("delegate", format!("controller {c} is not delegated"));
            }
        }
        if unit
            .get("DisableControllers")
            .is_some_and(|x| !trim(x).is_empty())
        {
            v(
                "delegate",
                "DisableControllers= disables a controller".into(),
            );
        }
    }

    // Catches a storage node or frontend that may swap (it would slow every build at once).
    if matches!(mode, "frontend" | "storage")
        && unit.get_in("Service", "MemorySwapMax").map(trim) != Some("0")
    {
        v("swap", "no MemorySwapMax=0".into());
    }

    // Catches Type=notify where kbf sends no READY=1 (the start would hang to its
    // timeout), and a frontend without it (a failed handover would look like success).
    let kind = unit.get_in("Service", "Type").map(trim);
    if mode == "frontend" {
        if kind != Some("notify") {
            v("notify", format!("Type= is {kind:?}, not notify"));
        }
        if matches!(
            unit.get_in("Service", "NotifyAccess").map(trim),
            Some("none" | "exec")
        ) {
            v("notify", "NotifyAccess= drops kbf's READY=1".into());
        }
        let start_timeout = unit
            .directives()
            .iter()
            .rev()
            .find(|d| {
                d.section == "Service"
                    && ["TimeoutSec", "TimeoutStartSec"].contains(&d.key.as_str())
            })
            .map(|d| trim(&d.value));
        if start_timeout.is_none_or(|t| t == "infinity") {
            v(
                "notify",
                format!("start timeout {start_timeout:?} is not finite"),
            );
        }
        // Catches a start timeout shorter than the bind retry's budget plus the checks
        // before it: kbf waits up to 90 s for a listen address (kbf#998), and systemd would
        // kill that start mid-retry.
        let seconds = start_timeout.and_then(|t| t.trim_end_matches('s').parse::<u64>().ok());
        if start_timeout.is_some_and(|t| t != "infinity") && seconds.is_none_or(|s| s < 120) {
            v(
                "notify",
                format!(
                    "start timeout {start_timeout:?} is below 120 s (the 90 s bind budget and the checks)"
                ),
            );
        }
    } else if kind == Some("notify") {
        v(
            "notify",
            "Type=notify on a mode that sends no READY=1".into(),
        );
    }

    if mode == "frontend" {
        // Catches a frontend that cannot hand over: no template instance, no --instance,
        // a control socket outside a preserved RuntimeDirectory=.
        if !name.ends_with("@.service") {
            v("handover", "not a template unit".into());
        }
        if !start.contains(&"--instance=%i") {
            v(
                "handover",
                "no --instance=%i: a frontend without it refuses every handover".into(),
            );
        }
        let socket = start
            .iter()
            .find_map(|w| w.strip_prefix("--handover-socket="));
        let dirs: Vec<&str> = all(&unit, "Service", "RuntimeDirectory")
            .into_iter()
            .flat_map(str::split_whitespace)
            .collect();
        let inside = socket
            .and_then(|s| s.strip_prefix("/run/"))
            .and_then(|r| r.split_once('/'))
            .is_some_and(|(dir, file)| !file.is_empty() && dirs.contains(&dir));
        if !inside {
            v(
                "handover",
                format!("--handover-socket={socket:?} is not in RuntimeDirectory={dirs:?}"),
            );
        }
        if unit.get_in("Service", "RuntimeDirectoryPreserve").map(trim) != Some("yes") {
            v("handover", "no RuntimeDirectoryPreserve=yes".into());
        }
        if unit.get_in("Install", "DefaultInstance").map(trim) != Some("a") {
            v(
                "handover",
                "no DefaultInstance=a: only instance a starts at boot".into(),
            );
        }
    }
    // Catches Conflicts= (a start of one frontend instance would stop the draining other).
    if unit
        .get_in("Unit", "Conflicts")
        .is_some_and(|c| !trim(c).is_empty())
    {
        v("conflicts", "Conflicts= in [Unit]".into());
    }
    out
}

/// Rule `backup`: the state database's backup, a oneshot service and the timer that starts it
/// (docs/design/frontend-proxy.md section 11). Neither is a runtime unit, so none of the
/// runtime rules (network, slice, the checks, the restart policy) apply; these do.
fn backup_rules(name: &str, text: &str, v: &mut impl FnMut(&str, String)) {
    let unit = UnitFile::parse(text).unwrap_or_else(|e| panic!("{name}: {e}"));
    let mut bad = |why: String| v("backup", why);
    // Catches a backup that runs on a host with no installer.toml (no database to dump).
    if !all(&unit, "Unit", "ConditionPathExists").contains(&INSTALLER_CONFIG) {
        bad(format!(
            "no ConditionPathExists={INSTALLER_CONFIG} in [Unit]"
        ));
    }
    if name == "kbf-backup.timer" {
        // Catches a timer that never fires (no schedule), one that skips the run of a host that
        // was down, one nothing links at boot, and one that starts another unit.
        if all(&unit, "Timer", "OnCalendar")
            .iter()
            .all(|c| trim(c).is_empty())
        {
            bad("no OnCalendar= in [Timer]".into());
        }
        if unit.get_in("Timer", "Persistent").map(trim) != Some("true") {
            bad("no Persistent=true in [Timer]".into());
        }
        if !all(&unit, "Install", "WantedBy").contains(&"timers.target") {
            bad("no WantedBy=timers.target in [Install]".into());
        }
        if let Some(other) = unit.get_in("Timer", "Unit").map(trim)
            && other != "kbf-backup.service"
        {
            bad(format!("Unit= is {other}, not kbf-backup.service"));
        }
        return;
    }
    let get = |s: &str, k: &str| unit.get_in(s, k).map(trim);
    // Catches a service that is not run to completion (a forking or simple backup would be
    // `active` the moment it starts, and a failure after that would not fail the unit).
    if get("Service", "Type") != Some("oneshot") {
        bad("Type= is not oneshot".into());
    }
    // Catches a backup run as root or as the user builds run as: peer authentication makes
    // the user the database role, which is kbf-server's, and the backups are that user's alone.
    if get("Service", "User") != Some("kbf-server") {
        bad("User= is not kbf-server".into());
    }
    // Catches a backup that runs something else than the verb, or one that only says what it
    // would do (`--apply` is what makes the verb act).
    let start = unit.exec_start();
    if start != [INSTALLER, "--apply", "backup"] {
        bad(format!(
            "ExecStart= is {start:?}, not {INSTALLER} --apply backup"
        ));
    }
    // Catches a pre- or post-step, which would be a second thing the unit does as kbf-server.
    for key in ["ExecStartPre", "ExecStartPost", "ExecStop", "ExecCondition"] {
        if !all(&unit, "Service", key).is_empty() {
            bad(format!("{key}= in [Service]"));
        }
    }
    // Catches files made readable to others (the dump is the whole state, tokens included),
    // and a lost hardening key.
    for (k, want) in [
        ("UMask", "0077"),
        ("NoNewPrivileges", "yes"),
        ("ProtectSystem", "strict"),
        ("PrivateTmp", "yes"),
        ("StateDirectory", "kbf-server"),
        ("StateDirectoryMode", "0750"),
    ] {
        if get("Service", k) != Some(want) {
            bad(format!("{k}= is {:?}, not {want}", get("Service", k)));
        }
    }
    // Catches a service that is enabled on its own: it is started by its timer, and a
    // `systemctl enable` that linked it into multi-user.target would run a backup at every boot.
    if unit.directives().iter().any(|d| d.section == "Install") {
        bad("an [Install] section: the timer starts it".into());
    }
}

/// Rule `installer`: kbf-installer's own unit (contract section 7).
fn installer_rules(
    unit: &UnitFile,
    start: &[&str],
    pre: &[&str],
    v: &mut impl FnMut(&str, String),
) {
    // Catches an installer that does not check itself before it serves, runs something
    // else, or is not the plain service the contract names.
    let check = format!("{INSTALLER} check");
    if pre != [check.as_str()] {
        v(
            "installer",
            format!("ExecStartPre= is {pre:?}, not [{check}]"),
        );
    }
    if start != [INSTALLER, "serve"] {
        v(
            "installer",
            format!("ExecStart= is {start:?}, not {INSTALLER} serve"),
        );
    }
    let get = |k: &str| unit.get_in("Service", k).map(trim);
    if get("Type").is_some_and(|t| t != "simple") {
        v("installer", "Type= is not simple".into());
    }
    // Catches an installer that stays down after a crash, and one that restarts or fails
    // on exit 4 (busy: an activation is pending, which is not a failure).
    if get("Restart") != Some("on-failure") {
        v("installer", "Restart= is not on-failure".into());
    }
    if get("SuccessExitStatus") != Some("4") {
        v("installer", "no SuccessExitStatus=4".into());
    }
    // Catches a lost hardening key: a root service that can write anywhere, gain
    // privileges, or share /tmp.
    for (k, want) in [
        ("NoNewPrivileges", "yes"),
        ("ProtectSystem", "strict"),
        ("PrivateTmp", "yes"),
        (
            "ReadWritePaths",
            "/var/lib/kbf-installer /run/kbf-installer /var/cache/kbf-fetch",
        ),
    ] {
        if get(k) != Some(want) {
            v("installer", format!("{k}= is {:?}, not {want}", get(k)));
        }
    }
    // Catches a capability set that is not trimmed (empty, absent, or one that gives away
    // the host).
    match get("CapabilityBoundingSet") {
        None | Some("") => v("installer", "no CapabilityBoundingSet=".into()),
        Some(set) if set.contains("SYS_ADMIN") || set.contains("ALL") => {
            v(
                "installer",
                format!("CapabilityBoundingSet={set} is not trimmed"),
            );
        }
        Some(_) => {}
    }
    // Catches an installer that may start after the units whose release it must revert.
    let before = all(unit, "Unit", "Before").join(" ");
    for u in RUNTIME {
        if !before.split_whitespace().any(|b| *b == *u) {
            v("installer", format!("no Before={u}"));
        }
    }
}

fn rules_of(found: &[String]) -> Vec<&str> {
    found.iter().filter_map(|f| f.split(':').next()).collect()
}

/// Catches a file added to or removed from the package without this test being told.
#[test]
fn the_package_has_exactly_the_expected_files() {
    let mut have: Vec<String> = std::fs::read_dir(units_dir())
        .expect("packaging/linux/units")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    have.sort();
    assert_eq!(
        have, FILES,
        "update FILES and the per-mode rules with the package"
    );
}

/// Catches every defect `RULES` names, in the real files.
#[test]
fn every_unit_follows_every_rule() {
    let mut found = Vec::new();
    for name in FILES {
        found.extend(violations(name, &read(name)));
    }
    assert!(found.is_empty(), "violations:\n{}", found.join("\n"));
}

/// Catches a gate dropped from a service: the Condition on the config, and the checks.
#[test]
fn every_service_is_gated_on_its_config_and_its_checks() {
    let mut n = 0;
    for name in FILES.iter().filter(|n| n.ends_with(".service")) {
        let unit = UnitFile::parse(&read(name)).expect("parses");
        let config = if ["kbf-installer.service", "kbf-backup.service"].contains(name) {
            INSTALLER_CONFIG
        } else {
            CONFIG
        };
        assert!(
            all(&unit, "Unit", "ConditionPathExists").contains(&config),
            "{name}: no ConditionPathExists={config}"
        );
        // Envoy is the host's own (no kbf check of it at start yet: the supported version
        // range is checked by host preparation); every kbf unit runs its checks first.
        // The backup is a oneshot of the installer's own verb: its gate is the file alone (the
        // `backup` rule says what it may run).
        if !["kbf-envoy.service", "kbf-backup.service"].contains(name) {
            assert!(
                !all(&unit, "Service", "ExecStartPre").is_empty(),
                "{name}: no ExecStartPre= check"
            );
        }
        n += 1;
    }
    assert_eq!(n, 8, "a unit was not checked");
}

/// The modes whose units `files` hold, sorted.
fn modes_in<'a>(files: &[&'a str]) -> Vec<&'a str> {
    let mut m: Vec<&str> = files.iter().filter_map(|f| mode_of(f)).collect();
    m.sort_unstable();
    m
}

/// Catches the units drifting from the mode table of kbf-deploy: the modes the package
/// ships are exactly the five `kbf --mode=` takes (there is no `node` mode), each as
/// `kbf-<mode>.service`; an extra or a missing mode unit is not that set.
#[test]
fn the_kbf_modes_are_exactly_the_five() {
    let mut want: Vec<&str> = kbf_deploy::linux::Mode::ALL
        .iter()
        .map(|m| m.name())
        .collect();
    want.sort_unstable();
    assert_eq!(want.len(), 5);
    assert_eq!(modes_in(&FILES), want);
    // The mutants: a unit for a mode that does not exist, and one unit fewer.
    let mut extra = FILES.to_vec();
    extra.push("kbf-node.service");
    assert_ne!(modes_in(&extra), want, "an extra mode unit must not pass");
    let fewer: Vec<&str> = FILES
        .iter()
        .copied()
        .filter(|f| *f != "kbf-storage.service")
        .collect();
    assert_ne!(modes_in(&fewer), want, "a missing mode unit must not pass");
}

/// Catches the backup unit and the verb drifting apart: the unit's command line must parse
/// as the installer's own, as the verb `backup` with `--apply`, and its default directory must
/// be the one the unit's `StateDirectory=` makes (a backup written where systemd made no
/// directory would fail on the first run, or be written outside the unit's reach).
#[test]
fn the_backup_unit_runs_a_verb_the_installer_has() {
    use clap::Parser;
    use kbf_installer::cli::{Cli, Verb};

    let text = read("kbf-backup.service");
    let unit = UnitFile::parse(&text).expect("parses");
    let start = unit.exec_start();
    assert_eq!(start.first().copied(), Some(INSTALLER));
    let cli =
        Cli::try_parse_from(std::iter::once("kbf-installer").chain(start[1..].iter().copied()))
            .expect("the unit's command line is the installer's");
    assert!(
        cli.apply,
        "without --apply the verb only says what it would do"
    );
    let Verb::Backup(args) = cli.verb else {
        panic!("the unit does not run the backup verb");
    };
    let state = unit
        .get_in("Service", "StateDirectory")
        .map(trim)
        .expect("StateDirectory=");
    assert!(
        args.dir
            .to_string()
            .starts_with(&format!("/var/lib/{state}/")),
        "the backup directory {} is not under /var/lib/{state}",
        args.dir
    );
    assert_eq!(args.installer_config.to_string(), INSTALLER_CONFIG);
}

/// Catches the per-instance block of the frontend going missing from the template: the
/// two instances of kbf-deploy's `FRONTEND_INSTANCES` run the same line through `%i`.
#[test]
fn the_frontend_template_serves_both_instances_through_the_specifier() {
    assert_eq!(kbf_deploy::linux::modes::FRONTEND_INSTANCES, ["a", "b"]);
    let text = read("kbf-frontend@.service");
    let unit = UnitFile::parse(&text).expect("parses");
    assert_eq!(unit.get_in("Install", "DefaultInstance"), Some("a"));
    assert!(text.contains("--instance=%i"));
    assert!(!text.contains("--instance=a") && !text.contains("--instance=b"));
}

/// Catches a directory the installer unit may write that nothing makes: with
/// `ProtectSystem=strict` a `ReadWritePaths=` that does not exist fails the start (226).
/// `/var/lib/kbf-installer` is `setup`'s; the other two are tmpfiles.d's.
#[test]
fn the_installers_run_and_cache_directories_are_made_by_tmpfiles() {
    let path = units_dir().join("../tmpfiles.d/kbf-installer.conf");
    let text = std::fs::read_to_string(&path).expect("tmpfiles.d/kbf-installer.conf");
    let made: Vec<&str> = text
        .lines()
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| {
            let w: Vec<&str> = l.split_whitespace().collect();
            (w.first() == Some(&"d")).then(|| w[1])
        })
        .collect();
    assert_eq!(made, ["/run/kbf-installer", "/var/cache/kbf-fetch"]);
}

/// One defect per rule, planted in a copy of the real file: `(file, from, to, rule)`.
/// `to` replaces the first `from`; the rule named must report the copy.
const MUTANTS: &[(&str, &str, &str, &str)] = &[
    (
        "kbf-storage.service",
        "After=network-online.target\n",
        "After=network-online.target @MINIO_UNIT@\n",
        "placeholder",
    ),
    (
        "kbf-daemon.service",
        "ExecStart=/var/lib/kbf-installer/current/kbf --mode=daemon",
        "ExecStart=/var/lib/kbf-installer/current/kbf --mode=daemon \\\n    # a note\n    ",
        "continuation",
    ),
    (
        "kbf-daemon.service",
        "ExecStart=/var/lib/kbf-installer/current/kbf --mode=daemon",
        "ExecStart=/usr/local/bin/kbf daemon",
        "exec",
    ),
    (
        "kbf-daemon.service",
        "ExecStartPre=/var/lib/kbf-installer/current/kbf --mode=daemon --check",
        "ExecStartPre=-/var/lib/kbf-installer/current/kbf --mode=daemon --check",
        "exec-prefix",
    ),
    (
        "kbf-storage.service",
        "ExecStart=/var/lib/kbf-installer/current/kbf --mode=storage\n",
        "ExecStart=/var/lib/kbf-installer/current/kbf --mode=storage --config=/etc/kbf/kbf.toml\n",
        "flags",
    ),
    (
        "kbf-storage.service",
        "ConditionPathExists=/etc/kbf/kbf.toml\n",
        "",
        "gate-file",
    ),
    (
        "kbf-all-in-one.service",
        "ExecStartPre=/var/lib/kbf-installer/current/kbf --mode=all-in-one --check",
        "ExecCondition=/var/lib/kbf-installer/current/kbf --mode=all-in-one --check",
        "gate-skip",
    ),
    (
        "kbf-daemon.service",
        "ExecStartPre=+/usr/bin/kbf-installer boot-check\n",
        "",
        "gate-check",
    ),
    (
        "kbf-frontend@.service",
        "slots/%i/kbf --mode=frontend --instance",
        "slots/%i/kbf frontend --instance",
        "positional",
    ),
    (
        "kbf-storage.service",
        "Slice=kbf.slice",
        "Slice=system.slice",
        "slice",
    ),
    (
        "kbf-frontend@.service",
        "LimitNOFILE=1048576",
        "LimitNOFILE=1024",
        "nofile",
    ),
    (
        "kbf-storage.service",
        "\nUser=kbf-server\n",
        "\nUser=kbf\n",
        "user",
    ),
    (
        "kbf-daemon.service",
        "\nUser=kbf\n",
        "\nUser=kbf-server\n",
        "user",
    ),
    (
        "kbf-storage.service",
        "StateDirectory=kbf-server\n",
        "StateDirectory=kbf\n",
        "state-dir",
    ),
    (
        "kbf-daemon.service",
        "\nWantedBy=multi-user.target\n",
        "\n",
        "install",
    ),
    (
        "kbf-storage.service",
        "Wants=network-online.target\n",
        "",
        "network",
    ),
    (
        "kbf-frontend@.service",
        "TimeoutStartSec=120\n",
        "TimeoutStartSec=60\n",
        "notify",
    ),
    (
        "kbf-envoy.service",
        "StartLimitIntervalSec=0\n",
        "",
        "restart-limit",
    ),
    (
        "kbf-daemon.service",
        "\nDelegate=yes\n",
        "\nDelegate=yes\nEnvironmentFile=/etc/kbf/s3.env\n",
        "credential",
    ),
    (
        "kbf-storage-and-daemon.service",
        "\nRestart=on-failure\n",
        "\nRestart=always\n",
        "restart",
    ),
    (
        "kbf-storage.service",
        "StartLimitIntervalSec=0\n",
        "",
        "restart-limit",
    ),
    (
        "kbf-all-in-one.service",
        "RestartPreventExitStatus=2",
        "RestartPreventExitStatus=2 75",
        "restart-exit",
    ),
    (
        "kbf-frontend@.service",
        "\nTimeoutStopSec=infinity\n",
        "\nTimeoutStopSec=90\n",
        "stop",
    ),
    (
        "kbf-daemon.service",
        "\nTimeoutStopSec=660\n",
        "\nTimeoutStopSec=infinity\n",
        "stop",
    ),
    (
        "kbf-daemon.service",
        "\nKillSignal=SIGTERM\n",
        "\nKillSignal=SIGKILL\n",
        "stop",
    ),
    (
        "kbf-storage-and-daemon.service",
        "\nDelegate=yes\n",
        "\nDelegate=memory pids\n",
        "delegate",
    ),
    (
        "kbf-storage.service",
        "MemorySwapMax=0",
        "MemorySwapMax=1G",
        "swap",
    ),
    (
        "kbf-storage.service",
        "[Service]\n",
        "[Service]\n# rootless PodMan\n",
        "runtime",
    ),
    (
        "kbf-daemon.service",
        "[Service]\n",
        "[Service]\nType=notify\n",
        "notify",
    ),
    (
        "kbf-frontend@.service",
        "kbf --mode=frontend --instance=%i --handover",
        "kbf --mode=frontend --handover",
        "handover",
    ),
    (
        "kbf-frontend@.service",
        "[Unit]\nDescription",
        "[Unit]\nConflicts=kbf-frontend@b.service\nDescription",
        "conflicts",
    ),
    (
        "kbf-envoy.service",
        "ConditionPathExists=/etc/kbf/envoy/envoy.json\n",
        "",
        "envoy",
    ),
    (
        "kbf-envoy.service",
        "ExecStart=/usr/bin/envoy ",
        "ExecStart=/var/lib/kbf-installer/current/envoy ",
        "envoy",
    ),
    (
        "kbf.slice",
        "[Slice]\n",
        "[Slice]\nMemoryMin=8G\n",
        "memory",
    ),
    (
        "kbf-envoy.service",
        "\nRestart=on-failure\n",
        "\nRestart=on-failure\nExecReload=/usr/bin/systemctl restart kbf-envoy\n",
        "reload",
    ),
    (
        "kbf-installer.service",
        "NoNewPrivileges=yes\n",
        "",
        "installer",
    ),
    (
        "kbf-backup.service",
        "ConditionPathExists=/etc/kbf/installer.toml\n",
        "",
        "backup",
    ),
    (
        "kbf-backup.service",
        "Type=oneshot\n",
        "Type=simple\n",
        "backup",
    ),
    (
        "kbf-backup.service",
        "User=kbf-server\n",
        "User=root\n",
        "backup",
    ),
    (
        "kbf-backup.service",
        "ExecStart=/usr/bin/kbf-installer --apply backup\n",
        "ExecStart=/usr/bin/kbf-installer backup\n",
        "backup",
    ),
    (
        "kbf-backup.service",
        "ExecStart=/usr/bin/kbf-installer --apply backup\n",
        "ExecStartPre=/usr/bin/kbf-installer check\nExecStart=/usr/bin/kbf-installer --apply backup\n",
        "backup",
    ),
    (
        "kbf-backup.service",
        "UMask=0077\n",
        "UMask=0022\n",
        "backup",
    ),
    ("kbf-backup.service", "NoNewPrivileges=yes\n", "", "backup"),
    (
        "kbf-backup.service",
        "StateDirectoryMode=0750\n",
        "StateDirectoryMode=0755\n",
        "backup",
    ),
    (
        "kbf-backup.service",
        "CapabilityBoundingSet=\n",
        "CapabilityBoundingSet=\n\n[Install]\nWantedBy=multi-user.target\n",
        "backup",
    ),
    ("kbf-backup.timer", "Persistent=true\n", "", "backup"),
    ("kbf-backup.timer", "OnCalendar=daily\n", "", "backup"),
    (
        "kbf-backup.timer",
        "WantedBy=timers.target\n",
        "WantedBy=multi-user.target\n",
        "backup",
    ),
    (
        "kbf-backup.timer",
        "[Timer]\n",
        "[Timer]\nUnit=kbf-daemon.service\n",
        "backup",
    ),
    (
        "kbf-backup.timer",
        "ConditionPathExists=/etc/kbf/installer.toml\n",
        "",
        "backup",
    ),
];

/// Catches a rule in `RULES` that would pass a unit with its defect: each mutant must be
/// reported by the rule it names, and the unmutated file by none.
#[test]
fn each_planted_defect_is_reported_by_its_rule() {
    for (file, from, to, rule) in MUTANTS {
        let text = read(file);
        assert!(
            text.contains(from),
            "{file}: the mutant's `{from}` is not in the file"
        );
        let mutant = text.replacen(from, to, 1);
        assert_ne!(mutant, text);
        let found = violations(file, &mutant);
        assert!(
            rules_of(&found).contains(rule),
            "{file}: mutant `{from}` -> `{to}` was not reported by rule {rule}: {found:?}"
        );
    }
}

/// Catches a daemon stop timeout the rule lets through that cuts a drain short or holds a
/// stop for ever: none, `infinity`, `0`, the deadline itself (600), and a word that is not
/// seconds are refused; 601 and the shipped 660 are accepted. Refusing 660 would fail
/// the real unit; accepting 600 SIGKILLs a drain that runs its whole deadline.
#[test]
fn a_daemon_stop_timeout_must_exceed_the_drain_deadline() {
    let name = "kbf-daemon.service";
    let text = read(name);
    let shipped = "\nTimeoutStopSec=660\n";
    assert!(text.contains(shipped));
    let stop = |mutant: &str| {
        violations(name, mutant)
            .into_iter()
            .filter(|f| f.starts_with("stop:"))
            .count()
    };
    for ok in ["660", "601"] {
        let t = text.replacen(shipped, &format!("\nTimeoutStopSec={ok}\n"), 1);
        assert_eq!(stop(&t), 0, "{ok} was refused");
    }
    for bad in ["infinity", "0", "600", "90", "11min"] {
        let t = text.replacen(shipped, &format!("\nTimeoutStopSec={bad}\n"), 1);
        assert_eq!(stop(&t), 1, "{bad} was accepted");
    }
    let none = text.replacen(shipped, "\n", 1);
    assert_eq!(stop(&none), 1, "no TimeoutStopSec= was accepted");
}

/// Catches a rule with no mutant (a check that cannot be shown to fail), and a mutant
/// for a rule that does not exist.
#[test]
fn every_rule_has_a_mutant() {
    for rule in RULES {
        assert!(
            MUTANTS.iter().any(|m| m.3 == rule),
            "rule {rule} has no mutant"
        );
    }
    for m in MUTANTS {
        assert!(RULES.contains(&m.3), "mutant names no rule: {}", m.3);
    }
}
