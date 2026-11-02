//! `install` and `install --uninstall` on the fake host.

mod common;

use common::{
    FakeNet, Kit, SVC, binary_bytes, changes, config, kit, kit_with, random_hex, ready_host,
    site_addresses,
};
use host_step::fake::FakeHost;
use host_step::{Host, Mode, SYSTEMCTL};
use mdm_core::commands::scep_ca_init_args;
use mdm_core::render::{render, render_tls};
use mdm_host::install::{Install, run, run_uninstall};
use mdm_host::{Env, MdmError};

const STATE: &str = "/var/lib/js-mdm";
const ETC: &str = "/etc/js-mdm";
const BIN: &str = "/opt/js-mdm/bin";
const UNITS: &str = "/etc/systemd/system";
const CA: &str = "/var/lib/js-mdm/scep/depot/ca.pem";
const CERT: &str = "/var/lib/js-mdm/lego/certificates/mdm.example.com.crt";

fn install(host: &FakeHost, kit: &Kit, mode: Mode) -> (Result<(), MdmError>, String) {
    let net = FakeNet::new(Default::default());
    let a = Install {
        config: config(),
        bin_dir: "/srv/fetch".to_owned(),
        max_archive: 1 << 30,
    };
    let mut out = Vec::new();
    let result = common::with_env(host, &net, |env| run(env, &a, &kit.pins, mode, &mut out));
    (result, String::from_utf8(out).unwrap())
}

fn uninstall(host: &FakeHost, mode: Mode) -> (Result<(), MdmError>, String) {
    let net = FakeNet::new(Default::default());
    let mut out = Vec::new();
    let result = common::with_env(host, &net, |env| {
        run_uninstall(env, &config(), mode, &mut out)
    });
    (result, String::from_utf8(out).unwrap())
}

/// The commands that make a unit enabled and active, for a rerun.
fn running(host: &FakeHost, units: &[&str]) {
    for unit in units {
        host.reply(&[SYSTEMCTL, "is-enabled", unit], 0, "enabled\n");
        host.reply(&[SYSTEMCTL, "is-active", unit], 0, "active\n");
    }
}

const CORE: [&str; 3] = [
    "js-mdm-scep.service",
    "js-mdm-nanohub.service",
    "js-mdm-depserver.service",
];

fn ca_argv() -> Vec<String> {
    let mut argv: Vec<String> = [
        "/usr/sbin/runuser",
        "-u",
        "js-mdm",
        "--",
        "/opt/js-mdm/bin/scepserver",
    ]
    .map(str::to_owned)
    .to_vec();
    argv.extend(scep_ca_init_args(&config()));
    argv
}

/// Tells the fake that the CA command leaves a CA, as the real one does.
fn ca_is_made_by_init(host: &FakeHost) {
    let argv = ca_argv();
    let words: Vec<&str> = argv.iter().map(String::as_str).collect();
    host.effect(&words, CA, 0o600, (SVC, SVC), b"CA");
}

