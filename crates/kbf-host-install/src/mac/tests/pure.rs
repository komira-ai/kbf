//! The readers and builders that touch nothing: tool output, command lines, the sums, the
//! node's settings, the allowlist, the status line, the links.

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;
use std::time::Duration;

use super::fx::{Fx, NEW, NEW_VERSION, OLD, OLD_VERSION, mkdir, write};
use crate::mac::channel::{DEFAULT_SOURCE_REF, MacAllowlist, Source, is_branch_ref};
use crate::mac::conf::{node_id, parse_node_id};
use crate::mac::fetch::AssetKind;
use crate::mac::kit::{self, Links};
use crate::mac::release::{
    PickError, SUMS_NAME, bundle_name, bundle_of, commit_prefix, is_commit, listed_hash, pick,
    pick_named, tarball_name,
};
use crate::mac::status::{self, ApplyState};
use crate::mac::tools;
use crate::outcome::Refusal;
use crate::request::Channel;
use crate::runner::Identity;

const LIMIT: Duration = Duration::from_secs(7);

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

// ---- tool output ----

/// The daemon's pid and state are read from the lines of `launchctl print` that name them
/// at the top of the service, and a pid of 0 or text is no pid.
#[test]
fn launchctl_print_gives_the_pid_and_the_state() {
    let text = "system/com.komira.kbf-daemon = {\n\tactive count = 1\n\tpid = 4242\n\tstate = running\n}\n";
    assert_eq!(tools::print_pid(text), Some(4242));
    assert!(tools::print_running(text));
    assert_eq!(tools::print_pid("\tpid = 0\n"), None);
    assert_eq!(tools::print_pid("\tpid = x\n"), None);
    assert_eq!(tools::print_pid("\tstate = waiting\n"), None);
    assert!(!tools::print_running("\tstate = not running\n"));
    assert!(!tools::print_running(""));
}

#[test]
fn lsof_output_gives_the_open_files() {
    let text = "p4242\nftxt\nn/usr/local/kbf/releases/x/kbf\nn/usr/lib/dyld\n";
    assert_eq!(
        tools::lsof_paths(text),
        ["/usr/local/kbf/releases/x/kbf", "/usr/lib/dyld"]
    );
    assert!(tools::lsof_paths("p1\n").is_empty());
}

/// Only the quarantine attribute itself counts, not a name that contains it.
#[test]
fn the_quarantine_attribute_is_found_by_name() {
    assert!(tools::has_quarantine("a\ncom.apple.quarantine\n"));
    assert!(tools::has_quarantine("  com.apple.quarantine  \n"));
    assert!(!tools::has_quarantine(
        "com.apple.quarantine.x\nnot-com.apple.quarantine\n"
    ));
    assert!(!tools::has_quarantine(""));
}

/// The hardened runtime is a flag of the code directory line; a word `runtime` elsewhere is
/// not one.
#[test]
fn the_hardened_runtime_is_a_flag_of_the_code_directory() {
    assert!(tools::has_hardened_runtime(
        "CodeDirectory v=20500 size=1 flags=0x10002(adhoc,runtime) hashes=1\n"
    ));
    assert!(tools::has_hardened_runtime("flags=0x10000(runtime)\n"));
    assert!(!tools::has_hardened_runtime("flags=0x2(adhoc)\n"));
    assert!(!tools::has_hardened_runtime(
        "Identifier=runtime\nflags=0x0(none)\n"
    ));
    assert!(!tools::has_hardened_runtime("flags=0x10002\n"));
    assert!(!tools::has_hardened_runtime("flags=0x2(adhoc,runtimex)\n"));
}

#[test]
fn account_ids_are_read_from_dscl() {
    assert_eq!(
        tools::parse_ids("PrimaryGroupID: 4294967294\nUniqueID: 4294967294\n"),
        Some(Identity {
            uid: 4_294_967_294,
            gid: 4_294_967_294
        })
    );
    assert_eq!(tools::parse_ids("UniqueID: 301\n"), None);
    assert_eq!(tools::parse_ids("PrimaryGroupID: 20\n"), None);
    assert_eq!(tools::parse_ids("UniqueID: x\nPrimaryGroupID: 20\n"), None);
    assert_eq!(tools::parse_ids(""), None);
}

// ---- command lines ----

