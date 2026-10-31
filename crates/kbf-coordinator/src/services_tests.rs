//! Services as data: the file that defines them, and the decision across two services.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use kbf_types::HostRole;
use kbf_updater::wire::{MAX_SERVICE_NAME, MAX_SERVICES, Reply};

use crate::decide::decide_in;
use crate::desired::{Desired, KBF};
use crate::fleet::{Fleet, ORDER_AGE};
use crate::services::{RoleDef, ServiceDef, Services, ServicesError};
use crate::testutil::{beat, desired, id, updating, version};

const NOW: Duration = Duration::from_secs(1_000);

const TWO: &str = r#"
[[service]]
name = "server"
[service.roles.daemon]
max_updating = 1
min_up = 0

[[service]]
name = "agent"
after = ["server"]
[service.roles.daemon]
max_updating = 2
min_up = 0
"#;

fn update(target: &str) -> Reply {
    Reply::Update {
        version: version(target),
    }
}

fn wants(entries: &[(&str, &Desired)]) -> BTreeMap<String, Desired> {
    entries
        .iter()
        .map(|(name, d)| ((*name).to_owned(), (*d).clone()))
        .collect()
}

fn ask(s: &Services, w: &BTreeMap<String, Desired>, f: &Fleet, service: &str, host: &str) -> Reply {
    decide_in(s, w, service, f, &id(host), NOW)
}

fn refusal(text: &str) -> String {
    Services::parse(text).expect_err("refused").to_string()
}

/// Catches: kbf's rules being code and not data: its definition names all four roles, makes
/// only the frontend never the last one up, and has the frontend wait for the other three
/// roles and nothing else wait for anything.
#[test]
fn kbf_is_built_in_as_data() {
    let kbf = ServiceDef::kbf();
    assert_eq!(kbf.name, KBF);
    assert!(kbf.after.is_empty());
    assert_eq!(kbf.roles.len(), 4);
    for role in [HostRole::STORAGE, HostRole::DAEMON, HostRole::MAC_DAEMON] {
        assert_eq!(kbf.roles[role.name()], RoleDef::plain(), "{role:?}");
    }
    let frontend = &kbf.roles["frontend"];
    assert!(frontend.never_last);
    assert_eq!(frontend.after, ["storage", "daemon", "mac-daemon"]);
    assert_eq!(Services::default().get(KBF), Some(&kbf));
}

/// Catches: a services file that is read wrongly: the two services of the file are defined
/// with their limits, roles and `after`; kbf stays; CI may set only kbf unless the file
/// says otherwise; and a file that cannot be read is refused.
#[test]
fn a_services_file_defines_further_services() {
    let s = Services::parse(TWO).expect("parsed");
    let agent = s.get("agent").expect("defined");
    assert_eq!(agent.after, ["server"]);
    let daemon = &agent.roles["daemon"];
    assert_eq!((daemon.max_updating, daemon.min_up), (Some(2), Some(0)));
    assert!(!daemon.never_last);
    assert!(s.get(KBF).is_some() && s.get("server").is_some());
    assert_eq!(Services::parse("").expect("empty"), Services::default());

    let dir = kbf_testkit::dir::TestDir::under(std::env::temp_dir(), "services-file");
    let path = dir.path().join("services.toml");
    std::fs::write(&path, TWO).expect("written");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("mode");
    assert_eq!(Services::from_file(&path).expect("read"), s);
    let missing = Services::from_file(&dir.path().join("nope.toml")).expect_err("refused");
    assert!(matches!(missing, ServicesError::Read(_)));
    assert!(missing.to_string().contains("nope.toml"));
}

/// Catches: a service name using digits or `-` being refused, as if names were letters only.
#[test]
fn a_service_name_may_hold_digits_and_hyphens() {
    let text = "[[service]]\nname = \"web-2\"\n[service.roles.daemon]\nmin_up = 0\n";
    assert!(
        Services::parse(text)
            .expect("parsed")
            .get("web-2")
            .is_some()
    );
}