/// Catches a dry run that changes the host or lists a secret: the plan names paths, sizes and
/// modes only; the secrets it would make appear by name and size; only read-only commands run.
#[test]
fn a_dry_run_lists_names_sizes_and_modes_and_changes_nothing() {
    let kit = kit();
    let host = ready_host(&kit);
    let before = host.paths();
    let (result, shown) = install(&host, &kit, Mode::DryRun);
    result.unwrap();
    assert_eq!(host.paths(), before);
    assert_eq!(
        changes(&host),
        ["/usr/bin/ip -o -4 addr show", "/usr/bin/ss -Hltn"]
    );
    let size = format!("NANOHUB_API_KEY={}\n", random_hex()).len();
    for line in [
        "archives: every one matches its pin",
        "secret NANOHUB_API_KEY: makes /etc/js-mdm/nanohub.env (0600 root; never shown)",
        "proxy: not started; it waits for the TLS certificate /var/lib/js-mdm/lego/certificates/mdm.example.com.crt",
        "DRY-RUN: mkdir /var/lib/js-mdm mode 0700 owner js-mdm:js-mdm",
        "DRY-RUN: mkdir /var/lib/js-mdm/scep/depot mode 0700 owner js-mdm:js-mdm",
        "DRY-RUN: mkdir /etc/js-mdm mode 0750 owner root:js-mdm",
        "DRY-RUN: mkdir /opt/js-mdm/bin mode 0755 owner root:root",
        "DRY-RUN: write /opt/js-mdm/bin/scepserver mode 0755 owner root:root (",
        &format!("DRY-RUN: write /etc/js-mdm/nanohub.env mode 0600 owner root:root ({size} bytes)"),
        "DRY-RUN: write /etc/systemd/system/js-mdm-scep.service mode 0644 owner root:root (",
        "DRY-RUN: write /etc/js-mdm/Caddyfile mode 0640 owner root:js-mdm (",
        "DRY-RUN: systemctl daemon-reload",
        "DRY-RUN: systemctl enable -- js-mdm-scep.service",
        "DRY-RUN: systemctl restart -- js-mdm-depserver.service",
        "DRY-RUN: run /usr/sbin/runuser -u js-mdm -- /opt/js-mdm/bin/scepserver ca -init",
        "dry run: nothing changed; re-run with --apply to carry it out",
    ] {
        assert!(shown.contains(line), "missing {line:?} in\n{shown}");
    }
    assert!(!shown.contains(&random_hex()));
    assert!(!shown.contains("CHALLENGE_PASSWORD="));
}

/// Catches an install that can be run by anyone: without root it is refused before any
/// command runs, even as a dry run (it reads root-only files), with exit 2.
#[test]
fn install_without_root_is_refused_even_as_a_dry_run() {
    let kit = kit();
    let other = FakeHost::new(1000);
    for mode in [Mode::DryRun, Mode::Apply] {
        let (result, shown) = install(&other, &kit, mode);
        let err = result.unwrap_err();
        assert!(
            matches!(err, MdmError::NeedsRoot { euid: 1000, .. }),
            "{err}"
        );
        assert_eq!(err.exit_code(), 2);
        assert_eq!(shown, "");
    }
    assert!(other.calls().is_empty());
    let (result, _) = uninstall(&other, Mode::Apply);
    assert!(matches!(result.unwrap_err(), MdmError::NeedsRoot { .. }));
}