/// The tools are started by absolute path with the arguments the Mac's own documentation
/// gives; nothing goes through a shell.
#[test]
fn the_command_lines() {
    let p = Path::new("/p/kbf");
    let c = tools::print("com.x", LIMIT);
    assert_eq!(
        (c.program.as_str(), c.args, c.timeout),
        (
            "/bin/launchctl",
            strings(&["print", "system/com.x"]),
            Some(LIMIT)
        )
    );
    assert_eq!(
        tools::bootout("com.x", LIMIT).args,
        strings(&["bootout", "system/com.x"])
    );
    assert_eq!(
        tools::bootstrap(Path::new("/a.plist"), LIMIT).args,
        strings(&["bootstrap", "system", "/a.plist"])
    );
    assert_eq!(
        tools::lsof_text(7, LIMIT).args,
        strings(&["-nPw", "-a", "-p", "7", "-d", "txt", "-F", "n"])
    );
    assert_eq!(tools::xattr_list(p, LIMIT).args, strings(&["/p/kbf"]));
    assert_eq!(
        tools::xattr_remove_quarantine(p, LIMIT).args,
        strings(&["-d", "com.apple.quarantine", "/p/kbf"])
    );
    assert_eq!(
        tools::codesign_verify(p, LIMIT).args,
        strings(&["--verify", "--strict", "/p/kbf"])
    );
    assert_eq!(
        tools::codesign_describe(p, LIMIT).args,
        strings(&["-d", "--verbose=4", "/p/kbf"])
    );
    // The account's record in the local directory: its path ends with its name.
    let ids = tools::dscl_ids("_kbf", LIMIT).args;
    assert_eq!((ids[0].as_str(), ids[1].as_str()), (".", "-read"));
    assert!(
        ids[2].ends_with("/_kbf") && ids[2].starts_with('/'),
        "{ids:?}"
    );
    assert_eq!(ids[3..], strings(&["UniqueID", "PrimaryGroupID"]));
    let sudo = tools::sudo_root(Path::new("/x/host-install"), "query");
    assert_eq!(sudo.program, "/usr/bin/sudo");
    assert_eq!(sudo.args, strings(&["-n", "/x/host-install", "--as-root"]));
    assert_eq!(sudo.stdin, b"query");
}

/// The attestation check is the installer's, offline: the bundle, the trusted root, the
/// repository, workflow, issuer, branch and commit, hosted runners only, as the account it
/// is given, with every proxy dead.
#[test]
fn the_attestation_command() {
    let who = Identity { uid: 5, gid: 6 };
    let cmd = tools::Attest {
        gh: Path::new("/t/gh"),
        tarball: Path::new("/s/t.tar.gz"),
        bundle: Path::new("/s/t.tar.gz.sigstore.json"),
        trusted_root: Path::new("/t/root.jsonl"),
        repo: "komira-ai/kbf",
        workflow: "komira-ai/kbf/.github/workflows/artifacts.yml",
        source_ref: "refs/heads/main",
        commit: NEW,
        who,
        home: Path::new("/s/home"),
    }
    .cmd(LIMIT);
    assert_eq!(cmd.program, "/t/gh");
    assert_eq!(
        cmd.args,
        strings(&[
            "attestation",
            "verify",
            "/s/t.tar.gz",
            "--bundle",
            "/s/t.tar.gz.sigstore.json",
            "--custom-trusted-root",
            "/t/root.jsonl",
            "--repo",
            "komira-ai/kbf",
            "--signer-workflow",
            "komira-ai/kbf/.github/workflows/artifacts.yml",
            "--cert-oidc-issuer",
            "https://token.actions.githubusercontent.com",
            "--source-ref",
            "refs/heads/main",
            "--source-digest",
            NEW,
            "--predicate-type",
            "https://slsa.dev/provenance/v1",
            "--digest-alg",
            "sha256",
            "--deny-self-hosted-runners",
        ])
    );
    assert_eq!(cmd.run_as, Some(who));
    assert_eq!(cmd.timeout, Some(LIMIT));
    let env = |name: &str| {
        cmd.env
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    };
    for proxy in [
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "ALL_PROXY",
        "all_proxy",
    ] {
        assert_eq!(env(proxy), Some("http://127.0.0.1:9"), "{proxy}");
    }
    assert_eq!(env("HOME"), Some("/s/home"));
    assert_eq!(env("GH_CONFIG_DIR"), Some("/s/home"));
    assert_eq!(env("GH_NO_UPDATE_NOTIFIER"), Some("1"));
}

#[test]
fn the_configuration_check_command() {
    let who = Identity { uid: 301, gid: 301 };
    let cmd = tools::config_check(
        Path::new("/s/kbf"),
        Path::new("/c/kbf.toml"),
        Path::new("/h"),
        who,
        LIMIT,
    );
    assert_eq!(cmd.program, "/s/kbf");
    assert_eq!(
        cmd.args,
        strings(&["--mode=daemon", "--config", "/c/kbf.toml", "--check"])
    );
    assert_eq!(cmd.run_as, Some(who));
    assert_eq!(cmd.env, [("HOME".to_owned(), "/h".to_owned())]);
}

// ---- the release a version names ----

#[test]
fn a_version_names_the_commit_digits_after_the_last_plus() {
    for (version, want) in [
        ("0.1.7+gbbbbbbbbbbbb-3", Some("bbbbbbbbbbbb")),
        ("0.1.7+g0123456789ab", Some("0123456789ab")),
        ("0.1.7~feat+g0123456789ab-9", Some("0123456789ab")),
        ("0.1.7+gx+g0123456789ab-9", Some("0123456789ab")),
        ("0.1.7+g0123456789a-3", None),
        ("0.1.7+g0123456789abc-3", None),
        ("0.1.7+g0123456789AB-3", None),
        ("0.1.7+g0123456789ab.1", None),
        ("0.1.7-3", None),
        ("1.2.3-1", None),
        // A deb's version has no `g`: `<semver>+<12 hex>`, with or without `-<run>`.
        ("0.1.0+cb6f107dd94c", Some("cb6f107dd94c")),
        ("0.1.7+0123456789ab-3", Some("0123456789ab")),
        ("0.1.7~feat+0123456789ab-9", Some("0123456789ab")),
        ("0.1.7+g0123456789ab+0123456789cd", Some("0123456789cd")),
        ("0.1.7+0123456789a", None),
        ("0.1.7+0123456789abc", None),
        ("0.1.7+0123456789AB", None),
        ("0.1.7+0123456789ax", None),
        ("0.1.7+gg0123456789ab", None),
        ("0.1.7+0123456789ab.1", None),
        ("0.1.7+", None),
        ("0123456789ab", None),
    ] {
        assert_eq!(commit_prefix(version), want, "{version}");
    }
}