/// Catches: a services file that is accepted although it cannot work: each refusal names
/// what is wrong (not TOML or an unknown key, a bad or repeated name, kbf defined after
/// another service, no roles, a role that is not a host role, a count over the limit, a
/// role waiting for a role the service lacks, an `after` that names an unknown or later
/// service, and a CI service that does not exist).
#[test]
fn a_services_file_that_cannot_work_is_refused() {
    let role = "[service.roles.daemon]\n";
    let svc = |name: &str, extra: &str, roles: &str| {
        format!("[[service]]\nname = \"{name}\"\n{extra}\n{roles}")
    };
    assert!(matches!(
        Services::parse("service = 3").expect_err("refused"),
        ServicesError::Parse(_)
    ));
    assert!(matches!(
        Services::parse("[[service]]\nname = \"a\"\nbogus = 1\n").expect_err("refused"),
        ServicesError::Parse(_)
    ));
    for bad in ["", "Upper", "has space"] {
        assert!(
            refusal(&svc(bad, "", role)).contains("lowercase"),
            "{bad:?}"
        );
    }
    let kbf_later = format!("{}{}", svc("a", "", role), svc("kbf", "", role));
    assert!(refusal(&kbf_later).contains("first entry"));
    let twice = format!("{}{}", svc("a", "", role), svc("a", "", role));
    assert!(refusal(&twice).contains("twice"));
    assert!(refusal(&svc("a", "", "")).contains("no roles"));
    assert!(refusal(&svc("a", "", "[service.roles.cook]\n")).contains("cook is not a host role"));
    assert!(refusal(&svc("a", "", "[service.roles.cache]\n")).contains("cache is not a host role"));
    assert!(
        refusal(&svc("a", "", "[service.roles.daemon]\nmin_up = 10001\n")).contains("over 10000")
    );
    assert!(
        refusal(&svc(
            "a",
            "",
            "[service.roles.daemon]\nmax_updating = 10001\n"
        ))
        .contains("over 10000")
    );
    assert!(
        refusal(&svc(
            "a",
            "",
            "[service.roles.daemon]\nafter = [\"cook\"]\n"
        ))
        .contains("cook is not a host role")
    );
    assert!(
        refusal(&svc(
            "a",
            "",
            "[service.roles.daemon]\nafter = [\"storage\"]\n"
        ))
        .contains("a role it does not have")
    );
    assert!(refusal(&svc("a", "after = [\"b\"]", role)).contains("not kbf or defined earlier"));
    let later = format!(
        "{}{}",
        svc("a", "after = [\"b\"]", role),
        svc("b", "", role)
    );
    assert!(refusal(&later).contains("defined earlier"));
    // Which services a token may set is the allow-list's: the old key is refused by name.
    assert!(
        refusal(&format!("ci_services = [\"kbf\"]\n{}", svc("a", "", role)))
            .contains("ci_services")
    );
    let after_kbf = svc(
        "a",
        "after = [\"kbf\"]",
        "[service.roles.daemon]\nafter = []\n",
    );
    Services::parse(&after_kbf).expect("kbf may be waited for");
    Services::parse(&svc("js-server2", "", role)).expect("letters, digits and - make a name");
}

/// Catches: `after` not holding the second service until the first has converged: the agent
/// host is told nothing while a server host is behind, while it is updating, and while a
/// server host is ordered; once every live server host runs the desired version it is told
/// to update. A silent server host does not hold it, and a first service with no desired
/// state holds nothing.
#[test]
fn a_service_waits_for_the_services_it_comes_after() {
    let s = Services::parse(TWO).expect("parsed");
    let want = desired("2.0", 0, 0);
    let w = wants(&[("server", &want), ("agent", &want)]);
    let fleet_of = |server: kbf_updater::wire::Heartbeat, seen: u64| {
        let mut f = Fleet::new(Duration::ZERO);
        f.record_in(
            "server",
            id("s1"),
            HostRole::DAEMON,
            server,
            Duration::from_secs(seen),
        );
        f.record_in("agent", id("a1"), HostRole::DAEMON, beat("1.0"), NOW);
        f
    };
    let behind = fleet_of(beat("1.0"), 1_000);
    assert_eq!(ask(&s, &w, &behind, "agent", "a1"), Reply::Nothing);
    assert_eq!(ask(&s, &w, &behind, "server", "s1"), update("2.0"));
    let busy = fleet_of(updating(beat("1.0"), "2.0"), 1_000);
    assert_eq!(ask(&s, &w, &busy, "agent", "a1"), Reply::Nothing);
    let done = fleet_of(beat("2.0"), 1_000);
    assert_eq!(ask(&s, &w, &done, "agent", "a1"), update("2.0"));
    let gone = fleet_of(beat("1.0"), 900);
    assert_eq!(ask(&s, &w, &gone, "agent", "a1"), update("2.0"));
    let agent_only = wants(&[("agent", &want)]);
    assert_eq!(ask(&s, &agent_only, &behind, "agent", "a1"), update("2.0"));
}

