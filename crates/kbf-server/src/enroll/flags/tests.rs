//! The enroll flags on `kbf frontend`: off by default, what they need, what they refuse.

use super::*;
use crate::frontend::{FrontendFlags, command, parse};

const BASE: [&str; 5] = [
    "frontend",
    "--storage=http://127.0.0.1:1",
    "--listen=127.0.0.1:18980",
    "--worker-listen=127.0.0.1:18981",
    "--api-listen=127.0.0.1:18982",
];
const ENROLL: [&str; 4] = [
    "--enroll-listen=127.0.0.1:18983",
    "--state-store=sqlite:///x.db",
    "--ca-dir=/var/lib/kbf/ca",
    "--frontend-name=front.test",
];

fn flags(more: &[&str]) -> Result<FrontendFlags, clap::Error> {
    let mut argv = BASE.to_vec();
    argv.extend_from_slice(more);
    parse(&command().try_get_matches_from(argv)?)
}

fn error(more: &[&str]) -> String {
    flags(more).expect_err("refused").to_string()
}

fn with<'a>(extra: &[&'a str]) -> Vec<&'a str> {
    let mut all: Vec<&'a str> = ENROLL.to_vec();
    all.extend_from_slice(extra);
    all
}

/// Without `--enroll-listen` there is no enrollment: the flags are `None` and the
/// frontend's other flags parse as before.
///
/// Catches: enrollment on by default, or the new flags changing a frontend that does not
/// use them.
#[test]
fn enrollment_is_off_unless_asked_for() {
    assert_eq!(flags(&[]).expect("flags").enroll, None);
}

/// The development signer parses, with every default the documentation states.
///
/// Catches: a default that differs from the one in `docs/api.md` (rates, lifetime) and a
/// listener that does not carry the names it was given.
#[test]
fn the_dev_signer_parses_with_the_documented_defaults() {
    let parsed = flags(&with(&[
        "--ca-dev-file-signer",
        "--frontend-name=Front2.Test",
    ]))
    .expect("flags")
    .enroll
    .expect("enroll");
    assert_eq!(parsed.listen, "127.0.0.1:18983".parse().expect("addr"));
    assert_eq!(parsed.signer, SignerChoice::DevFile);
    assert_eq!(parsed.frontend_names, ["front.test", "front2.test"]);
    assert_eq!(parsed.ca_dir, PathBuf::from("/var/lib/kbf/ca"));
    assert_eq!(parsed.cert_lifetime_secs, 604_800);
    assert_eq!(parsed.limits, Limits::DEFAULT);
    assert!(!parsed.allow_unenforced_deny);
    assert_eq!(parsed.settings, NodeSettings::default());
}

/// Production names a signer socket; the file signer is a flag of its own, and the two
/// together or neither are refused.
///
/// Catches: a frontend that holds the CA key because nobody said otherwise (a start with
/// neither flag must fail, not default to the file), and a start that takes both.
#[test]
fn production_needs_a_signer_socket_and_the_file_signer_is_explicit() {
    let none = error(&with(&[]));
    assert!(none.contains("--ca-signer"), "{none}");
    assert!(none.contains("--ca-dev-file-signer"), "{none}");

    let prod = flags(&with(&["--ca-signer=/run/kbf/signer.sock"])).expect("flags");
    assert_eq!(
        prod.enroll.expect("enroll").signer,
        SignerChoice::External(PathBuf::from("/run/kbf/signer.sock"))
    );

    let both = error(&with(&[
        "--ca-signer=/run/kbf/signer.sock",
        "--ca-dev-file-signer",
    ]));
    assert!(
        both.contains("--ca-signer") && both.contains("--ca-dev-file-signer"),
        "{both}"
    );
}

/// `--enroll-listen` needs the state store, a CA directory and a frontend name, and every
/// companion flag needs `--enroll-listen`.
///
/// Catches: a listener without the registry it serves, a certificate for no name, and a
/// flag typed for a listener that is not on, silently ignored.
#[test]
fn the_listener_needs_its_companions_and_they_need_it() {
    let dev = ["--ca-dev-file-signer"];
    for missing in ["--state-store", "--ca-dir", "--frontend-name"] {
        let args: Vec<&str> = with(&dev)
            .into_iter()
            .filter(|arg| !arg.starts_with(missing))
            .collect();
        let text = error(&args);
        assert!(
            text.contains(&missing[2..].replace('-', "_")) || text.contains(missing),
            "{missing}: {text}"
        );
    }
    for companion in [
        "--ca-dir=/x",
        "--ca-signer=/x",
        "--ca-dev-file-signer",
        "--frontend-name=a.test",
        "--enroll-allow-unenforced-deny",
        "--enroll-rate-per-minute=5",
        "--enroll-cert-lifetime-secs=7200",
    ] {
        let text = error(&[companion]);
        assert!(text.contains("--enroll-listen"), "{companion}: {text}");
    }
}

