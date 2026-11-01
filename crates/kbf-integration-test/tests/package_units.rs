//! The static unit files of the kbf-installer and kbf packages (`packaging/linux/units/`)
//! against the rules `kbf-deploy linux check-units` (retired) enforced on the units it rendered,
//! and against the kbf-installer contract (docs/design/kbf-installer.md, section 7).
//!
//! The packages ship these files disabled, with no rendering: kbf-installer's own unit and the
//! proxy units (the kbf-installer package), and each mode's unit, the frontend's included (the
//! kbf package, which runs `/usr/bin/kbf`), gated on `/etc/kbf/kbf.toml`
//! (`ConditionPathExists=`), on `kbf boot-check --mode=<mode>` and on `kbf --mode=<mode> --check`
//! (both `ExecStartPre=`; `ExecCondition=` would skip a failed check silently).
//! The rules are held here, over the unit parser (`kbf_unitfile`) the docs check uses, each with the defect it catches, and `DEFECTS`
//! plants one defect per rule in a copy of the real file and requires that rule to name
//! it. `every_rule_has_a_defect` fails when a rule is added with no defect, so no rule
//! can be one that cannot fail.
//!
//! Catches:
//! - a unit missing a rule that its mode needs (`restart`, `stop`, `kill-mode`,
//!   `delegate`, `credential`, `caps`, `swap`, `oom`, `cpuset`, user, slice,
//!   `LimitNOFILE`), each named below;
//! - a unit that runs a program other than `/usr/bin/kbf` and the installer's binary (the
//!   installer's own unit and the proxy's), another mode than its name, a `--config` or any
//!   flag the contract does not give it;
//! - kbf-installer's own unit running anything but `run`, or gating its start on a step that is
//!   not built; the verbs every unit and the macOS property list name exist, and the runtime
//!   units' program is `/usr/bin/kbf`: `package_installer_verbs.rs`;
//! - a unit that starts without its gate (`ConditionPathExists=`, the two checks, in
//!   order), or whose gate is `ExecCondition=` or a `-`-prefixed command that ignores a
//!   failure, or whose `boot-check` is not `kbf boot-check` for the unit's own mode, run as
//!   root;
//! - kbf-installer's own unit losing a hardening key or its place before the runtime units;
//! - a kbf-deploy placeholder (`@KEY@`, `<value>`) left in a file that is not rendered;
//! - a file added or removed without this test being told.
//!
//! Dependencies on work not on the branch yet: `kbf --mode=<mode> --check` and
//! `--instance` (P12, P13); `kbf-cli/tests/package_units.rs` holds
//! the parse of each mode's flags by `kbf`'s own parser, ignored until they land.

use std::path::{Path, PathBuf};

use kbf_unitfile::{UnitFile, comments_in_continuations};

/// The proxy's two units: `kbf-proxy.socket` and `kbf-proxy.service`.
#[path = "package_units/proxy.rs"]
mod proxy;

/// The `kbf` the kbf package installs: the program of every mode unit, which no release
/// tree holds.
const KBF: &str = "/usr/bin/kbf";
/// The installer binary the units call.
const INSTALLER: &str = "/usr/bin/kbf-installer";
/// The configuration every runtime unit is gated on and every mode reads.
const CONFIG: &str = "/etc/kbf/kbf.toml";
/// The installer's own configuration, which `setup` writes.
const INSTALLER_CONFIG: &str = "/etc/kbf/installer.toml";

/// The files of the package, and nothing else.
const FILES: [&str; 9] = [
    "kbf-all-in-one.service",
    "kbf-daemon.service",
    "kbf-frontend.service",
    "kbf-installer.service",
    "kbf-proxy.service",
    "kbf-proxy.socket",
    "kbf-storage-and-daemon.service",
    "kbf-storage.service",
    "kbf.slice",
];

/// The runtime units kbf-installer starts before (`Before=` of its unit). The proxy is not one:
/// it runs no release.
const RUNTIME: [&str; 5] = [
    "kbf-all-in-one.service",
    "kbf-daemon.service",
    "kbf-frontend.service",
    "kbf-storage-and-daemon.service",
    "kbf-storage.service",
];

/// The rules, by the tag a violation starts with.
const RULES: [&str; 36] = [
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
    "state-dir-search",
    "credential",
    "restart",
    "restart-limit",
    "restart-exit",
    "stop",
    "kill-mode",
    "delegate",
    "notify",
    "conflicts",
    "swap",
    "caps",
    "oom",
    "cpuset",
    "memory",
    "reload",
    "installer",
    "runtime",
    "proxyless",
    "config",
    "slots",
];

fn units_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging/linux/units")
}