/// Catches wrong modes and owners, secrets that differ from what was made, units that are not
/// what mdm-core renders, and a secret that reaches a command line or the output.
#[test]
fn apply_writes_exact_modes_the_core_render_and_keeps_secrets_off_argv() {
    let kit = kit();
    let host = ready_host(&kit);
    ca_is_made_by_init(&host);
    let (result, shown) = install(&host, &kit, Mode::Apply);
    result.unwrap();

    let dir = |path: &str, mode: u32, owner: (u32, u32)| {
        let n = host
            .node(path)
            .unwrap_or_else(|| panic!("{path} is missing"));
        assert_eq!((n.mode, n.uid, n.gid), (mode, owner.0, owner.1), "{path}");
    };
    dir(STATE, 0o700, (SVC, SVC));
    for d in [
        "nanohub",
        "scep",
        "scep/depot",
        "dep",
        "lego",
        "caddy",
        "www",
    ] {
        dir(&format!("{STATE}/{d}"), 0o700, (SVC, SVC));
    }
    dir(ETC, 0o750, (0, SVC));
    dir(BIN, 0o755, (0, 0));

    for (name, member_bytes) in [
        ("scepserver", binary_bytes("scepserver")),
        ("nanohub", binary_bytes("nanohub")),
        ("depserver", binary_bytes("depserver")),
        ("depsyncer", binary_bytes("depsyncer")),
        ("caddy", binary_bytes("caddy")),
        ("lego", binary_bytes("lego")),
        ("mdmctl", binary_bytes("mdmctl")),
    ] {
        let n = host.node(&format!("{BIN}/{name}")).unwrap();
        assert_eq!(
            (n.mode, n.uid, n.gid, n.bytes),
            (0o755, 0, 0, member_bytes),
            "{name}"
        );
    }
    for (file, key) in [
        ("nanohub.env", "NANOHUB_API_KEY"),
        ("nanodep.env", "NANODEP_API"),
        ("scep.env", "SCEP_CHALLENGE_PASSWORD"),
    ] {
        let n = host.node(&format!("{ETC}/{file}")).unwrap();
        assert_eq!((n.mode, n.uid, n.gid), (0o600, 0, 0), "{file}");
        assert_eq!(n.bytes, format!("{key}={}\n", random_hex()).into_bytes());
    }

    let rendered = render(&config()).unwrap();
    for unit in &rendered.units {
        let n = host.node(&format!("{UNITS}/{}", unit.name)).unwrap();
        assert_eq!((n.mode, n.uid, n.gid), (0o644, 0, 0), "{}", unit.name);
        assert_eq!(
            n.bytes,
            unit.text.as_bytes(),
            "{} differs from the render",
            unit.name
        );
    }
    let caddy = host.node(&format!("{ETC}/Caddyfile")).unwrap();
    assert_eq!((caddy.mode, caddy.uid, caddy.gid), (0o640, 0, SVC));
    assert_eq!(caddy.bytes, rendered.caddyfile.as_bytes());

    // The commands: the CA as the service user, the units, in order; no secret on any line.
    let ran = changes(&host);
    assert_eq!(ran[2], ca_argv().join(" "));
    assert_eq!(ran[3], "/usr/bin/systemctl daemon-reload");
    assert_eq!(
        &ran[4..],
        [
            "/usr/bin/systemctl enable -- js-mdm-scep.service",
            "/usr/bin/systemctl restart -- js-mdm-scep.service",
            "/usr/bin/systemctl enable -- js-mdm-nanohub.service",
            "/usr/bin/systemctl restart -- js-mdm-nanohub.service",
            "/usr/bin/systemctl enable -- js-mdm-depserver.service",
            "/usr/bin/systemctl restart -- js-mdm-depserver.service",
        ]
    );
    for call in host.calls() {
        let line = call.join(" ");
        assert!(!line.contains(&random_hex()), "{line}");
    }
    assert!(!shown.contains(&random_hex()));
    assert!(shown.contains("+ write /etc/js-mdm/scep.env mode 0600 owner root:root ("));
}

/// Catches a rerun that plans again what the first run did: with the services running and the
/// CA made, a second run finds nothing to do, runs no change, and keeps the secrets as they
/// were. The held ports are the kit's own active units, so they are not a refusal.
#[test]
fn a_rerun_finds_nothing_to_do() {
    let kit = kit();
    let host = ready_host(&kit);
    ca_is_made_by_init(&host);
    install(&host, &kit, Mode::Apply).0.unwrap();
    running(&host, &CORE);
    host.reply(
        &["/usr/bin/ss", "-Hltn"],
        0,
        "LISTEN 0 4096 127.0.0.1:9004 0.0.0.0:*\nLISTEN 0 4096 127.0.0.1:9005 0.0.0.0:*\nLISTEN 0 4096 127.0.0.1:9001 0.0.0.0:*\n",
    );
    let files_before: Vec<_> = host.paths().iter().map(|p| host.node(p)).collect();
    host.clear_calls();
    let (result, shown) = install(&host, &kit, Mode::Apply);
    result.unwrap();
    assert!(
        shown.contains("port 9004: held by js-mdm-nanohub.service (a rerun)"),
        "{shown}"
    );
    assert!(shown.contains("secret NANOHUB_API_KEY: kept (/etc/js-mdm/nanohub.env)"));
    assert!(shown.contains("SCEP CA: kept (/var/lib/js-mdm/scep/depot/ca.pem)"));
    assert!(shown.ends_with("nothing to do\n"), "{shown}");
    assert_eq!(
        changes(&host),
        ["/usr/bin/ip -o -4 addr show", "/usr/bin/ss -Hltn"]
    );
    let files_after: Vec<_> = host.paths().iter().map(|p| host.node(p)).collect();
    assert_eq!(files_before, files_after);
}