#[test]
fn a_commit_is_forty_lower_case_hex_digits() {
    assert!(is_commit(NEW));
    assert!(!is_commit(&NEW[1..]));
    assert!(!is_commit(&format!("{}G", &NEW[1..])));
    assert!(!is_commit(&NEW.to_uppercase()));
    assert_eq!(tarball_name(NEW), format!("kbf-{NEW}-darwin-arm64.tar.gz"));
    assert_eq!(
        bundle_name(NEW),
        format!("kbf-{NEW}-darwin-arm64.tar.gz.sigstore.json")
    );
}

fn sum_line(hash_digit: char, name: &str) -> String {
    format!("{}  {name}\n", hash_digit.to_string().repeat(64))
}

/// The one tarball of the commit is picked with the hash the sums give; other files of the
/// sums (packages, facts, another platform) are not candidates.
#[test]
fn the_sums_pick_the_one_tarball_of_the_commit() {
    let other = format!("{}1111", &NEW[..36]);
    let sums = [
        sum_line('1', &tarball_name(OLD)),
        sum_line('2', &tarball_name(NEW)),
        sum_line('3', &format!("kbf-{NEW}-linux-x86_64.tar.gz")),
        sum_line('4', &format!("kbf-installer-{NEW}-darwin-arm64.tar.gz")),
        sum_line('5', &format!("kbf-{other}.tar.gz")),
        sum_line('6', &format!("kbf-{}-darwin-arm64.tar.gz", &NEW[..39])),
        sum_line(
            '7',
            "kbf-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-darwin-arm64.tar.gz.sigstore.json",
        ),
    ]
    .concat();
    let picked = pick(&sums, "bbbbbbbbbbbb").unwrap();
    assert_eq!(
        (picked.commit.as_str(), picked.sha256),
        (NEW, "2".repeat(64))
    );
    assert_eq!(pick(&sums, "cccccccccccc"), Err(PickError::Unknown));
}

/// Without sums the release is picked among the file names of the directory: the one
/// tarball of the platform whose commit starts with the digits; other names are ignored,
/// and a name listed twice is one commit.
#[test]
fn a_listing_picks_the_one_tarball_of_the_commit() {
    let other = tarball_name(&format!("{}c", &NEW[..39]));
    let names = [
        tarball_name(OLD),
        tarball_name(NEW),
        format!("{}.sigstore.json", tarball_name(NEW)),
        format!("kbf-{NEW}-linux-x86_64.tar.gz"),
        format!("kbf-installer-{NEW}-darwin-arm64.tar.gz"),
        format!("kbf-{}-darwin-arm64.tar.gz", &NEW[..39]),
        "SHA256SUMS".to_owned(),
    ];
    let listed = || names.iter().map(String::as_str);
    assert_eq!(pick_named(listed(), "bbbbbbbbbbbb"), Ok(NEW.to_owned()));
    assert_eq!(pick_named(listed(), "aaaaaaaaaaaa"), Ok(OLD.to_owned()));
    assert_eq!(
        pick_named(listed(), "cccccccccccc"),
        Err(PickError::Unknown)
    );
    assert_eq!(pick_named([], "bbbbbbbbbbbb"), Err(PickError::Unknown));
    assert_eq!(
        pick_named(listed().chain([other.as_str()]), "bbbbbbbbbbbb"),
        Err(PickError::Ambiguous)
    );
    // The same name twice is still one release.
    assert_eq!(
        pick_named(listed().chain([names[1].as_str()]), "bbbbbbbbbbbb"),
        Ok(NEW.to_owned())
    );
}

#[test]
fn two_commits_with_the_same_digits_are_not_picked_between() {
    let sums = [
        sum_line('1', &tarball_name(NEW)),
        sum_line('2', &tarball_name(&format!("{}c", &NEW[..39]))),
    ]
    .concat();
    assert_eq!(pick(&sums, "bbbbbbbbbbbb"), Err(PickError::Ambiguous));
}

/// Sums that are not exactly `<64 hex>  <name>` lines are refused whole, whatever else
/// they hold.
#[test]
fn malformed_sums_are_refused() {
    let good = sum_line('1', &tarball_name(NEW));
    for (why, text) in [
        ("no final newline", good.trim_end().to_owned()),
        (
            "one space",
            format!("{} {}\n", "1".repeat(64), tarball_name(NEW)),
        ),
        (
            "short hash",
            format!("{}  {}\n", "1".repeat(63), tarball_name(NEW)),
        ),
        (
            "upper-case hash",
            format!("{}  {}\n", "A".repeat(64), tarball_name(NEW)),
        ),
        (
            "path in name",
            format!("{}  dist/{}\n", "1".repeat(64), tarball_name(NEW)),
        ),
        ("empty name", format!("{}  \n", "1".repeat(64))),
        (
            "a name twice",
            format!("{good}{}", sum_line('2', &tarball_name(NEW))),
        ),
        ("a stray line", format!("{good}\n")),
    ] {
        assert_eq!(
            pick(&text, "bbbbbbbbbbbb"),
            Err(PickError::Malformed),
            "{why}"
        );
    }
    assert_eq!(pick("", "bbbbbbbbbbbb"), Err(PickError::Unknown));
}

