//! The `approve` subcommand.

mod support;

use std::process::ExitCode;

use support::{Scratch, my_uid};
use update_rights::cli::{Env, main, run};
use update_rights::{Approvals, Digest};

fn strings(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| (*w).to_owned()).collect()
}

fn run_in(env: &Env, words: &[&str]) -> (u8, String, String) {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run(&strings(words), env, &mut out, &mut err);
    (
        code,
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

fn env_in(scratch: &Scratch) -> Env {
    Env {
        approvals: scratch.path().join("approved"),
        trusted_uid: my_uid(),
        effective_uid: my_uid(),
    }
}

/// Catches an approval that prints nothing (the line is the audit record) or records the wrong
/// thing: it says what it did, and the digest is in the file.
#[test]
fn approve_records_and_says_so() {
    let scratch = Scratch::new("cli-approve");
    let env = env_in(&scratch);
    let digest = Digest::of(b"a declaration");
    let (code, out, err) = run_in(&env, &["tierstore", digest.as_str()]);
    assert_eq!(
        (code, out.as_str(), err.as_str()),
        (0, format!("approved tierstore {digest}\n").as_str(), "")
    );
    assert!(
        Approvals::read(&env.approvals, my_uid())
            .unwrap()
            .contains(&digest)
    );
    let (code, out, _) = run_in(&env, &["tierstore", digest.as_str()]);
    assert_eq!(
        (code, out.as_str()),
        (0, format!("already approved tierstore {digest}\n").as_str())
    );
}

/// Catches a wrong command line being acted on: no arguments, one, three, a digest that is not
/// one. Exit 2, the usage on standard error, nothing written.
#[test]
fn a_wrong_command_line_is_a_usage_error() {
    let scratch = Scratch::new("cli-usage");
    let env = env_in(&scratch);
    let digest = Digest::of(b"a");
    for words in [
        &[][..],
        &["tierstore"],
        &["tierstore", digest.as_str(), "extra"],
        &["tierstore", "not-a-digest"],
        &["tierstore", &digest.as_str().to_uppercase()],
    ] {
        let (code, out, err) = run_in(&env, words);
        assert_eq!((code, out.as_str()), (2, ""), "{words:?}");
        assert!(err.contains("usage: approve <service>"), "{words:?}: {err}");
    }
    assert!(!env.approvals.exists());
}

/// Catches a refusal that exits 0: a service that is not one, or an account that may not
/// approve, exits 1 and says why.
#[test]
fn a_refused_approval_exits_one_and_says_why() {
    let scratch = Scratch::new("cli-refused");
    let mut env = env_in(&scratch);
    let digest = Digest::of(b"a");
    let (code, out, err) = run_in(&env, &["Not A Service", digest.as_str()]);
    assert_eq!((code, out.as_str()), (1, ""));
    assert!(err.starts_with("not approved: "), "{err}");
    env.effective_uid = my_uid() + 1;
    let (code, _, err) = run_in(&env, &["tierstore", digest.as_str()]);
    assert_eq!(code, 1);
    assert!(err.contains("owns the approval file"), "{err}");
    assert!(!env.approvals.exists());
}

/// Catches the production environment pointing anywhere but the host's record, or trusting
/// anyone but root.
#[test]
fn the_production_environment_is_the_hosts_record_owned_by_root() {
    let env = Env::production();
    assert_eq!(
        env.approvals,
        std::path::Path::new("/etc/updater.d/approved")
    );
    assert_eq!(env.trusted_uid, 0);
    assert_eq!(env.effective_uid, my_uid());
}

/// Catches the entry point the root part calls dropping the exit code: a usage error is 2.
#[test]
fn main_returns_the_exit_code() {
    assert_eq!(
        format!("{:?}", main(&[])),
        format!("{:?}", ExitCode::from(2))
    );
}