/// Catches a CA that is replaced: when `ca.pem` is there the init command is not planned and
/// the file is untouched.
#[test]
fn an_existing_scep_ca_is_kept() {
    let kit = kit();
    let host = ready_host(&kit);
    host.put_file(CA, 0o600, (SVC, SVC), b"EXISTING CA");
    let (result, shown) = install(&host, &kit, Mode::Apply);
    result.unwrap();
    assert!(shown.contains("SCEP CA: kept (/var/lib/js-mdm/scep/depot/ca.pem)"));
    assert!(!host.calls().iter().any(|c| c[0] == "/usr/sbin/runuser"));
    assert_eq!(host.node(CA).unwrap().bytes, b"EXISTING CA");
}

/// Catches a binary upgrade that loses the old binary or restarts the wrong services: the old
/// one is kept as `.previous`, only the service that runs it is restarted.
#[test]
fn an_upgraded_binary_keeps_the_old_one_and_restarts_its_service() {
    let kit = kit();
    let host = ready_host(&kit);
    ca_is_made_by_init(&host);
    install(&host, &kit, Mode::Apply).0.unwrap();
    running(&host, &CORE);
    let v2 = kit_with(|name, bytes| {
        if name == "nanohub" {
            b"nanohub v2".to_vec()
        } else {
            bytes
        }
    });
    for (file, bytes) in &v2.archives {
        host.put_file(&format!("/srv/fetch/{file}"), 0o644, (1000, 1000), bytes);
    }
    host.clear_calls();
    let (result, shown) = install(&host, &v2, Mode::Apply);
    result.unwrap();
    assert!(
        shown.contains("write /opt/js-mdm/bin/nanohub.previous mode 0755 owner root:root"),
        "{shown}"
    );
    assert_eq!(
        host.node("/opt/js-mdm/bin/nanohub.previous").unwrap().bytes,
        binary_bytes("nanohub")
    );
    assert_eq!(
        host.node("/opt/js-mdm/bin/nanohub").unwrap().bytes,
        b"nanohub v2"
    );
    assert_eq!(
        changes(&host),
        [
            "/usr/bin/ip -o -4 addr show",
            "/usr/bin/ss -Hltn",
            "/usr/bin/systemctl restart -- js-mdm-nanohub.service"
        ]
    );
}

/// Catches a proxy started without its certificate, and one never started once the certificate
/// is there; a changed Caddyfile restarts it.
#[test]
fn the_proxy_starts_with_the_certificate_and_restarts_on_a_new_caddyfile() {
    let kit = kit();
    let host = ready_host(&kit);
    host.put_file(CERT, 0o644, (SVC, SVC), b"CERT");
    ca_is_made_by_init(&host);
    let (result, shown) = install(&host, &kit, Mode::Apply);
    result.unwrap();
    assert!(!shown.contains("proxy: not started"));
    assert!(
        changes(&host).contains(&"/usr/bin/systemctl restart -- js-mdm-proxy.service".to_owned())
    );
    running(&host, &CORE);
    running(&host, &["js-mdm-proxy.service"]);
    host.put_file(
        &format!("{ETC}/Caddyfile"),
        0o640,
        (0, SVC),
        b"edited by hand",
    );
    host.clear_calls();
    install(&host, &kit, Mode::Apply).0.unwrap();
    assert_eq!(
        changes(&host).last().unwrap(),
        "/usr/bin/systemctl restart -- js-mdm-proxy.service"
    );
    // Untouched, the running proxy is left alone.
    host.clear_calls();
    install(&host, &kit, Mode::Apply).0.unwrap();
    assert_eq!(
        changes(&host),
        ["/usr/bin/ip -o -4 addr show", "/usr/bin/ss -Hltn"]
    );
}