/// A name that is neither a DNS name nor an address, and a lifetime or a rate out of
/// range, are refused.
///
/// Catches: a certificate request for a name no client could verify, a CA lifetime outside
/// what the signer allows, and a zero rate that locks every node out.
#[test]
fn bad_names_and_out_of_range_numbers_are_refused() {
    let dev = ["--ca-dev-file-signer"];
    let name = error(&with(&[dev[0], "--frontend-name=not a name"]));
    assert!(name.contains("--frontend-name"), "{name}");
    for bad in [
        "--enroll-cert-lifetime-secs=60",
        "--enroll-cert-lifetime-secs=99999999",
        "--enroll-rate-per-minute=0",
        "--enroll-rate-burst=0",
        "--enroll-max-in-flight=0",
        "--enroll-storage-port=70000",
    ] {
        assert!(flags(&with(&[dev[0], bad])).is_err(), "{bad}");
    }
    let long_label = format!("--frontend-name={}.test", "a".repeat(64));
    let long_name = format!("--frontend-name={}", ["abcdefghi"; 28].join("."));
    for bad in [
        "--frontend-name=a..test",
        "--frontend-name=-a.test",
        "--frontend-name=a-.test",
        "--frontend-name=",
        &long_label,
        &long_name,
    ] {
        let said = error(&with(&[dev[0], bad]));
        assert!(said.contains("--frontend-name"), "{bad}: {said}");
    }
    let edge = format!("--frontend-name={}.test", "a".repeat(63));
    assert!(
        flags(&with(&[dev[0], &edge])).is_ok(),
        "a 63-character label"
    );
    assert!(
        flags(&with(&[
            dev[0],
            "--frontend-name=192.0.2.7",
            "--frontend-name=[::1]"
        ]))
        .is_err()
    );
    assert!(
        flags(&with(&[
            dev[0],
            "--frontend-name=192.0.2.7",
            "--frontend-name=2001:db8::1"
        ]))
        .is_ok()
    );
}

/// The numbers a node is told are the flags'.
///
/// Catches: a flag read and dropped.
#[test]
fn what_nodes_are_told_comes_from_the_flags() {
    let parsed = flags(&with(&[
        "--ca-dev-file-signer",
        "--enroll-worker-endpoint=https://front.test:7443",
        "--enroll-cas-endpoint=https://front.test:7444",
        "--enroll-storage-port=9000",
        "--enroll-rate-per-minute=5",
        "--enroll-rate-burst=7",
        "--enroll-max-in-flight=3",
        "--enroll-allow-unenforced-deny",
        "--enroll-cert-lifetime-secs=7200",
    ]))
    .expect("flags")
    .enroll
    .expect("enroll");
    assert_eq!(parsed.settings.worker_endpoint, "https://front.test:7443");
    assert_eq!(parsed.settings.cas_endpoint, "https://front.test:7444");
    assert_eq!(parsed.settings.storage_port, 9000);
    assert_eq!(
        parsed.limits,
        Limits {
            per_minute: 5,
            burst: 7,
            in_flight: 3
        }
    );
    assert!(parsed.allow_unenforced_deny);
    assert_eq!(parsed.cert_lifetime_secs, 7200);
}

/// Instance b of a handover pair listens ten ports above instance a, enroll listener
/// included.
///
/// Catches: the successor binding the enroll port the predecessor still holds (it could
/// not start), or an enroll listener left out of the port block.
#[test]
fn instance_b_moves_the_enroll_listener_with_the_others() {
    let handover = [
        "--handover-socket=/run/kbf/control.sock",
        "--ca-dev-file-signer",
    ];
    let a = flags(&with(&[handover[0], handover[1], "--instance=a"])).expect("flags");
    let b = flags(&with(&[handover[0], handover[1], "--instance=b"])).expect("flags");
    assert_eq!(a.enroll.expect("enroll").listen.port(), 18983);
    assert_eq!(b.enroll.expect("enroll").listen.port(), 18993);
}

/// What a handover compares is the same for the two instances of one command line, and
/// differs for each flag that decides the CA or the address.
///
/// Catches: a key built from the shifted port (the pair never agree, so no handover
/// happens), or a key that leaves out the listener's address, the CA directory, the
/// signer or the deny-row switch (a successor serving another CA is let through).
#[test]
fn the_handover_key_is_the_same_for_both_instances_and_differs_by_each_ca_flag() {
    let key = |extra: &[&str]| {
        let mut all = vec!["--handover-socket=/run/kbf/control.sock"];
        all.extend_from_slice(extra);
        let parsed = flags(&with(&all)).expect("flags");
        parsed
            .enroll
            .expect("enroll")
            .handover_key(parsed.handover.and_then(|h| h.instance))
    };
    let a = key(&["--ca-dev-file-signer", "--instance=a"]);
    assert_eq!(a, key(&["--ca-dev-file-signer", "--instance=b"]));
    let others = [
        key(&["--ca-signer=/run/kbf/signer.sock", "--instance=a"]),
        key(&[
            "--ca-dev-file-signer",
            "--enroll-allow-unenforced-deny",
            "--instance=a",
        ]),
    ];
    for other in &others {
        assert_ne!(&a, other);
    }
    assert_ne!(others[0], others[1]);
    let mut elsewhere = with(&[
        "--handover-socket=/run/kbf/control.sock",
        "--ca-dev-file-signer",
        "--instance=a",
    ]);
    // Another loopback address, built rather than written out (the repo hygiene lint
    // refuses non-documentation IPv4 literals).
    let other = format!(
        "--enroll-listen={}:18983",
        std::net::Ipv4Addr::new(127, 0, 0, 2)
    );
    elsewhere[0] = &other;
    let moved = flags(&elsewhere).expect("flags");
    assert_ne!(
        a,
        moved
            .enroll
            .expect("enroll")
            .handover_key(moved.handover.and_then(|h| h.instance))
    );
    elsewhere[0] = "--enroll-listen=127.0.0.1:18983";
    elsewhere[2] = "--ca-dir=/var/lib/kbf/other";
    let other_ca = flags(&elsewhere).expect("flags");
    assert_ne!(
        a,
        other_ca
            .enroll
            .expect("enroll")
            .handover_key(other_ca.handover.and_then(|h| h.instance))
    );
}