fn read(name: &str) -> String {
    let path = units_dir().join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// The mode a unit file starts: the word after `kbf`. The installer's and the proxy's
/// units start no mode.
fn mode_of(name: &str) -> Option<&str> {
    let stem = name.strip_suffix(".service")?;
    let mode = stem.strip_prefix("kbf-")?;
    (!["installer", "proxy"].contains(&mode)).then_some(mode)
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

/// systemd's `Delegate=` as `config_parse_delegate` reads it (see package rule
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
    if name.ends_with(".socket") {
        let unit = UnitFile::parse(text).unwrap_or_else(|e| panic!("{name}: {e}"));
        proxy::socket_rules(&unit, &mut v);
        // Catches a socket that runs a program (it only holds a port).
        if unit.directives().iter().any(|d| d.key.starts_with("Exec")) {
            v("proxy-socket-pair", "an Exec* directive in a socket".into());
        }
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
        // Catches a slice that disables a controller below it (package rule
        // `delegate`; a slice cannot delegate) or narrows the CPUs or NUMA nodes of
        // every unit in it (`cpuset`; the node's own set is a per-host drop-in, never
        // this static file).
        if unit
            .get("DisableControllers")
            .is_some_and(|x| !trim(x).is_empty())
        {
            v("delegate", "kbf.slice sets DisableControllers=".into());
        }
        for key in ["AllowedCPUs", "AllowedMemoryNodes"] {
            if unit.get(key).is_some_and(|x| !trim(x).is_empty()) {
                v("cpuset", format!("kbf.slice sets {key}="));
            }
        }
        return out;
    }
    let unit = UnitFile::parse(text).unwrap_or_else(|e| panic!("{name}: {e}"));
    let is_installer = name == "kbf-installer.service";
    let is_proxy = name == "kbf-proxy.service";
    let mode = mode_of(name);

    // Catches a unit that starts with no configuration: ConditionPathExists= skips it.
    let conditions = all(&unit, "Unit", "ConditionPathExists");
    let own_config = if is_installer || is_proxy {
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

    // Every program the unit runs is a package's own file: the kbf-installer package replaces
    // /usr/bin/kbf-installer, the kbf package replaces /usr/bin/kbf. Only the root check
    // (`kbf boot-check`) carries a `+`; a `-` would ignore a failed gate, and `!`, `@`, `:`
    // change the sandbox.
    // The proxy's reload is the one `ExecReload` a unit may have, and it runs a program outside
    // the packages' files (`/bin/kill`): `proxy-reload` pins its exact text, so the generic
    // exec and reload rules skip that one key there.
    let execs: Vec<(&str, &str)> = unit
        .directives()
        .iter()
        .filter(|d| d.section == "Service" && d.key.starts_with("Exec"))
        .filter(|d| !(is_proxy && d.key == "ExecReload"))
        .map(|d| (d.key.as_str(), trim(&d.value)))
        .collect();
    for (key, value) in &execs {
        let plus_ok = value.starts_with(&format!("+{KBF} boot-check"));
        if value.starts_with(['-', '!', '@', ':']) || (value.starts_with('+') && !plus_ok) {
            v("exec-prefix", format!("{key}={value}"));
        }
        let program = value
            .trim_start_matches('+')
            .split_whitespace()
            .next()
            .unwrap_or("");
        let allowed = program == KBF || program == INSTALLER;
        if !allowed {
            v(
                "exec",
                format!("{key} runs {program}, not {KBF} or {INSTALLER}"),
            );
        }
    }
    // Catches a reload that restarts: `systemctl restart` (or a reload that runs one) cuts
    // every open stream of the process it restarts. No unit has a reload; the proxy's is
    // SIGHUP, under `proxy-reload`.
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
    if is_proxy {
        proxy::service_rules(&unit, &start, &pre, &mut v);
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
    // As the retired kbf-deploy units did.
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

    let mode = mode.expect("a mode unit");
    // The frontend for a host without the proxy: no handover, the packaged `/usr/bin/kbf`,
    // its configuration named by `--config`.
    let plain = name == "kbf-frontend.service";
    // Catches a unit that starts another mode (or an old binary) under this mode's name,
    // a `--config` (kbf reads /etc/kbf/kbf.toml itself), or a flag the contract does not
    // give the mode: the frontend's `--config` only.
    let mode_flag = format!("--mode={mode}");
    if start.get(..2) != Some(&[KBF, mode_flag.as_str()][..]) {
        v(
            "exec",
            format!("ExecStart= is {start:?}, not {KBF} {mode_flag}"),
        );
    }
    // Catches a positional mode word (`kbf frontend`): kbf takes no subcommand, the mode
    // is the named flag --mode=. Read over every kbf command line of the unit.
    for (key, value) in &execs {
        let mut words = value.trim_start_matches('+').split_whitespace();
        let mut after_config = false;
        // The root check's word is `boot-check`; the rule `gate-check` pins the line it is in.
        let verb = *key == "ExecStartPre";
        if words.next().is_some_and(|p| p.ends_with("/kbf"))
            && let Some(w) = words.find(|w| {
                // `--config FILE` takes its value as the next word.
                let is_value = std::mem::replace(&mut after_config, *w == "--config");
                !(w.starts_with('-') || is_value || (verb && *w == "boot-check"))
            })
        {
            v(
                "positional",
                format!("{key} gives the word `{w}` to kbf: use --mode="),
            );
        }
    }
    let flags: Vec<&str> = start.iter().skip(2).copied().collect();
    let allowed: &[&str] = if plain { &["--config", CONFIG] } else { &[] };
    if flags != allowed {
        v(
            "flags",
            format!("ExecStart= flags are {flags:?}, not {allowed:?}"),
        );
    }
    // Catches a start without the two checks, in this order: boot-check first (root, `kbf
    // boot-check` brings the state database to the schema the kbf at /usr/bin/kbf needs, for the
    // mode the unit runs), then the mode's own `--check`. Written as ExecStartPre=, so a failure
    // fails the unit.
    let boot = format!("+{KBF} boot-check --mode={mode}");
    let check = format!("{KBF} --mode={mode} --check");
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
    // Catches a container-running unit whose state directory others cannot search: systemd
    // applies StateDirectoryMode= at every start, and the container driver refuses to start
    // unless every directory down to its lease directories (under /var/lib/kbf) has the
    // others-execute bit (its container ids are not the unit user's), so the node would
    // crash-loop. The bits are read as octal; a unit with no StateDirectoryMode= gets
    // systemd's 0755, which passes.
    if matches!(mode, "daemon" | "storage-and-daemon" | "all-in-one") {
        let bits = unit.get_in("Service", "StateDirectoryMode").map(trim);
        let searchable = match bits {
            None => true,
            Some(m) => u32::from_str_radix(m, 8).is_ok_and(|b| b & 0o001 != 0),
        };
        if !searchable {
            v(
                "state-dir-search",
                format!("StateDirectoryMode= is {bits:?}, which others cannot search (need o+x)"),
            );
        }
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

    // Catches a stop that SIGKILLs a drain (every build in flight fails, and the daemon's
    // leases are abandoned): every mode unit waits for its drain without end, the daemon's
    // too (it has no default drain deadline).
    let timeout = unit
        .directives()
        .iter()
        .rev()
        .find(|d| {
            d.section == "Service" && ["TimeoutSec", "TimeoutStopSec"].contains(&d.key.as_str())
        })
        .map(|d| trim(&d.value));
    let signal = unit.get_in("Service", "KillSignal").map(trim);
    if timeout != Some("infinity") {
        v(
            "stop",
            format!("TimeoutStopSec= is {timeout:?}, not infinity"),
        );
    }
    if !matches!(signal, Some("SIGTERM" | "TERM" | "15")) {
        v("stop", format!("KillSignal= is {signal:?}, not SIGTERM"));
    }

    // Catches a node unit that signals the actions with the daemon: with KillMode=
    // control-group (the default) a stop sends SIGTERM to every process of the unit, the
    // actions in their sub-cgroups among them, and the daemon's drain ends in seconds.
    // process would leave them for ever, so only mixed is accepted.
    if matches!(mode, "daemon" | "storage-and-daemon" | "all-in-one") {
        let kill_mode = unit.get_in("Service", "KillMode").map(trim);
        if kill_mode != Some("mixed") {
            v(
                "kill-mode",
                format!("KillMode= is {kill_mode:?}, not mixed"),
            );
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

    // Catches a limit on a build node's own unit (package rules `caps` and `swap`):
    // its leases live in the unit's cgroup, so the builds are capped by the daemon's
    // actions/memory.max alone and a limit here would cap the supervisor with them; and
    // a MemorySwapMax= would turn the daemon's swap watch into an OOM kill of the unit
    // that holds every lease. An empty value is unset, as systemd reads it.
    if matches!(mode, "daemon" | "storage-and-daemon" | "all-in-one") {
        for key in ["MemoryMax", "MemoryHigh", "CPUQuota", "AllowedCPUs"] {
            if unit.get(key).is_some_and(|x| !trim(x).is_empty()) {
                v("caps", format!("sets {key}= on a build node's unit"));
            }
        }
        if unit
            .get("MemorySwapMax")
            .is_some_and(|x| !trim(x).is_empty())
        {
            v("swap", "a build node sets MemorySwapMax=".into());
        }
        // Catches a build node whose OOM score is not below 0, the build containers'
        // score (package rule `oom`): a host-wide OOM kills the highest score
        // first, so a lease must die before the daemon that holds them.
        match unit
            .get("OOMScoreAdjust")
            .map(trim)
            .filter(|x| !x.is_empty())
        {
            None => v("oom", "no OOMScoreAdjust= (it defaults to 0)".into()),
            Some(x) => match x.parse::<i64>() {
                Ok(n) if n < 0 => {}
                Ok(_) => v("oom", format!("OOMScoreAdjust={x} is not below 0")),
                Err(_) => v("oom", format!("OOMScoreAdjust={x} is not an integer")),
            },
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
        // Catches a start timeout shorter than the state store wait (300 s by default) plus the
        // bind retry's budget and the checks: kbf waits up to 300 s for a state store that is
        // not up yet and up to 90 s for a listen address (kbf#998), and systemd would kill
        // that start mid-wait instead of letting it end with exit 1.
        let seconds = start_timeout.and_then(|t| t.trim_end_matches('s').parse::<u64>().ok());
        if start_timeout.is_some_and(|t| t != "infinity") && seconds.is_none_or(|s| s < 420) {
            v(
                "notify",
                format!(
                    "start timeout {start_timeout:?} is below 420 s (the 300 s state store wait, the 90 s bind budget and the checks)"
                ),
            );
        }
    } else if kind == Some("notify") {
        v(
            "notify",
            "Type=notify on a mode that sends no READY=1".into(),
        );
    }

    if plain {
        // Catches a plain frontend that still carries the handover model: a template, an
        // instance and a control socket (flags kbf still accepts, ignores and warns of), a
        // RuntimeDirectory= for it, a `%i`. Read over every line of the unit's commands.
        if name.contains('@') {
            v("proxyless", "a template unit".into());
        }
        for (key, value) in &execs {
            for flag in ["--instance", "--handover-socket"] {
                if value.contains(flag) {
                    v("proxyless", format!("{key} gives {flag}: {value}"));
                }
            }
            if value.contains("%i") {
                v(
                    "proxyless",
                    format!("{key} uses the instance specifier: {value}"),
                );
            }
            // Catches a command that runs a slot: with no slots the link is not there.
            if value.contains("/slots/") {
                v("slots", format!("{key} runs a slot: {value}"));
            }
        }
        if !all(&unit, "Service", "RuntimeDirectory").is_empty() {
            v(
                "proxyless",
                "RuntimeDirectory= (the handover socket's)".into(),
            );
        }
        // Catches a start that does not read kbf.toml as the contract has it: exactly
        // `--config /etc/kbf/kbf.toml`, the two words in order.
        if !start.windows(2).any(|w| w == ["--config", CONFIG]) {
            v(
                "config",
                format!("ExecStart= is {start:?}, without --config {CONFIG}"),
            );
        }
    }
    // Catches Conflicts= (a start of one unit would stop the other, e.g. the frontend the proxy runs).
    if unit
        .get_in("Unit", "Conflicts")
        .is_some_and(|c| !trim(c).is_empty())
    {
        v("conflicts", "Conflicts= in [Unit]".into());
    }
    out
}

/// Rule `installer`: kbf-installer's own unit (contract section 7).
fn installer_rules(
    unit: &UnitFile,
    start: &[&str],
    pre: &[&str],
    v: &mut impl FnMut(&str, String),
) {
    // Catches an installer unit that runs something else than `run` (the verb the contract
    // names; `serve` does not exist), or one that gates its start on a step that is not built
    // (`check` exits 1 "not implemented"): either way the unit cannot start. Which verbs
    // exist, and that they parse, is `package_installer_verbs.rs`'s.
    if !pre.is_empty() {
        v("installer", format!("ExecStartPre= is {pre:?}, not none"));
    }
    if start != [INSTALLER, "run"] {
        v(
            "installer",
            format!("ExecStart= is {start:?}, not {INSTALLER} run"),
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
        let config = if ["kbf-installer.service", "kbf-proxy.service"].contains(name) {
            INSTALLER_CONFIG
        } else {
            CONFIG
        };
        assert!(
            all(&unit, "Unit", "ConditionPathExists").contains(&config),
            "{name}: no ConditionPathExists={config}"
        );
        // Every kbf unit runs its checks first. The installer's own unit has none: its
        // `check` is not built, and a rule pins that it names no ExecStartPre= (a stub there
        // fails every start).
        if *name != "kbf-installer.service" {
            assert!(
                !all(&unit, "Service", "ExecStartPre").is_empty(),
                "{name}: no ExecStartPre= check"
            );
        }
        n += 1;
    }
    assert_eq!(n, 7, "a unit was not checked");
}

/// The modes whose units `files` hold, sorted.
fn modes_in<'a>(files: &[&'a str]) -> Vec<&'a str> {
    let mut m: Vec<&str> = files.iter().filter_map(|f| mode_of(f)).collect();
    m.sort_unstable();
    m
}

/// Catches the units drifting from the mode table of kbf-cli (`names::MODES`): the modes the package
/// ships are exactly the five `kbf --mode=` takes (there is no `node` mode), each as
/// `kbf-<mode>.service`; an extra or a missing mode unit is not that set.
#[test]
fn the_kbf_modes_are_exactly_the_five() {
    let mut want: Vec<&str> = kbf_cli::names::MODES.iter().map(|m| m.name).collect();
    want.sort_unstable();
    assert_eq!(want.len(), 5);
    assert_eq!(modes_in(&FILES), want);
    // The defects: a unit for a mode that does not exist, and one unit fewer.
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
const DEFECTS: &[(&str, &str, &str, &str)] = &[
    (
        "kbf-storage.service",
        "After=network-online.target\n",
        "After=network-online.target @MINIO_UNIT@\n",
        "placeholder",
    ),
    (
        "kbf-daemon.service",
        "ExecStart=/usr/bin/kbf --mode=daemon",
        "ExecStart=/usr/bin/kbf --mode=daemon \\\n    # a note\n    ",
        "continuation",
    ),
    (
        "kbf-daemon.service",
        "ExecStart=/usr/bin/kbf --mode=daemon",
        "ExecStart=/usr/local/bin/kbf daemon",
        "exec",
    ),
    (
        "kbf-daemon.service",
        "ExecStartPre=/usr/bin/kbf --mode=daemon --check",
        "ExecStartPre=-/usr/bin/kbf --mode=daemon --check",
        "exec-prefix",
    ),
    (
        "kbf-storage.service",
        "ExecStart=/usr/bin/kbf --mode=storage\n",
        "ExecStart=/usr/bin/kbf --mode=storage --config=/etc/kbf/kbf.toml\n",
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
        "ExecStartPre=/usr/bin/kbf --mode=all-in-one --check",
        "ExecCondition=/usr/bin/kbf --mode=all-in-one --check",
        "gate-skip",
    ),
    (
        "kbf-daemon.service",
        "ExecStartPre=+/usr/bin/kbf boot-check --mode=daemon\n",
        "",
        "gate-check",
    ),
    (
        "kbf-storage.service",
        "Slice=kbf.slice",
        "Slice=system.slice",
        "slice",
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
        "kbf-frontend.service",
        "\nTimeoutStopSec=infinity\n",
        "\nTimeoutStopSec=90\n",
        "stop",
    ),
    (
        "kbf-frontend.service",
        "\nKillSignal=SIGTERM\n",
        "\nKillSignal=SIGKILL\n",
        "stop",
    ),
    (
        "kbf-frontend.service",
        "\nType=notify\n",
        "\nType=simple\n",
        "notify",
    ),
    (
        "kbf-frontend.service",
        "\nNotifyAccess=main\n",
        "\nNotifyAccess=none\n",
        "notify",
    ),
    (
        "kbf-frontend.service",
        "\nTimeoutStartSec=420\n",
        "\nTimeoutStartSec=60\n",
        "notify",
    ),
    (
        "kbf-daemon.service",
        "\nTimeoutStopSec=infinity\n",
        "\nTimeoutStopSec=660\n",
        "stop",
    ),
    (
        "kbf-daemon.service",
        "\nKillSignal=SIGTERM\n",
        "\nKillSignal=SIGKILL\n",
        "stop",
    ),
    (
        "kbf-daemon.service",
        "\nKillMode=mixed\n",
        "\n",
        "kill-mode",
    ),
    (
        "kbf-daemon.service",
        "\nKillMode=mixed\n",
        "\nKillMode=control-group\n",
        "kill-mode",
    ),
    (
        "kbf-storage-and-daemon.service",
        "\nKillMode=mixed\n",
        "\n",
        "kill-mode",
    ),
    (
        "kbf-storage-and-daemon.service",
        "\nKillMode=mixed\n",
        "\nKillMode=control-group\n",
        "kill-mode",
    ),
    (
        "kbf-all-in-one.service",
        "\nKillMode=mixed\n",
        "\n",
        "kill-mode",
    ),
    (
        "kbf-all-in-one.service",
        "\nKillMode=mixed\n",
        "\nKillMode=control-group\n",
        "kill-mode",
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
        "kbf-daemon.service",
        "OOMScoreAdjust=-900",
        "OOMScoreAdjust=-900\nMemorySwapMax=0",
        "swap",
    ),
    (
        "kbf-all-in-one.service",
        "OOMScoreAdjust=-925",
        "OOMScoreAdjust=-925\nMemorySwapMax=0",
        "swap",
    ),
    (
        "kbf-daemon.service",
        "OOMScoreAdjust=-900",
        "OOMScoreAdjust=-900\nMemoryMax=64G",
        "caps",
    ),
    (
        "kbf-storage-and-daemon.service",
        "OOMScoreAdjust=-925",
        "OOMScoreAdjust=-925\nMemoryHigh=64G",
        "caps",
    ),
    (
        "kbf-all-in-one.service",
        "OOMScoreAdjust=-925",
        "OOMScoreAdjust=-925\nCPUQuota=400%",
        "caps",
    ),
    (
        "kbf-daemon.service",
        "OOMScoreAdjust=-900",
        "OOMScoreAdjust=-900\nAllowedCPUs=0-3",
        "caps",
    ),
    (
        "kbf-daemon.service",
        "OOMScoreAdjust=-900",
        "OOMScoreAdjust=0",
        "oom",
    ),
    (
        "kbf-storage-and-daemon.service",
        "OOMScoreAdjust=-925",
        "OOMScoreAdjust=100",
        "oom",
    ),
    (
        "kbf-all-in-one.service",
        "OOMScoreAdjust=-925",
        "OOMScoreAdjust=high",
        "oom",
    ),
    ("kbf-daemon.service", "OOMScoreAdjust=-900\n", "", "oom"),
    (
        "kbf.slice",
        "[Slice]\n",
        "[Slice]\nDisableControllers=cpuset\n",
        "delegate",
    ),
    (
        "kbf.slice",
        "[Slice]\n",
        "[Slice]\nAllowedCPUs=0-3\n",
        "cpuset",
    ),
    (
        "kbf.slice",
        "[Slice]\n",
        "[Slice]\nAllowedMemoryNodes=0\n",
        "cpuset",
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
        "kbf-frontend.service",
        "[Unit]\nDescription",
        "[Unit]\nConflicts=kbf-proxy.service\nDescription",
        "conflicts",
    ),
    (
        "kbf.slice",
        "[Slice]\n",
        "[Slice]\nMemoryMin=8G\n",
        "memory",
    ),
    (
        "kbf-frontend.service",
        "\nRestart=on-failure\n",
        "\nRestart=on-failure\nExecReload=/usr/bin/systemctl restart kbf-frontend\n",
        "reload",
    ),
    (
        "kbf-frontend.service",
        "ExecStart=/usr/bin/kbf --mode=frontend --config /etc/kbf/kbf.toml",
        "ExecStart=/var/lib/kbf-installer/slots/a/kbf --mode=frontend --config /etc/kbf/kbf.toml",
        "slots",
    ),
    (
        "kbf-frontend.service",
        "ExecStart=/usr/bin/kbf --mode=frontend --config /etc/kbf/kbf.toml",
        "ExecStart=/var/lib/kbf-installer/slots/a/kbf --mode=frontend --config /etc/kbf/kbf.toml",
        "exec",
    ),
    (
        "kbf-frontend.service",
        "ExecStart=/usr/bin/kbf --mode=frontend --config /etc/kbf/kbf.toml",
        "ExecStart=/var/lib/kbf-installer/current/kbf --mode=frontend --config /etc/kbf/kbf.toml",
        "exec",
    ),
    (
        "kbf-frontend.service",
        "kbf --mode=frontend --config",
        "kbf --mode=daemon --config",
        "exec",
    ),
    (
        "kbf-frontend.service",
        "kbf --mode=frontend --config",
        "kbf frontend --config",
        "positional",
    ),
    (
        "kbf-frontend.service",
        "--config /etc/kbf/kbf.toml",
        "--config /etc/kbf/kbf.toml --handover-socket=/run/kbf-frontend/control.sock",
        "proxyless",
    ),
    (
        "kbf-frontend.service",
        "--config /etc/kbf/kbf.toml",
        "--config /etc/kbf/kbf.toml --instance=a",
        "proxyless",
    ),
    (
        "kbf-frontend.service",
        "--config /etc/kbf/kbf.toml",
        "--config /etc/kbf/kbf.toml --instance=%i",
        "proxyless",
    ),
    (
        "kbf-frontend.service",
        "[Service]\n",
        "[Service]\nRuntimeDirectory=kbf-frontend\n",
        "proxyless",
    ),
    (
        "kbf-frontend.service",
        "--config /etc/kbf/kbf.toml",
        "--config /etc/kbf/other.toml",
        "config",
    ),
    (
        "kbf-frontend.service",
        " --config /etc/kbf/kbf.toml",
        "",
        "config",
    ),
    (
        "kbf-frontend.service",
        "--config /etc/kbf/kbf.toml",
        "--config=/etc/kbf/kbf.toml",
        "config",
    ),
    (
        "kbf-frontend.service",
        "ExecStartPre=/usr/bin/kbf --mode=frontend --check",
        "ExecStartPre=/var/lib/kbf-installer/slots/a/kbf --mode=frontend --check",
        "gate-check",
    ),
    (
        "kbf-frontend.service",
        "ExecStartPre=/usr/bin/kbf --mode=frontend --check",
        "ExecStartPre=/var/lib/kbf-installer/slots/a/kbf --mode=frontend --check",
        "slots",
    ),
    (
        "kbf-frontend.service",
        "\nUser=kbf-server\n",
        "\nUser=kbf\n",
        "user",
    ),
    (
        "kbf-frontend.service",
        "TimeoutStopSec=infinity",
        "TimeoutStopSec=90",
        "stop",
    ),
    (
        "kbf-frontend.service",
        "Type=notify",
        "Type=simple",
        "notify",
    ),
    (
        "kbf-frontend.service",
        "ExecStartPre=+/usr/bin/kbf boot-check --mode=frontend\n",
        "",
        "gate-check",
    ),
    (
        "kbf-frontend.service",
        "MemorySwapMax=0",
        "MemorySwapMax=1G",
        "swap",
    ),
    (
        "kbf-frontend.service",
        "ConditionPathExists=/etc/kbf/kbf.toml\n",
        "",
        "gate-file",
    ),
    (
        "kbf-frontend.service",
        "StartLimitIntervalSec=0\n",
        "",
        "restart-limit",
    ),
    (
        "kbf-frontend.service",
        "Restart=on-failure",
        "Restart=always",
        "restart",
    ),
    (
        "kbf-frontend.service",
        "RestartPreventExitStatus=2",
        "RestartPreventExitStatus=",
        "restart-exit",
    ),
    (
        "kbf-frontend.service",
        "LimitNOFILE=1048576",
        "LimitNOFILE=1024",
        "nofile",
    ),
    (
        "kbf-frontend.service",
        "StateDirectory=kbf-server\n",
        "StateDirectory=kbf\n",
        "state-dir",
    ),
    (
        "kbf-daemon.service",
        "StateDirectoryMode=0711\n",
        "StateDirectoryMode=0750\n",
        "state-dir-search",
    ),
    (
        "kbf-storage-and-daemon.service",
        "StateDirectoryMode=0711\n",
        "StateDirectoryMode=0700\n",
        "state-dir-search",
    ),
    (
        "kbf-all-in-one.service",
        "StateDirectoryMode=0711\n",
        "StateDirectoryMode=0750\n",
        "state-dir-search",
    ),
    (
        "kbf-frontend.service",
        "Slice=kbf.slice",
        "Slice=system.slice",
        "slice",
    ),
    (
        "kbf-installer.service",
        "NoNewPrivileges=yes\n",
        "",
        "installer",
    ),
    (
        "kbf-installer.service",
        "ExecStart=/usr/bin/kbf-installer run",
        "ExecStart=/usr/bin/kbf-installer serve",
        "installer",
    ),
    (
        "kbf-installer.service",
        "ExecStart=/usr/bin/kbf-installer run",
        "ExecStartPre=/usr/bin/kbf-installer check\nExecStart=/usr/bin/kbf-installer run",
        "installer",
    ),
];

/// Catches a rule in `RULES` that would pass a unit with its defect: each defect must be
/// reported by the rule it names, and the unmutated file by none.
#[test]
fn each_planted_defect_is_reported_by_its_rule() {
    for (file, from, to, rule) in DEFECTS {
        let text = read(file);
        assert!(
            text.contains(from),
            "{file}: the defect's `{from}` is not in the file"
        );
        let broken = text.replacen(from, to, 1);
        assert_ne!(broken, text);
        let found = violations(file, &broken);
        assert!(
            rules_of(&found).contains(rule),
            "{file}: defect `{from}` -> `{to}` was not reported by rule {rule}: {found:?}"
        );
    }
}

/// Asserts that the `gate-check` rule reports each of the five mode units whose `boot-check`
/// is not `+/usr/bin/kbf boot-check --mode=<the unit's mode>`: one with no mode, one that is
/// the installer's `boot-check` of the earlier packages, one that names the mode of another
/// unit and one that does not run as root.
#[test]
fn a_boot_check_other_than_kbfs_for_the_units_mode_fails_the_gate_check_of_every_mode_unit() {
    let mut checked = 0;
    for name in FILES.iter().filter(|n| mode_of(n).is_some()) {
        let mode = mode_of(name).expect("a mode unit");
        let text = read(name);
        let right = format!("+{KBF} boot-check --mode={mode}\n");
        assert!(text.contains(&right), "{name}: no `{right}`");
        let earlier = format!("+{INSTALLER} boot-check --kbf={KBF} --mode={mode}\n");
        let other = if mode == "daemon" {
            "storage"
        } else {
            "daemon"
        };
        for wrong in [
            format!("+{KBF} boot-check\n"),
            earlier,
            format!("+{KBF} boot-check --mode={other}\n"),
            format!("{KBF} boot-check --mode={mode}\n"),
        ] {
            let changed = text.replacen(&right, &wrong, 1);
            let found = violations(name, &changed);
            assert!(
                rules_of(&found).contains(&"gate-check"),
                "{name}: `{wrong}` was not reported by rule gate-check: {found:?}"
            );
        }
        checked += 1;
    }
    assert_eq!(checked, 5, "a mode unit was not checked");
}

/// Catches a daemon stop timeout the rule lets through that cuts a drain short: any
/// number of seconds (the old 660, 90, and one far above any drain), `0`, a word that is
/// not `infinity`, and none at all are refused; `infinity` is accepted. The daemon drains
/// until its leases end, so a finite timeout SIGKILLs them.
#[test]
fn a_daemon_stop_timeout_must_be_infinity() {
    let name = "kbf-daemon.service";
    let text = read(name);
    let shipped = "\nTimeoutStopSec=infinity\n";
    assert!(text.contains(shipped));
    let stop = |broken: &str| {
        violations(name, broken)
            .into_iter()
            .filter(|f| f.starts_with("stop:"))
            .count()
    };
    assert_eq!(stop(&text), 0, "the shipped unit was refused");
    for bad in ["660", "90", "86400", "0", "11min"] {
        let t = text.replacen(shipped, &format!("\nTimeoutStopSec={bad}\n"), 1);
        assert_eq!(stop(&t), 1, "{bad} was accepted");
    }
    let none = text.replacen(shipped, "\n", 1);
    assert_eq!(stop(&none), 1, "no TimeoutStopSec= was accepted");
}

/// Proves each unit that hosts the node daemon stops by signalling its main process alone
/// with SIGTERM: KillMode=mixed and KillSignal=SIGTERM, read from the shipped file.
#[test]
fn units_that_host_the_daemon_signal_only_the_main_process_on_stop() {
    for name in [
        "kbf-daemon.service",
        "kbf-storage-and-daemon.service",
        "kbf-all-in-one.service",
    ] {
        let text = read(name);
        let unit = UnitFile::parse(&text).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(
            unit.get_in("Service", "KillMode").map(trim),
            Some("mixed"),
            "{name}: KillMode="
        );
        assert_eq!(
            unit.get_in("Service", "KillSignal").map(trim),
            Some("SIGTERM"),
            "{name}: KillSignal="
        );
    }
}

/// Catches a rule with no defect (a check that cannot be shown to fail), and a defect
/// for a rule that does not exist.
#[test]
fn every_rule_has_a_defect() {
    for rule in RULES {
        assert!(
            DEFECTS.iter().any(|m| m.3 == rule),
            "rule {rule} has no defect"
        );
    }
    for m in DEFECTS {
        assert!(RULES.contains(&m.3), "defect names no rule: {}", m.3);
    }
}

/// Asserts that every unit `setup` enables on a Linux host, for each mode it takes, is a unit
/// the `kbf` deb ships. The deb's files are the plan `cargo xtask pkg-config` writes from the
/// real packaging directory (not a list here); the units `setup` enables are `mode_units`,
/// which `kbf-setup`'s own tests hold to the `systemctl enable` commands it runs.
///
/// Catches: `setup` enabling a unit no package carries (the installer's or the proxy's, which
/// the deb no longer ships), which would fail the last step of `kbf setup --apply` on a fresh
/// host that has only the `kbf` deb.
#[test]
fn setup_enables_only_units_the_kbf_deb_ships() {
    use clap::ValueEnum;
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging/linux");
    let at = |rel: &str| -> PathBuf { root.join(rel) };
    let plan = xtask::pkg_linux::kbf_plan(&xtask::pkg_linux::Inputs {
        kbf: &at("units/kbf-daemon.service"),
        units: &at("units"),
        trust: &at("testdata/trust"),
        sysusers: &at("sysusers.d/kbf-installer.conf"),
        tmpfiles: &at("tmpfiles.d/kbf-installer.conf"),
        needrestart: &at("needrestart/kbf.conf"),
        platform: xtask::package::Platform::LinuxX86_64,
        version: "0.0.0+ci",
    })
    .expect("the real packaging directory is a plan");
    let shipped: Vec<&str> = plan
        .iter()
        .filter_map(|e| {
            e.dst
                .strip_prefix(&format!("{}/", xtask::pkg_linux::UNIT_DIR))
        })
        .collect();
    assert!(shipped.contains(&"kbf-daemon.service"), "{shipped:?}");
    let modes = kbf_installer::cli::Mode::value_variants();
    assert_eq!(modes.len(), 5, "every mode is checked");
    for mode in modes {
        let units = kbf_installer::setup::plan::mode_units(*mode);
        assert!(!units.is_empty(), "{mode:?}");
        for unit in units {
            assert!(
                shipped.contains(unit),
                "setup enables {unit} for {mode:?}, which the kbf deb does not ship: {shipped:?}"
            );
        }
    }
}