/// Catches the refusals that must come before any change: a missing or altered archive, a
/// bind address no interface carries, a port held by another process, a failing `ip`, a link
/// where a directory should be. Each has the exit code that tells fix-the-input from retry.
#[test]
fn the_host_checks_refuse_before_anything_changes() {
    let kit = kit();
    let refused = |host: &FakeHost| {
        let before = host.paths();
        let (result, _) = install(host, &kit, Mode::Apply);
        let err = result.unwrap_err();
        assert_eq!(host.paths(), before, "changed the host");
        err
    };

    let host = ready_host(&kit);
    host.remove("/srv/fetch/caddy-v1.0.0.tar.gz").unwrap();
    let err = refused(&host);
    assert!(matches!(err, MdmError::NoArchive(_)), "{err}");
    assert_eq!(err.exit_code(), 2);

    let host = ready_host(&kit);
    host.put_file("/srv/fetch/lego-v1.0.0.tar.gz", 0o644, (0, 0), b"tampered");
    assert!(matches!(refused(&host), MdmError::ArchiveDiffers { .. }));

    let host = ready_host(&kit);
    host.reply(
        &["/usr/bin/ip", "-o", "-4", "addr", "show"],
        0,
        &format!(
            "2: eth0    inet {}/24 scope global eth0\n1: lo inet6 ::1/128\n",
            std::net::Ipv4Addr::new(10, 0, 0, 99)
        ),
    );
    let err = refused(&host);
    assert_eq!(
        err.to_string(),
        site_addresses("no interface carries MDM_BIND=@TEST_BIND@ (ip -4 addr)")
    );

    let host = ready_host(&kit);
    host.reply(
        &["/usr/bin/ss", "-Hltn"],
        0,
        "LISTEN 0 4096 0.0.0.0:9005 0.0.0.0:*\nLISTEN 0 1 [::]:22 [::]:*\n",
    );
    host.reply(
        &[SYSTEMCTL, "is-active", "js-mdm-scep.service"],
        3,
        "inactive\n",
    );
    let err = refused(&host);
    assert!(matches!(err, MdmError::PortHeld(9005)), "{err}");
    assert_eq!(err.exit_code(), 2);

    let host = ready_host(&kit);
    host.reply(&["/usr/bin/ip", "-o", "-4", "addr", "show"], 1, "");
    let err = refused(&host);
    assert!(matches!(err, MdmError::Command { code: 1, .. }), "{err}");
    assert_eq!(err.exit_code(), 1);

    for link in [STATE, ETC, BIN] {
        let host = ready_host(&kit);
        host.put_link(link);
        let err = refused(&host);
        assert_eq!(
            err.to_string(),
            format!("{link} is a symbolic link: name the real directory")
        );
    }
}

/// Catches a secret file that is trusted whatever its mode: an existing one is kept only when
/// it is 0600 and root's; one with the key but no value is made again.
#[test]
fn existing_secrets_are_kept_only_when_safe() {
    let kit = kit();
    let host = ready_host(&kit);
    host.put_file(
        &format!("{ETC}/nanohub.env"),
        0o644,
        (0, 0),
        b"NANOHUB_API_KEY=old\n",
    );
    let (result, _) = install(&host, &kit, Mode::Apply);
    let err = result.unwrap_err();
    assert!(
        matches!(
            err,
            MdmError::SecretMode {
                mode: 0o644,
                uid: 0,
                ..
            }
        ),
        "{err}"
    );
    assert!(err.to_string().contains("not 0600 and root: chmod 600"));
    assert_eq!(
        host.node(&format!("{ETC}/nanohub.env")).unwrap().bytes,
        b"NANOHUB_API_KEY=old\n"
    );

    let host = ready_host(&kit);
    host.put_file(
        &format!("{ETC}/nanohub.env"),
        0o600,
        (0, 0),
        b"NANOHUB_API_KEY=old\n",
    );
    host.put_file(
        &format!("{ETC}/scep.env"),
        0o600,
        (0, 0),
        b"SCEP_CHALLENGE_PASSWORD=\n",
    );
    let (result, shown) = install(&host, &kit, Mode::Apply);
    result.unwrap();
    assert_eq!(
        host.node(&format!("{ETC}/nanohub.env")).unwrap().bytes,
        b"NANOHUB_API_KEY=old\n"
    );
    assert!(shown.contains("secret NANOHUB_API_KEY: kept"));
    let scep = host.node(&format!("{ETC}/scep.env")).unwrap();
    assert_eq!(
        scep.bytes,
        format!("SCEP_CHALLENGE_PASSWORD={}\n", random_hex()).into_bytes()
    );
}