/// Catches: a host running two updates at once: a host whose other service is updating, or
/// has an order outstanding for it, is told nothing; an order that has aged out, or one for
/// the service asked about, does not block; and an order for one service is not ended by
/// the host's heartbeat of the other.
#[test]
fn a_host_updates_one_service_at_a_time() {
    let text = "[[service]]\nname = \"one\"\n[service.roles.daemon]\nmin_up = 0\n\
                [[service]]\nname = \"two\"\n[service.roles.daemon]\nmin_up = 0\n";
    let s = Services::parse(text).expect("parsed");
    let want = desired("2.0", 0, 0);
    let w = wants(&[("one", &want), ("two", &want)]);
    let mut f = Fleet::new(Duration::ZERO);
    f.record_in("one", id("h"), HostRole::DAEMON, beat("1.0"), NOW);
    f.record_in("two", id("h"), HostRole::DAEMON, beat("1.0"), NOW);
    assert!(!f.is_busy_besides("two", &id("h"), NOW));
    assert_eq!(ask(&s, &w, &f, "two", "h"), update("2.0"));

    f.order("one", id("h"), version("2.0"), NOW);
    assert_eq!(
        ask(&s, &w, &f, "two", "h"),
        Reply::Nothing,
        "ordered for one"
    );
    assert_eq!(
        ask(&s, &w, &f, "one", "h"),
        update("2.0"),
        "its own order repeats"
    );
    f.record_in("two", id("h"), HostRole::DAEMON, beat("1.0"), NOW);
    assert_eq!(
        ask(&s, &w, &f, "two", "h"),
        Reply::Nothing,
        "two's beat leaves one's order"
    );
    let later = NOW + ORDER_AGE;
    assert_eq!(
        decide_in(&s, &w, "two", &f, &id("h"), later),
        update("2.0"),
        "aged out"
    );

    f.record_in(
        "one",
        id("h"),
        HostRole::DAEMON,
        updating(beat("1.0"), "2.0"),
        NOW,
    );
    assert_eq!(
        ask(&s, &w, &f, "two", "h"),
        Reply::Nothing,
        "one is updating"
    );
    assert_eq!(
        ask(&s, &w, &f, "one", "h"),
        Reply::Nothing,
        "and not told again"
    );
    f.record_in("one", id("h"), HostRole::DAEMON, beat("2.0"), NOW);
    assert_eq!(ask(&s, &w, &f, "two", "h"), update("2.0"));
}

/// Catches: the lone-host guard not following the definition: a lone host of a role with
/// `min_up` 0 is ordered; with `min_up` 1 it is not until a second host is up; a role the
/// service lacks, a service with no definition and one with no desired state order nothing.
#[test]
fn a_lone_host_is_ordered_only_when_the_definition_allows_it() {
    let text = "[[service]]\nname = \"loose\"\n[service.roles.daemon]\nmin_up = 0\n\
                [[service]]\nname = \"tight\"\n[service.roles.daemon]\nmin_up = 1\n";
    let s = Services::parse(text).expect("parsed");
    let want = desired("2.0", 0, 0);
    let w = wants(&[("loose", &want), ("tight", &want)]);
    let mut f = Fleet::new(Duration::ZERO);
    for service in ["loose", "tight"] {
        f.record_in(service, id("d1"), HostRole::DAEMON, beat("1.0"), NOW);
        f.record_in(service, id("m1"), HostRole::MAC_DAEMON, beat("1.0"), NOW);
    }
    assert_eq!(ask(&s, &w, &f, "loose", "d1"), update("2.0"));
    assert_eq!(ask(&s, &w, &f, "tight", "d1"), Reply::Nothing);
    f.record_in("tight", id("d2"), HostRole::DAEMON, beat("1.0"), NOW);
    assert_eq!(ask(&s, &w, &f, "tight", "d1"), update("2.0"));
    assert_eq!(
        ask(&s, &w, &f, "loose", "m1"),
        Reply::Nothing,
        "no mac-daemon role"
    );
    assert_eq!(ask(&s, &w, &f, "nope", "d1"), Reply::Nothing);
    assert_eq!(ask(&s, &BTreeMap::new(), &f, "loose", "d1"), Reply::Nothing);
}

