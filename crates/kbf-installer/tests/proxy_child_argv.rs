//! The command line the proxy starts a frontend child with, parsed by the `kbf` binary's own
//! parser (`kbf-cli`), not by a copy of its rules. A child that cannot parse its command
//! line never starts, and the swap would find that out only at the first update.

#![cfg(target_os = "linux")]

use std::ffi::OsString;
use std::net::SocketAddr;
use std::path::Path;

use kbf_installer::proxy::model::{Slot, Start};
use kbf_installer::proxy::supervisor::frontend_argv;
use kbf_proxy::config::UpstreamTable;
use kbf_proxy::route::Target;
use kbf_proxy::slot::Slot as GateSlot;

const SECRET: &str = "/run/kbf-proxy/proxy-auth";

/// What a child's `kbf.toml` supplies on a real host and a parse without a file needs.
const FROM_CONFIG: [&str; 2] = [
    "--storage=http://127.0.0.1:19000",
    "--state-store=sqlite:///x.db",
];

/// The command line `kbf` is run with, as the spawner builds it.
fn command_line(table: UpstreamTable, slot: Slot, start: Start) -> Vec<OsString> {
    let mut words = vec![OsString::from("kbf")];
    words.extend(frontend_argv(Path::new(SECRET), table)(slot, start));
    words
}

/// The four listeners and the admin flags a child parses from that command line.
struct Parsed {
    reapi: SocketAddr,
    worker: SocketAddr,
    api: Option<SocketAddr>,
    admin: SocketAddr,
    await_takeover: bool,
    handover: bool,
    secret: Option<std::path::PathBuf>,
}

fn parse(mut words: Vec<OsString>) -> Result<Parsed, clap::Error> {
    words.extend(FROM_CONFIG.map(OsString::from));
    match kbf_cli::parse_from(words)? {
        kbf_cli::Invocation::Frontend(flags) => {
            let admin = flags.admin.expect("--admin-listen was given");
            Ok(Parsed {
                reapi: flags.server.listen,
                worker: flags.server.worker_listen,
                api: flags.server.api_listen,
                admin: admin.listen,
                await_takeover: admin.await_takeover,
                handover: flags.handover.is_some(),
                secret: flags.server.proxy_auth_file.clone(),
            })
        }
        other => panic!("not a frontend: {other:?}"),
    }
}

fn gate(slot: Slot) -> GateSlot {
    match slot {
        Slot::A => GateSlot::A,
        Slot::B => GateSlot::B,
    }
}

/// Catches a spawner whose command line the `kbf` parser refuses (the child would exit 2 at
/// once: the old `--instance=<slot>` needs a handover socket, and nobody gave one), that
/// puts a slot's listener off the proxy's table (the proxy would dial a port nothing
/// serves), or that starts a candidate without `--await-takeover` (it would take the
/// generation by itself and the swap would have two writers) or an active child with it (it
/// would wait for a takeover nothing sends).
#[test]
fn every_slot_and_start_parses_and_listens_where_the_proxys_table_dials() {
    let table = UpstreamTable { base_port: 18_000 };
    for slot in [Slot::A, Slot::B] {
        for start in [Start::Active, Start::Candidate] {
            let words = command_line(table, slot, start);
            let shown: Vec<_> = words.iter().map(|w| w.to_string_lossy()).collect();
            let got = parse(words.clone()).unwrap_or_else(|e| panic!("{shown:?}: {e}"));
            let at = |port: u16| SocketAddr::from(([127, 0, 0, 1], port));
            let want = |target| at(table.port(gate(slot), target));
            assert_eq!(got.reapi, want(Target::Reapi), "{shown:?}");
            assert_eq!(got.worker, want(Target::Worker), "{shown:?}");
            assert_eq!(got.api, Some(want(Target::Operator)), "{shown:?}");
            assert_eq!(got.admin, at(table.admin_port(gate(slot))), "{shown:?}");
            assert_eq!(got.await_takeover, start == Start::Candidate, "{shown:?}");
            assert!(
                !got.handover,
                "{shown:?}: no handover socket, so no --instance"
            );
            assert_eq!(got.secret.as_deref(), Some(Path::new(SECRET)), "{shown:?}");
        }
    }
}

/// Catches two slots, or two listeners of one slot, that share a port: the second child
/// would fail to bind and the swap would see a candidate that never gets ready. Checked on
/// what the parser read, at the lowest and the highest base the proxy accepts.
#[test]
fn no_two_listeners_of_the_two_slots_share_a_port() {
    for base_port in [1024, 18_000, 65_522] {
        let table = UpstreamTable { base_port };
        let mut ports = Vec::new();
        for slot in [Slot::A, Slot::B] {
            let got = parse(command_line(table, slot, Start::Candidate)).expect("parses");
            ports.extend([
                got.reapi.port(),
                got.worker.port(),
                got.api.expect("api").port(),
                got.admin.port(),
            ]);
        }
        let mut distinct = ports.clone();
        distinct.sort_unstable();
        distinct.dedup();
        assert_eq!(distinct.len(), 8, "base {base_port}: {ports:?}");
    }
}

/// The control for the two tests above: the parser really does refuse the `--instance` flag
/// without a handover socket, and `--await-takeover` without an admin port, so the tests
/// above go red on a spawner that brings either back.
#[test]
fn the_parser_refuses_what_the_old_command_line_gave() {
    let table = UpstreamTable { base_port: 18_000 };
    let mut with_instance = command_line(table, Slot::A, Start::Active);
    with_instance.push(OsString::from("--instance=a"));
    let e = parse(with_instance)
        .err()
        .expect("--instance alone is refused");
    assert!(e.to_string().contains("handover-socket"), "{e}");

    let mut no_admin = command_line(table, Slot::A, Start::Candidate);
    no_admin.retain(|w| !w.to_string_lossy().starts_with("--admin-listen"));
    let e = parse(no_admin)
        .err()
        .expect("a candidate needs the admin port");
    assert!(e.to_string().contains("admin-listen"), "{e}");
}