/// Catches a random source failure that goes unnoticed (a secret made of zeros): the run fails
/// (exit 1) and no secret file is written.
#[test]
fn a_failing_random_source_fails_the_run_and_writes_no_secret() {
    let kit = kit();
    let host = ready_host(&kit);
    let net = FakeNet::new(Default::default());
    let broken = |_: &mut [u8]| -> std::io::Result<()> { Err(std::io::Error::other("no entropy")) };
    let env = Env {
        host: &host,
        net: &net,
        random: &broken,
        now: 0,
    };
    let a = Install {
        config: config(),
        bin_dir: "/srv/fetch".to_owned(),
        max_archive: 1 << 30,
    };
    let err = run(&env, &a, &kit.pins, Mode::Apply, &mut Vec::new()).unwrap_err();
    assert!(matches!(err, MdmError::Io(_)), "{err}");
    assert_eq!(err.exit_code(), 1);
    assert!(host.node(&format!("{ETC}/nanohub.env")).is_none());
}

/// Catches an uninstall that removes state: the units are stopped, disabled and moved aside
/// (to a directory named by the time), and the state, the TLS certificate, the CA, the
/// secrets, the binaries and the user stay exactly as they were.
#[test]
fn uninstall_moves_units_aside_and_keeps_state_and_the_certificate() {
    let kit = kit();
    let host = ready_host(&kit);
    host.put_file(CERT, 0o644, (SVC, SVC), b"CERT");
    ca_is_made_by_init(&host);
    install(&host, &kit, Mode::Apply).0.unwrap();
    let tls = render_tls(&config(), &common::acme()).unwrap();
    for unit in &tls {
        host.put_file(
            &format!("{UNITS}/{}", unit.name),
            0o644,
            (0, 0),
            unit.text.as_bytes(),
        );
    }
    let keep: Vec<String> = host
        .paths()
        .into_iter()
        .filter(|p| !p.starts_with(UNITS))
        .collect();
    let kept_before: Vec<_> = keep.iter().map(|p| host.node(p)).collect();

    // Dry run first: it lists, and changes nothing.
    let before = host.paths();
    let (result, shown) = uninstall(&host, Mode::DryRun);
    result.unwrap();
    assert_eq!(host.paths(), before);
    assert!(shown.contains(
        "DRY-RUN: systemctl disable --now -- js-mdm-scep.service js-mdm-nanohub.service js-mdm-depserver.service js-mdm-depsyncer.service js-mdm-proxy.service js-mdm-cert.timer js-mdm-proxy-reload.path"
    ), "{shown}");
    assert!(shown.contains("kept: /var/lib/js-mdm (database, SCEP CA, DEP token, TLS key and certificate), /etc/js-mdm (secrets), /opt/js-mdm/bin, the user js-mdm"));

    host.clear_calls();
    let (result, _) = uninstall(&host, Mode::Apply);
    result.unwrap();
    let rendered = render(&config()).unwrap();
    let aside = format!("{STATE}/removed/1700000000");
    for unit in rendered.units.iter().chain(tls.iter()) {
        assert!(
            host.node(&format!("{UNITS}/{}", unit.name)).is_none(),
            "{} is still installed",
            unit.name
        );
        let moved = host.node(&format!("{aside}/{}", unit.name)).unwrap();
        assert_eq!(moved.bytes, unit.text.as_bytes());
        // The copies are the service user's, so every directory on the way is root's or its.
        assert_eq!((moved.uid, moved.gid), (SVC, SVC));
    }
    for (path, before) in keep.iter().zip(kept_before) {
        assert_eq!(host.node(path), before, "{path} changed");
    }
    assert!(host.node(CERT).is_some() && host.node(CA).is_some());
    let ran = changes(&host);
    assert!(ran.last().unwrap().ends_with("systemctl daemon-reload"));
    assert!(ran[0].starts_with("/usr/bin/systemctl disable --now -- js-mdm-scep.service"));
}