/// Catches: a definition's limits being ignored: `max_updating` of the definition, not the
/// desired state's, caps the role; `never_last` keeps one host up although `min_up` is 0;
/// and a role waits for the roles its definition names.
#[test]
fn a_definitions_limits_and_waits_decide_over_the_desired_state() {
    let text = "[[service]]\nname = \"svc\"\n\
        [service.roles.daemon]\nmax_updating = 1\nmin_up = 0\nnever_last = true\n\
        [service.roles.storage]\nmax_updating = 1\nafter = [\"daemon\"]\nmin_up = 0\n";
    let s = Services::parse(text).expect("parsed");
    let mut want = desired("2.0", 0, 0);
    want.max_updating.set("daemon", 4);
    let w = wants(&[("svc", &want)]);
    let mut f = Fleet::new(Duration::ZERO);
    f.record_in("svc", id("d1"), HostRole::DAEMON, beat("1.0"), NOW);
    assert_eq!(
        ask(&s, &w, &f, "svc", "d1"),
        Reply::Nothing,
        "never the last"
    );
    f.record_in(
        "svc",
        id("d2"),
        HostRole::DAEMON,
        updating(beat("1.0"), "2.0"),
        NOW,
    );
    f.record_in("svc", id("d3"), HostRole::DAEMON, beat("1.0"), NOW);
    assert_eq!(
        ask(&s, &w, &f, "svc", "d3"),
        Reply::Nothing,
        "the definition allows one at a time"
    );
    f.record_in("svc", id("d2"), HostRole::DAEMON, beat("2.0"), NOW);
    f.record_in("svc", id("t1"), HostRole::STORAGE, beat("1.0"), NOW);
    assert_eq!(
        ask(&s, &w, &f, "svc", "t1"),
        Reply::Nothing,
        "storage waits for daemons"
    );
    f.record_in("svc", id("d1"), HostRole::DAEMON, beat("2.0"), NOW);
    f.record_in("svc", id("d3"), HostRole::DAEMON, beat("2.0"), NOW);
    assert_eq!(ask(&s, &w, &f, "svc", "t1"), update("2.0"));
}

/// Catches: the services file's name check drifting from the wire's: a name of exactly
/// `MAX_SERVICE_NAME` bytes is defined, one byte more is refused and the refusal says why.
#[test]
fn a_service_name_has_the_wires_length_cap() {
    let role = "[service.roles.daemon]\n";
    let svc = |name: &str| format!("[[service]]\nname = \"{name}\"\n{role}");
    Services::parse(&svc(&"a".repeat(MAX_SERVICE_NAME))).expect("at the cap");
    let why = refusal(&svc(&"a".repeat(MAX_SERVICE_NAME + 1)));
    assert!(why.contains("64 bytes"), "{why}");
}

/// Catches: no cap on the services a file defines, or one off by one: exactly
/// `MAX_SERVICES` are defined, one more is refused.
#[test]
fn a_services_file_defines_at_most_the_most_services() {
    let file = |n: usize| {
        (0..n)
            .map(|i| format!("[[service]]\nname = \"svc-{i}\"\n[service.roles.daemon]\n"))
            .collect::<String>()
    };
    Services::parse(&file(MAX_SERVICES)).expect("at the cap");
    assert!(refusal(&file(MAX_SERVICES + 1)).contains("more than 16 services"));
}