// ---- av2_mac.conf ----

#[test]
fn the_node_id_is_read_from_the_settings() {
    let kit = "# kit settings\n\nFRONTEND_NAME=frontend.example.invalid\nWORKER_PORT=8981\nNODE_ID=mac-a\n";
    assert_eq!(parse_node_id(kit).as_deref(), Some("mac-a"));
    assert_eq!(
        parse_node_id("NODE_ID=\"mac-b\"\n").as_deref(),
        Some("mac-b")
    );
    assert_eq!(parse_node_id("NODE_ID='mac-c'").as_deref(), Some("mac-c"));
    assert_eq!(parse_node_id("_X=a b\n"), None);
    assert_eq!(
        parse_node_id("X1_2=a\nNODE_ID=mac-d\n").as_deref(),
        Some("mac-d")
    );
}

/// A file a shell would read differently than this reader does is not believed at all.
#[test]
fn a_settings_file_that_is_not_plain_gives_no_node_id() {
    for (why, text) in [
        ("no id", "WORKER_PORT=8981\n"),
        ("an id twice", "NODE_ID=a\nNODE_ID=b\n"),
        ("not a label", "NODE_ID=Mac_A\n"),
        ("empty", "NODE_ID=\n"),
        ("export", "export NODE_ID=a\n"),
        ("lower-case key", "node_id=a\n"),
        ("no equals", "NODE_ID\n"),
        ("indented comment", " # c\nNODE_ID=a\n"),
        ("expansion", "NODE_ID=a\nHOME=$HOME\n"),
        ("command", "NODE_ID=a\nX=$(id)\n"),
        ("backtick", "NODE_ID=a\nX=`id`\n"),
        ("unquoted space", "NODE_ID=a\nX=b c\n"),
        ("quoted expansion", "NODE_ID=a\nX=\"$HOME\"\n"),
        ("quoted backslash", "NODE_ID=a\nX='a\\b'\n"),
        ("mismatched quote", "NODE_ID=a\nX='a\"\n"),
        ("one quote", "NODE_ID=a\nX='\n"),
        ("quoted newline text", "NODE_ID=a\nX=\"a\u{7}b\"\n"),
        ("key with a dash", "NODE-ID=a\n"),
        ("key starting with a digit", "1X=a\n"),
    ] {
        assert_eq!(parse_node_id(text), None, "{why}");
    }
}

#[test]
fn the_settings_file_must_be_roots() {
    let fx = Fx::new("mac-conf");
    assert_eq!(
        node_id(&fx.host.kit_trust, &fx.host.node_conf).as_deref(),
        Some("mac-a")
    );
    fs::set_permissions(
        &fx.host.node_conf,
        std::os::unix::fs::PermissionsExt::from_mode(0o666),
    )
    .unwrap();
    assert_eq!(node_id(&fx.host.kit_trust, &fx.host.node_conf), None);
    fs::remove_file(&fx.host.node_conf).unwrap();
    assert_eq!(node_id(&fx.host.kit_trust, &fx.host.node_conf), None);
}

// ---- the allowlist ----

fn allowlist(fx: &Fx, text: &str) -> Result<MacAllowlist, Refusal> {
    write(&fx.host.allowlist, 0o644, text.as_bytes());
    MacAllowlist::load(&fx.host.trust, &fx.host.allowlist)
}

fn stable() -> Channel {
    Channel::new("stable").unwrap()
}

#[test]
fn a_channel_names_a_drop_directory_and_pinned_sums() {
    let fx = Fx::new("mac-channel");
    let list = allowlist(
        &fx,
        &format!(
            "[channels.stable]\ndir = \"/d\"\nsums = \"{}\"\n",
            fx.sums.display()
        ),
    )
    .unwrap();
    let channel = list.channel(&fx.host.trust, &stable()).unwrap();
    assert_eq!(channel.source, Source::Dir("/d".into()));
    assert_eq!(channel.sums, Some(fx.sums.clone()));
    assert_eq!(channel.source_ref, DEFAULT_SOURCE_REF);
    let list = allowlist(
        &fx,
        &format!("[channels.stable]\ndir = \"/d\"\nsums = \"{}\"\nsource_ref = \"refs/heads/release/1\"\n", fx.sums.display()),
    )
    .unwrap();
    assert_eq!(
        list.channel(&fx.host.trust, &stable()).unwrap().source_ref,
        "refs/heads/release/1"
    );
    assert_eq!(
        list.channel(&fx.host.trust, &Channel::new("beta").unwrap())
            .map_err(|e| e.refusal()),
        Err(Refusal::ChannelNotAllowed)
    );
}