/// Catches an uninstall on a host with nothing installed that plans work or fails: it says so
/// and has nothing to do.
#[test]
fn uninstall_with_nothing_installed_does_nothing() {
    let kit = kit();
    let host = ready_host(&kit);
    let (result, shown) = uninstall(&host, Mode::Apply);
    result.unwrap();
    assert!(
        shown.starts_with("no unit of the kit is installed\nkept: "),
        "{shown}"
    );
    assert!(shown.ends_with("nothing to do\n"));
    assert!(changes(&host).is_empty());
}

/// Catches an archive read whole whatever its size: one above the cap is refused (exit 2)
/// before it is read, and nothing is planned.
#[test]
fn an_archive_above_the_size_cap_is_refused() {
    let kit = kit();
    let host = ready_host(&kit);
    let net = FakeNet::new(Default::default());
    let a = Install {
        config: config(),
        bin_dir: "/srv/fetch".to_owned(),
        max_archive: 10,
    };
    let err = common::with_env(&host, &net, |env| {
        run(env, &a, &kit.pins, Mode::Apply, &mut Vec::new())
    })
    .unwrap_err();
    assert!(
        matches!(err, MdmError::ArchiveTooBig { max: 10, .. }),
        "{err}"
    );
    assert!(
        err.to_string()
            .contains("is larger than the 10 bytes an archive may be")
    );
    assert_eq!(err.exit_code(), 2);
    assert!(host.node(&format!("{ETC}/nanohub.env")).is_none());
}

/// Catches a secret file that loses what else it holds: an empty value is made again only
/// when nothing else is in the file; otherwise the run is refused and the file is untouched.
#[test]
fn a_secret_file_with_other_lines_and_no_value_is_not_overwritten() {
    let kit = kit();
    let host = ready_host(&kit);
    let path = format!("{ETC}/nanohub.env");
    host.put_file(&path, 0o600, (0, 0), b"EXTRA_SETTING=1\nNANOHUB_API_KEY=\n");
    let (result, _) = install(&host, &kit, Mode::Apply);
    let err = result.unwrap_err();
    assert!(matches!(err, MdmError::SecretOther { .. }), "{err}");
    assert!(err.to_string().contains("holds other lines"));
    assert_eq!(err.exit_code(), 2);
    assert_eq!(
        host.node(&path).unwrap().bytes,
        b"EXTRA_SETTING=1\nNANOHUB_API_KEY=\n"
    );
}

/// Catches an archive that cannot be read passing as a missing one, or as fine: a read error
/// is a failure (exit 1) that names nothing it should not, and nothing is planned.
#[test]
fn an_archive_that_cannot_be_read_fails_the_run() {
    let kit = kit();
    let host = ready_host(&kit);
    host.deny_read("/srv/fetch/caddy-v1.0.0.tar.gz");
    let (result, _) = install(&host, &kit, Mode::Apply);
    let err = result.unwrap_err();
    assert!(matches!(err, MdmError::Io(_)), "{err}");
    assert_eq!(err.exit_code(), 1);
    assert!(host.node(&format!("{ETC}/nanohub.env")).is_none());
}
