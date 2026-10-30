//! The flags that carry the v2 key set: how they parse, what they conflict with, the argv
//! refusal, and which URL the run takes from the set today.

use clap::CommandFactory;
use kbf_segments::sha256;
use kbf_testkit::dir::TestDir;

use super::*;

const SET: &str = r#"{"packs":["https://store.example/p0?X-Sig=very-secret","https://store.example/p1?X-Sig=other"],"large":["https://store.example/l0?X-Sig=third"]}"#;

fn scratch(name: &str) -> TestDir {
    TestDir::under(std::env::temp_dir(), &format!("kbf-put-urls-flag-{name}"))
}

/// The flags with `args` added to the grant, the lease and the action.
fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
    let action = format!("--action={}", sha256(b"a"));
    let mut all = vec![
        "kbf-task",
        "--grant-url=https://grant.example",
        "--grant-token-file=t",
        "--lease=l",
        action.as_str(),
    ];
    all.extend_from_slice(args);
    Cli::try_parse_from(all)
}

/// Catches either form of the key set being refused on its own, and a run being startable with
/// none of the four sources.
#[test]
fn each_form_of_the_key_set_is_a_pack_url_source() {
    parse(&["--put-urls-file=keys.json"]).expect("file");
    parse(&["--put-urls=x", "--insecure-argv-secrets"]).expect("flag");
    assert!(parse(&[]).is_err());
}

/// Catches the key set being accepted together with a v1 pack URL (which of the two the run
/// used would be a guess), in any pairing.
#[test]
fn the_key_set_conflicts_with_the_pack_url_and_with_itself() {
    let forms = [
        "--pack-url-file=u",
        "--pack-url=https://s/p",
        "--put-urls-file=k",
        "--put-urls=x",
    ];
    for (i, one) in forms.iter().enumerate() {
        for other in &forms[i + 1..] {
            assert!(parse(&[one, other]).is_err(), "{one} with {other}");
        }
    }
}

/// Catches the v1 flags no longer working on their own.
#[test]
fn the_v1_flags_still_parse_alone() {
    parse(&["--pack-url-file=u"]).expect("file");
    parse(&["--pack-url=https://s/p"]).expect("flag");
}

/// Catches `--put-urls` being accepted without the opt-in, or its refusal naming the value:
/// the message names the flag and the opt-in, never the URLs, and `connect` fails with a setup
/// failure before anything is read.
#[test]
fn put_urls_needs_the_insecure_opt_in_and_the_refusal_hides_the_value() {
    let flags = parse(&[&format!("--put-urls={SET}")]).expect("clap accepts it");
    let Err(failure) = check_argv_secrets(&flags) else {
        panic!("accepted without --insecure-argv-secrets");
    };
    assert_eq!(failure.exit_code(), EXIT_SETUP);
    let text = failure.to_string();
    assert!(text.contains("--put-urls"), "{text}");
    assert!(text.contains("--insecure-argv-secrets"), "{text}");
    assert!(!text.contains("very-secret"), "{text}");
    let Err(connect_failure) = connect(&flags) else {
        panic!("connected");
    };
    assert_eq!(connect_failure.exit_code(), EXIT_SETUP);
    assert!(!connect_failure.to_string().contains("very-secret"));

    let allowed = parse(&[&format!("--put-urls={SET}"), "--insecure-argv-secrets"]).expect("flags");
    check_argv_secrets(&allowed).expect("accepted with the opt-in");
}

#[tokio::test]
async fn main_with_refuses_put_urls_without_the_opt_in() {
    let flags = parse(&[&format!("--put-urls={SET}")]).expect("flags");
    assert_eq!(main_with(&flags).await, ExitCode::from(EXIT_SETUP));
}

/// Catches the run taking another slot than the first, or the file being consumed: with a key
/// set from the file or from the flag, the pack URL is `packs[0]`, and the file stays.
#[test]
fn today_the_run_uses_the_first_pack_slot() {
    let dir = scratch("first");
    let file = dir.join("keys.json");
    std::fs::write(&file, SET).expect("write");
    let flags = parse(&[&format!("--put-urls-file={}", file.display())]).expect("flags");
    assert_eq!(
        pack_url(&flags).expect("url"),
        "https://store.example/p0?X-Sig=very-secret"
    );
    assert!(file.exists());

    let flag = parse(&[&format!("--put-urls={SET}"), "--insecure-argv-secrets"]).expect("flags");
    assert_eq!(
        pack_url(&flag).expect("url"),
        "https://store.example/p0?X-Sig=very-secret"
    );
}

/// Catches the v1 pack URL being ignored when no key set is given.
#[test]
fn without_a_key_set_the_run_uses_the_v1_pack_url() {
    let dir = scratch("v1");
    let file = dir.join("url");
    std::fs::write(&file, "https://store.example/v1?X-Sig=s\n").expect("write");
    let flags = parse(&[&format!("--pack-url-file={}", file.display())]).expect("flags");
    assert_eq!(
        pack_url(&flags).expect("url"),
        "https://store.example/v1?X-Sig=s"
    );
}

/// Catches a bad key set starting a run, and its refusal carrying the input: a set with no
/// pack, a missing file and a URL that is not http(s) are setup failures that do not print the
/// secret.
#[test]
fn a_bad_key_set_is_a_setup_failure_without_the_value() {
    let dir = scratch("bad");
    let missing = dir.join("absent.json");
    let flags = parse(&[&format!("--put-urls-file={}", missing.display())]).expect("flags");
    let failure = pack_url(&flags).unwrap_err();
    assert_eq!(failure.exit_code(), EXIT_SETUP);
    assert!(failure.to_string().contains("absent.json"), "{failure}");

    for bad in [
        r#"{"packs":[],"large":[]}"#,
        r#"{"packs":["ftp://very-secret"]}"#,
    ] {
        let flags =
            parse(&[&format!("--put-urls={bad}"), "--insecure-argv-secrets"]).expect("flags");
        let failure = pack_url(&flags).unwrap_err();
        assert_eq!(failure.exit_code(), EXIT_SETUP);
        let text = failure.to_string();
        assert!(text.contains("key set"), "{text}");
        assert!(!text.contains("very-secret"), "{text}");
    }
}

/// Catches the help hiding what is and is not used: both flags are listed, and say that today
/// only the first pack slot is used.
#[test]
fn the_help_says_only_the_first_pack_slot_is_used() {
    let help = Cli::command().render_long_help().to_string();
    assert!(help.contains("--put-urls-file"), "{help}");
    assert!(help.contains("--put-urls "), "{help}");
    assert!(
        help.contains("v2 key set: today only the first pack slot is used"),
        "{help}"
    );
}