/// A channel needs no sums: the attestation is the trust anchor. Sums that are named are
/// still checked to be root's, and a channel without them reads as having none.
#[test]
fn a_channel_may_have_no_sums() {
    let fx = Fx::new("mac-channel-nosums");
    let list = allowlist(&fx, "[channels.stable]\ndir = \"/d\"\n").unwrap();
    let channel = list.channel(&fx.host.trust, &stable()).unwrap();
    assert_eq!(
        (channel.source, channel.sums, channel.source_ref.as_str()),
        (Source::Dir("/d".into()), None, DEFAULT_SOURCE_REF)
    );
    // Named sums that others can write are unsafe; sums that are not there are invalid.
    let list = allowlist(
        &fx,
        &format!(
            "[channels.stable]\ndir = \"/d\"\nsums = \"{}\"\n",
            fx.sums.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&fx.sums, fs::Permissions::from_mode(0o666)).unwrap();
    assert_eq!(
        list.channel(&fx.host.trust, &stable())
            .map_err(|e| e.refusal()),
        Err(Refusal::ConfigUnsafe)
    );
}

#[test]
fn an_allowlist_that_is_not_plain_is_refused() {
    let fx = Fx::new("mac-channel-bad");
    let s = fx.sums.display();
    for (why, text) in [
        ("not toml", "channels = ".to_owned()),
        ("no channels", String::new()),
        (
            "an unknown key",
            format!("[channels.stable]\ndir = \"/d\"\nsums = \"{s}\"\nurl = \"x\"\n"),
        ),
        ("no dir", format!("[channels.stable]\nsums = \"{s}\"\n")),
        (
            "a relative dir",
            format!("[channels.stable]\ndir = \"d\"\nsums = \"{s}\"\n"),
        ),
        (
            "a relative sums",
            "[channels.stable]\ndir = \"/d\"\nsums = \"s\"\n".to_owned(),
        ),
        (
            "a control character",
            format!("[channels.stable]\ndir = \"/d\\u0007\"\nsums = \"{s}\"\n"),
        ),
        (
            "a bad channel name",
            format!("[channels.Stable]\ndir = \"/d\"\nsums = \"{s}\"\n"),
        ),
        (
            "a bad ref",
            format!("[channels.stable]\ndir = \"/d\"\nsums = \"{s}\"\nsource_ref = \"main\"\n"),
        ),
    ] {
        assert_eq!(
            allowlist(&fx, &text).err(),
            Some(Refusal::ConfigInvalid),
            "{why}"
        );
    }
}

#[test]
fn the_allowlist_and_the_pinned_sums_must_be_roots() {
    let fx = Fx::new("mac-channel-trust");
    let text = format!(
        "[channels.stable]\ndir = \"/d\"\nsums = \"{}\"\n",
        fx.sums.display()
    );
    write(&fx.host.allowlist, 0o666, text.as_bytes());
    assert_eq!(
        MacAllowlist::load(&fx.host.trust, &fx.host.allowlist).err(),
        Some(Refusal::ConfigUnsafe)
    );
    let list = allowlist(&fx, &text).unwrap();
    fs::set_permissions(
        &fx.sums,
        std::os::unix::fs::PermissionsExt::from_mode(0o664),
    )
    .unwrap();
    assert_eq!(
        list.channel(&fx.host.trust, &stable())
            .map_err(|e| e.refusal()),
        Err(Refusal::ConfigUnsafe)
    );
    fs::remove_file(&fx.sums).unwrap();
    assert_eq!(
        list.channel(&fx.host.trust, &stable())
            .map_err(|e| e.refusal()),
        Err(Refusal::ConfigInvalid)
    );
    fs::remove_file(&fx.host.allowlist).unwrap();
    assert_eq!(
        MacAllowlist::load(&fx.host.trust, &fx.host.allowlist).err(),
        Some(Refusal::ConfigInvalid)
    );
}

#[test]
fn a_source_ref_is_a_branch_that_is_safe_as_an_argument() {
    for good in [
        "refs/heads/main",
        "refs/heads/release/1.2",
        "refs/heads/a_b-c",
    ] {
        assert!(is_branch_ref(good), "{good}");
    }
    for bad in [
        "main",
        "refs/tags/v1",
        "refs/heads/",
        "refs/heads//a",
        "refs/heads/a//b",
        "refs/heads/a/",
        "refs/heads/a..b",
        "refs/heads/a b",
        "refs/heads//x",
        "refs/heads/a;b",
    ] {
        assert!(!is_branch_ref(bad), "{bad}");
    }
}

// ---- the status line ----

#[test]
fn the_status_line_round_trips() {
    for state in [
        ApplyState::Running,
        ApplyState::Done,
        ApplyState::Failed("not_welcomed:rolled_back".into()),
    ] {
        assert_eq!(ApplyState::parse(&state.line()), Some(state));
    }
    assert_eq!(ApplyState::Failed("x".into()).line(), "failed:x");
    for bad in [
        "",
        "Running",
        "failed:",
        "failed:A",
        "failed:a b",
        &format!("failed:{}", "a".repeat(65)),
        "done ",
        "ok",
    ] {
        assert_eq!(ApplyState::parse(bad), None, "{bad:?}");
    }
}

#[test]
fn the_status_is_written_whole_and_read_back() {
    let fx = Fx::new("mac-status");
    mkdir(&fx.host.work, 0o711);
    assert_eq!(status::read(&fx.host), None);
    status::write(&fx.host, &ApplyState::Running).unwrap();
    assert_eq!(fx.status(), "running\n");
    assert_eq!(status::read(&fx.host), Some(ApplyState::Running));
    status::write(&fx.host, &ApplyState::Done).unwrap();
    assert_eq!(status::read(&fx.host), Some(ApplyState::Done));
    // A leftover aside file does not stop the next write.
    write(&fx.host.work.join("status.new"), 0o600, b"junk");
    status::write(&fx.host, &ApplyState::Failed("x".into())).unwrap();
    assert_eq!(status::read(&fx.host), Some(ApplyState::Failed("x".into())));
    // No final newline, or another text, is no state.
    write(&fx.host.status_file(), 0o644, b"done");
    assert_eq!(status::read(&fx.host), None);
    write(&fx.host.status_file(), 0o644, b"what\n");
    assert_eq!(status::read(&fx.host), None);
    // A write into a directory that is not there fails.
    let mut gone = fx.host.clone();
    gone.work = fx.dir.join("nowhere");
    assert!(status::write(&gone, &ApplyState::Done).is_err());
}

/// An apply that is `running` while its job is not running, and no install holds the lock,
/// was lost; anything that makes the job's state unknown keeps it running.
#[test]
fn a_running_apply_whose_job_is_gone_is_reported_lost() {
    let fx = Fx::new("mac-status-lost");
    mkdir(&fx.host.work, 0o711);
    let sim = fx.sim();
    assert_eq!(status::effective(&fx.host, &sim), None);
    status::write(&fx.host, &ApplyState::Running).unwrap();
    // The job is not loaded at all.
    assert_eq!(
        status::effective(&fx.host, &sim),
        Some(ApplyState::Failed("apply_lost".into()))
    );
    // Loaded and not running (it ended without writing an end).
    sim.loaded
        .borrow_mut()
        .insert(crate::mac::layout::APPLY_LABEL.to_owned());
    assert_eq!(
        status::effective(&fx.host, &sim),
        Some(ApplyState::Failed("apply_lost".into()))
    );
    // Running.
    sim.apply_running.set(true);
    assert_eq!(status::effective(&fx.host, &sim), Some(ApplyState::Running));
    // launchctl cannot be asked.
    sim.apply_running.set(false);
    sim.break_("launchctl");
    assert_eq!(status::effective(&fx.host, &sim), Some(ApplyState::Running));
    // An end is reported as it is, without asking launchd.
    status::write(&fx.host, &ApplyState::Done).unwrap();
    assert_eq!(status::effective(&fx.host, &sim), Some(ApplyState::Done));
}

/// A job that is loaded and has not started yet is not gone: it is `running` until it has
/// an exit; a second install in that gap is `busy`.
#[test]
fn an_apply_job_that_has_not_started_yet_is_still_running() {
    let fx = Fx::new("mac-status-spawning");
    mkdir(&fx.host.work, 0o711);
    let sim = fx.sim();
    status::write(&fx.host, &ApplyState::Running).unwrap();
    sim.loaded
        .borrow_mut()
        .insert(crate::mac::layout::APPLY_LABEL.to_owned());
    sim.apply_spawning.set(true);
    assert_eq!(status::effective(&fx.host, &sim), Some(ApplyState::Running));
    // Ended: it has an exit code.
    sim.apply_spawning.set(false);
    assert_eq!(
        status::effective(&fx.host, &sim),
        Some(ApplyState::Failed("apply_lost".into()))
    );
}

/// What `launchctl print` says of a job that ended, and of one that did not start.
#[test]
fn launchctl_print_says_when_a_job_has_ended() {
    assert!(tools::print_finished(
        "\tstate = not running\n\tlast exit code = 0\n"
    ));
    assert!(tools::print_finished(
        "\tstate = not running\n\tlast exit code = 78\n"
    ));
    assert!(tools::print_finished(
        "\tstate = not running\n\tlast terminating signal = Killed: 9\n"
    ));
    assert!(!tools::print_finished(
        "\tstate = spawn scheduled\n\tlast exit code = (never exited)\n"
    ));
    assert!(!tools::print_finished("\tstate = not running\n"));
    assert!(!tools::print_finished(
        "\tstate = running\n\tlast exit code = 0\n"
    ));
}

// ---- the kit ----

#[test]
fn the_links_name_releases() {
    let fx = Fx::new("mac-links");
    assert_eq!(
        kit::links(&fx.host),
        Ok(Links {
            current: Some(OLD.into()),
            previous: None
        })
    );
    kit::set_link(&fx.host.previous(), Some(NEW)).unwrap();
    assert_eq!(
        fx.link(&fx.host.previous()).as_deref(),
        Some(format!("releases/{NEW}").as_str())
    );
    kit::set_link(&fx.host.previous(), Some(OLD)).unwrap();
    assert_eq!(
        fx.link(&fx.host.previous()).as_deref(),
        Some(format!("releases/{OLD}").as_str())
    );
    assert!(!fx.host.kit.join(".previous.new").exists());
    kit::set_link(&fx.host.previous(), None).unwrap();
    kit::set_link(&fx.host.previous(), None).unwrap();
    assert_eq!(kit::links(&fx.host).unwrap().previous, None);
}

/// A link that does not name `releases/<commit>` (a directory, an absolute link, another
/// shape) is not a state to build on.
#[test]
fn a_link_that_names_no_release_is_unreadable() {
    let fx = Fx::new("mac-links-bad");
    for target in [
        format!("/usr/local/kbf/releases/{OLD}"),
        "releases/short".to_owned(),
        format!("other/{OLD}"),
    ] {
        fs::remove_file(fx.host.current()).unwrap();
        symlink(&target, fx.host.current()).unwrap();
        assert!(
            kit::links(&fx.host)
                .unwrap_err()
                .contains("does not name a release"),
            "{target}"
        );
    }
    fs::remove_file(fx.host.current()).unwrap();
    mkdir(&fx.host.current(), 0o755);
    assert!(kit::links(&fx.host).is_err());
}

/// A leftover aside link, or a directory in its way, is dealt with: the former is replaced,
/// the latter is an error.
#[test]
fn a_link_is_replaced_over_a_leftover() {
    let fx = Fx::new("mac-links-aside");
    symlink("x", fx.host.kit.join(".current.new")).unwrap();
    kit::set_link(&fx.host.current(), Some(NEW)).unwrap();
    assert_eq!(
        fx.link(&fx.host.current()).as_deref(),
        Some(format!("releases/{NEW}").as_str())
    );
    mkdir(&fx.host.kit.join(".previous.new"), 0o755);
    write(&fx.host.kit.join(".previous.new/f"), 0o644, b"");
    assert!(kit::set_link(&fx.host.previous(), Some(NEW)).is_err());
    mkdir(&fx.host.kit.join("d"), 0o755);
    write(&fx.host.kit.join("d/f"), 0o644, b"");
    assert!(kit::set_link(&fx.host.kit.join("d"), None).is_err());
}

#[test]
fn the_installed_version_is_the_text_the_release_was_installed_as() {
    let fx = Fx::new("mac-version");
    assert_eq!(
        kit::installed_version(&fx.host, OLD).as_deref(),
        Some(OLD_VERSION)
    );
    assert_eq!(kit::installed_version(&fx.host, NEW), None);
    let file = fx.host.release(OLD).join("VERSION");
    for text in ["", "no newline", "not a version\n", "1.0\nmore\n", "\n"] {
        write(&file, 0o644, text.as_bytes());
        assert_eq!(kit::installed_version(&fx.host, OLD), None, "{text:?}");
    }
    write(&file, 0o666, format!("{NEW_VERSION}\n").as_bytes());
    assert_eq!(
        kit::installed_version(&fx.host, OLD),
        None,
        "a file others can write"
    );
}

/// The running commit is the release of the file the daemon's process executes, found
/// through its pid; every step that does not say gives none, and a path that is not a
/// release's binary is not a release.
#[test]
fn the_running_commit_is_the_release_the_daemon_process_executes() {
    let fx = Fx::new("mac-running");
    let sim = fx.sim();
    assert_eq!(kit::running_commit(&fx.host, &sim).as_deref(), Some(OLD));
    let releases = fx.host.releases();
    for (why, path) in [
        ("another tree", Some("/opt/kbf/kbf".into())),
        ("no program", None),
        ("a link-shaped name", Some(releases.join("current/kbf"))),
        ("an extra part", Some(releases.join(OLD).join("bin/kbf"))),
        ("a part after it", Some(releases.join(OLD).join("kbf/x"))),
        ("another file", Some(releases.join(OLD).join("kbf-daemon"))),
        ("the directory", Some(releases.join(OLD))),
        ("the releases directory", Some(releases.clone())),
    ] {
        *sim.running_path.borrow_mut() = path;
        assert_eq!(kit::running_commit(&fx.host, &sim), None, "{why}");
    }
    *sim.running_path.borrow_mut() = Some(releases.join(NEW).join("kbf"));
    assert_eq!(kit::running_commit(&fx.host, &sim).as_deref(), Some(NEW));
    sim.break_("lsof");
    assert_eq!(kit::running_commit(&fx.host, &sim), None);
    let sim = fx.sim();
    sim.break_("lsof-start");
    assert_eq!(
        kit::running_commit(&fx.host, &sim),
        None,
        "lsof cannot start"
    );
    // lsof exits 1 on a warning (a file system it cannot stat) with the listing whole.
    let sim = fx.sim();
    sim.break_("lsof-warns");
    assert_eq!(kit::running_commit(&fx.host, &sim).as_deref(), Some(OLD));
    let sim = fx.sim();
    sim.break_("no-pid");
    assert_eq!(kit::running_commit(&fx.host, &sim), None, "no process");
    let sim = fx.sim();
    sim.loaded.borrow_mut().clear();
    assert_eq!(kit::running_commit(&fx.host, &sim), None, "not loaded");
    sim.break_("launchctl");
    assert_eq!(
        kit::running_commit(&fx.host, &sim),
        None,
        "launchctl cannot start"
    );
}

// ---- the files of a release ----

/// A release file is named for its kind, platform and commit, its bundle is the same name with
/// `.sigstore.json`, and a name gives its commit back only for its own kind and platform.
#[test]
fn a_release_file_has_its_own_name_bundle_and_commit() {
    let commit = "0123456789abcdef0123456789abcdef01234567";
    let tarball = AssetKind::Tarball;
    let image = AssetKind::FrontendImage;
    assert_eq!(
        tarball.name_on("kbf", "linux-arm64", commit),
        format!("kbf-{commit}-linux-arm64.tar.gz")
    );
    assert_eq!(
        image.name_on("kbf", "linux-arm64", commit),
        format!("kbf-frontend-image-{commit}-linux-arm64.oci.tar")
    );
    assert_eq!(
        bundle_of(&image.name_on("kbf", "linux-x86_64", commit)),
        format!("kbf-frontend-image-{commit}-linux-x86_64.oci.tar.sigstore.json")
    );
    assert_eq!(
        bundle_of(&tarball.name_on("kbf", "linux-x86_64", commit)),
        format!("kbf-{commit}-linux-x86_64.tar.gz.sigstore.json")
    );

    assert_eq!(
        AssetKind::Deb.name_on("kbf", "linux-arm64", commit),
        format!("kbf-{commit}-linux-arm64.deb")
    );
    for kind in [tarball, image, AssetKind::Deb, AssetKind::Rpm] {
        let name = kind.name_on("kbf", "linux-arm64", commit);
        assert_eq!(
            kind.commit_of_name_on("kbf", "linux-arm64", &name),
            Some(commit)
        );
        assert_eq!(kind.commit_of_name_on("kbf", "linux-x86_64", &name), None);
        assert_eq!(
            kind.commit_of_name_on("kbf", "linux-arm64", &format!("{name}.sigstore.json")),
            None
        );
    }
    // One kind's name is not the other's.
    let image_name = image.name_on("kbf", "linux-arm64", commit);
    assert_eq!(
        tarball.commit_of_name_on("kbf", "linux-arm64", &image_name),
        None
    );
    let tarball_name = tarball.name_on("kbf", "linux-arm64", commit);
    assert_eq!(
        image.commit_of_name_on("kbf", "linux-arm64", &tarball_name),
        None
    );
    // A commit that is not 40 hex digits is none.
    assert_eq!(
        image.commit_of_name_on(
            "kbf",
            "linux-arm64",
            "kbf-frontend-image-abc-linux-arm64.oci.tar"
        ),
        None
    );
}

/// The sha256 a release's sums list for a file is found by the file's whole name; sums that are
/// not lines of `<64 hex>  <name>` (or name a file twice, or a path) are malformed, and a file
/// not listed is unknown.
#[test]
fn the_sums_give_the_sha256_listed_for_one_file() {
    let (a, b) = ("a".repeat(64), "b".repeat(64));
    let sums = format!("{a}  one.tar\n{b}  two.tar\n");
    assert_eq!(SUMS_NAME, "SHA256SUMS");
    assert_eq!(listed_hash(&sums, "two.tar").as_deref(), Ok(b.as_str()));
    assert_eq!(listed_hash(&sums, "one.tar").as_deref(), Ok(a.as_str()));
    assert_eq!(listed_hash(&sums, "two"), Err(PickError::Unknown));
    assert_eq!(listed_hash("", "one.tar"), Err(PickError::Unknown));
    for bad in [
        format!("{a}  one.tar"),
        format!("{a} one.tar\n"),
        format!("{}  one.tar\n", "a".repeat(63)),
        format!("{}  one.tar\n", "A".repeat(64)),
        format!("{a}  \n"),
        format!("{a}  dir/one.tar\n"),
        format!("{a}  one.tar\n{b}  one.tar\n"),
    ] {
        assert_eq!(
            listed_hash(&bad, "one.tar"),
            Err(PickError::Malformed),
            "{bad:?}"
        );
    }
}

/// A channel's artifact prefix names its files and is read back from them: the prefix is the
/// whole of the start of the name, the frontend image keeps its `-frontend-image-` word after
/// it, and the files of another prefix (or of `kbf`) are not this channel's.
#[test]
fn the_artifact_prefix_names_and_recognises_the_channels_files() {
    let commit = "0123456789abcdef0123456789abcdef01234567";
    let cases = [
        (AssetKind::Tarball, "other-{c}-linux-arm64.tar.gz"),
        (AssetKind::Deb, "other-{c}-linux-arm64.deb"),
        (AssetKind::Rpm, "other-{c}-linux-arm64.rpm"),
        (
            AssetKind::FrontendImage,
            "other-frontend-image-{c}-linux-arm64.oci.tar",
        ),
    ];
    for (kind, pattern) in cases {
        let name = pattern.replace("{c}", commit);
        assert_eq!(kind.name_on("other", "linux-arm64", commit), name);
        assert_eq!(
            kind.commit_of_name_on("other", "linux-arm64", &name),
            Some(commit)
        );
        assert_eq!(kind.commit_of_name_on("kbf", "linux-arm64", &name), None);
        assert_eq!(kind.commit_of_name_on("oth", "linux-arm64", &name), None);
        let kbfs = kind.name_on("kbf", "linux-arm64", commit);
        assert!(kbfs.starts_with("kbf-"), "{kbfs}");
        assert_eq!(kind.commit_of_name_on("other", "linux-arm64", &kbfs), None);
    }
    // The default is the name of today.
    assert_eq!(
        crate::mac::release::named_asset_on("kbf", "darwin-arm64", "tar.gz", commit),
        tarball_name(commit)
    );
}
