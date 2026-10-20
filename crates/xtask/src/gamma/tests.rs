//! Unit tests of `cargo xtask gamma`. The verbs that read GitHub or run `tailscale` are
//! tested as the workflow runs them in `tests/gamma.rs`.

use kbf_installer::deploy::cli::HOST_FLAGS;
use kbf_testkit::dir::TestDir;

use super::*;

const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
const OTHER: &str = "badbad0123456789badbad0123456789badbad01";
const REPO: &str = "komira-ai/kbf";
const HOST: &str = "farm-front";
const USER: &str = "kbf-deploy";
const TAG: &str = "tag:kbf-gamma-ci";

/// An empty directory of its own for one test, removed when it drops.
fn scratch(name: &str) -> TestDir {
    let beside = std::env::current_exe().expect("test binary");
    let deps = beside.parent().expect("deps");
    TestDir::under(deps.join("xtask-gamma-unit"), name)
}

/// The message of a usage error; any other answer fails the test.
fn usage<T: std::fmt::Debug>(answer: Result<T, Error>) -> String {
    match answer {
        Err(Error::Usage(message)) => message,
        other => panic!("not a usage error: {other:?}"),
    }
}

/// Asserts that `text` holds `needle`.
fn has(text: &str, needle: &str) {
    assert!(text.contains(needle), "{needle:?} is not in {text}");
}

/// Asserts that `text` does not hold `needle`.
fn lacks(text: &str, needle: &str) {
    assert!(!text.contains(needle), "{needle:?} is in {text}");
}

/// Whether sudoers runs `argv` for `pattern`: word for word, the commit's forty classes
/// matching one lowercase hex digit each, and no other word a wildcard.
fn allowed(pattern: &str, argv: &[String]) -> bool {
    let classes = "[0-9a-f]".repeat(40);
    let words: Vec<&str> = pattern.split(' ').collect();
    let lower_hex = |arg: &String| {
        arg.len() == 40 && arg.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    };
    words.len() == argv.len()
        && words.iter().zip(argv).all(|(word, arg)| {
            if *word == classes {
                lower_hex(arg)
            } else {
                word == arg
            }
        })
}

/// Proves the command the workflow sends is `sudo -n` and the installer's one deploy command,
/// with the commit once, and that it reads the offer and the files from standard input.
/// Catches a flag dropped from or added to the command: the string is written out here.
#[test]
fn the_ssh_command_is_the_one_deploy_command_through_sudo() {
    let args = ssh_args(SHA, "host", "user");
    assert_eq!(args[..4], ["ssh", "user@host", "sudo", "-n"]);
    let tail = format!("--commit {SHA} --offer - --bundle -");
    let want = format!("{INSTALLER} deploy --apply --json {tail}");
    assert_eq!(args[4..].join(" "), want);
}

/// Proves the sudoers pattern matches the command the workflow sends for a real commit, and
/// no other: an inserted flag, a flag dropped, a commit of 39 or 41 digits, a non-hex one and
/// an upper-case one are not allowed. Catches a wildcard in the rule, or the rule and the
/// command drifting apart (a flag dropped from one).
#[test]
fn the_sudoers_pattern_allows_the_workflows_command_and_no_other() {
    let pattern = sudoers_command();
    let sent = |sha: &str| ssh_args(sha, "h", "u")[4..].to_vec();
    assert!(allowed(&pattern, &sent(SHA)));
    assert!(!allowed(&pattern, &sent(&SHA[..39])));
    assert!(!allowed(&pattern, &sent(&format!("{SHA}0"))));
    assert!(!allowed(&pattern, &sent(&SHA.to_uppercase())));
    let junk = "z".repeat(40);
    assert!(!allowed(&pattern, &sent(&junk)));
    let mut more = sent(SHA);
    more.insert(2, "--api".to_owned());
    assert!(!allowed(&pattern, &more));
    let mut fewer = sent(SHA);
    fewer.retain(|word| word != "--offer");
    assert!(!allowed(&pattern, &fewer));
    lacks(&pattern, "*");
}

/// Proves the line for an account: the rule, the installer's path, the arguments in order and
/// forty classes for the commit. A root account and a malformed name are usage errors.
#[test]
fn the_sudoers_line_names_the_account_and_the_whole_command() {
    let line = sudoers_line(USER).unwrap();
    let mut want = String::from("kbf-deploy ALL=(root) NOPASSWD: ");
    want.push_str("/usr/bin/kbf-installer deploy --apply --json --commit ");
    want.push_str(&"[0-9a-f]".repeat(40));
    want.push_str(" --offer - --bundle -");
    assert_eq!(line, want);
    assert_eq!(line.matches("[0-9a-f]").count(), 40);
    has(&usage(sudoers_line("root")), "--user");
    has(&usage(sudoers_line("Kbf Deploy")), "--user");
}

/// Proves no flag the workflow sends is one the installer refuses through sudo (its host
/// flags): the deploy account would be refused its own command. Catches `--stage` or `--api`
/// added to the command.
#[test]
fn no_flag_of_the_command_is_a_host_flag_of_the_installer() {
    for arg in COMMAND {
        assert!(!HOST_FLAGS.contains(&arg), "{arg} is a host flag");
    }
}

#[test]
fn an_account_is_a_lowercase_name_and_never_root() {
    // Catches: a root account (the installer does not narrow its flags for root), a name
    // that starts with a digit, a dash or a capital, or with a character a name has none of;
    // and a message that repeats the name.
    for ok in ["kbf-deploy", "_x", "a1_b-c"] {
        assert!(account(ok).is_ok(), "{ok}");
    }
    for bad in ["", "Root", "1a", "-a", "a b", "a.b", "root"] {
        has(&usage(account(bad)), "--user");
    }
    lacks(&usage(account("canary name")), "canary");
}

#[test]
fn a_host_is_a_name_and_not_an_address() {
    // Catches: an address (the repository's text lints and the log both show it), an empty
    // name, a character a host name has none of, and a message that repeats the name.
    for ok in ["farm-front", "farm.tail-net.example", "h_1", "a1"] {
        assert!(host_name(ok).is_ok(), "{ok}");
    }
    for bad in ["", "a b", "a:b", "198.51.100.7", "12345", "1.2.3"] {
        has(&usage(host_name(bad)), "--host");
    }
    lacks(&usage(host_name("canary name")), "canary");
}

#[test]
fn a_repository_is_owner_and_name() {
    for ok in ["komira-ai/kbf", "a/b.c", "a_b/c-d"] {
        assert!(repo_name(ok).is_ok(), "{ok}");
    }
    for bad in ["", "a", "/b", "a/", "a/b/c", "a b/c", "a/b c"] {
        has(&usage(repo_name(bad)), "--repo");
    }
}

#[test]
fn a_tag_is_tag_colon_and_a_lowercase_name() {
    for ok in ["tag:kbf-gamma-ci", "tag:a1"] {
        assert!(tag_name(ok).is_ok(), "{ok}");
    }
    for bad in ["", "tag:", "kbf", "tag:Upper", "tag:a_b", "TAG:a"] {
        has(&usage(tag_name(bad)), "--tag");
    }
}

/// A run that matches what the event said.
fn run() -> Value {
    json!({
        "path": RELEASE_PATH,
        "name": "release",
        "event": "push",
        "head_branch": "main",
        "head_sha": SHA,
        "status": "completed",
        "conclusion": "success",
    })
}

/// Proves a run of the release workflow's file, named `release`, a push to main at this
/// commit, completed and successful, contradicts nothing, with the path as GitHub writes it
/// for a run at a ref too.
#[test]
fn a_run_that_matches_the_event_contradicts_nothing() {
    assert_eq!(contradictions(&run(), SHA), Vec::<String>::new());
    let mut at_ref = run();
    at_ref["path"] = json!(format!("{RELEASE_PATH}@refs/heads/main"));
    assert_eq!(contradictions(&at_ref, SHA), Vec::<String>::new());
}

/// Catches: a run of another file or workflow, another event, branch or commit, one still
/// going or one that did not succeed passing as the run the event named. Each fact alone is
/// named; a run that says nothing contradicts all seven.
#[test]
fn each_fact_that_differs_is_named() {
    let wrong = [
        ("path", ".github/workflows/other.yml"),
        ("name", "other"),
        ("event", "workflow_dispatch"),
        ("head_branch", "feature"),
        ("head_sha", OTHER),
        ("status", "in_progress"),
        ("conclusion", "failure"),
    ];
    for (key, value) in wrong {
        let mut changed = run();
        changed[key] = json!(value);
        let found = contradictions(&changed, SHA);
        assert_eq!(found.len(), 1, "{key}: {found:?}");
        has(&found[0], key);
    }
    assert_eq!(contradictions(&json!({}), SHA).len(), 7);
}

#[test]
fn only_short_lowercase_words_pass() {
    let reason = json!("node_not_confirmed");
    assert_eq!(word(&reason), reason);
    assert_eq!(word(&json!("linux-x86_64")), json!("linux-x86_64"));
    assert_eq!(word(&json!("a".repeat(64))), json!("a".repeat(64)));
    let dropped = [
        json!(""),
        json!("Upper"),
        json!("has space"),
        json!("a".repeat(65)),
        json!(7),
        json!(null),
        json!(["a"]),
    ];
    for bad in dropped {
        assert_eq!(word(&bad), Value::Null, "{bad}");
    }
}

/// The installer's summary as a failed deploy prints it, with `canary` words where it names
/// a node, a free text, a field this code does not know or a reason of a step.
fn raw() -> String {
    json!({
        "schema": 2,
        "commit": SHA,
        "previous_desired": { "commit": OTHER, "generation": 4 },
        "generation": 5,
        "result": "failed",
        "exit": 1,
        "reason": "node_not_confirmed",
        "started": "2026-10-20T01:02:03Z",
        "detail": "canary-detail: canary-node did not confirm",
        "steps": [
            { "name": "bundle", "ok": true, "ms": 3 },
            { "name": "nodes", "ok": false, "ms": 9, "reason": "canary-step-reason" },
        ],
        "platforms": { "canary-platform-key": "complete" },
        "nodes": [{
            "id": "canary-node",
            "platform": "linux-x86_64",
            "state": "timed_out",
            "running_commit": SHA,
            "secs": 1800,
            "refusal": "canary-refusal",
        }],
        "frontend": { "update_id": "canary-update", "phase": "done" },
        "post": {
            "members_active": true,
            "nodes_on_commit": false,
            "readyz": null,
            "cas_smoke": "skipped",
        },
        "offer": {
            "kind": "offer",
            "set_hash": "canary-hash",
            "reversible": true,
            "host_reversible": true,
            "meta_log": 3,
            "previous_meta_log": 2,
            "canary-field": "x",
        },
        "canary-unknown": { "canary": "x" },
    })
    .to_string()
}

/// Proves the redacted summary holds the allow-listed facts and no more: no node id, no free
/// text, no refusal, no step reason and no field this code does not know, whatever they say.
/// Catches a redaction that removes the fields it knows are sensitive and keeps the rest.
#[test]
fn redact_keeps_the_allow_list_and_nothing_else() {
    let text = redact(&raw());
    lacks(&text, "canary");
    let kept: Value = serde_json::from_str(&text).expect("redacted JSON");
    let want = json!({
        "commit": SHA,
        "result": "failed",
        "exit": 1,
        "reason": "node_not_confirmed",
        "steps": [{ "name": "bundle", "ok": true }, { "name": "nodes", "ok": false }],
        "nodes": [{ "platform": "linux-x86_64", "state": "timed_out", "secs": 1800 }],
        "post": {
            "members_active": true,
            "nodes_on_commit": false,
            "readyz": null,
            "cas_smoke": "skipped",
        },
        "offer": {
            "reversible": true,
            "host_reversible": true,
            "meta_log": 3,
            "previous_meta_log": 2,
        },
    });
    assert_eq!(kept, want);
}

/// Catches: a summary that is not a document, or has none of the fields, turning into
/// something a table would show as fact. The first has no summary; the second has fields
/// that are all null.
#[test]
fn redact_gives_nothing_for_text_that_is_not_a_summary() {
    assert_eq!(redact("not json"), "");
    assert_eq!(redact(""), "");
    let empty: Value = serde_json::from_str(&redact("{}")).expect("redacted JSON");
    assert_eq!(empty["result"], Value::Null);
    assert_eq!(empty["steps"], json!([]));
    assert_eq!(empty["nodes"], json!([]));
    assert_eq!(empty["post"]["readyz"], Value::Null);
}

/// Proves the table shows the platform, state and seconds of each node, the result, the
/// reason and the steps, and no node id. A cell that is not there is a question mark.
#[test]
fn the_table_names_no_node() {
    let md = table(&redact(&raw()));
    has(&md, "| linux-x86_64 | timed_out | 1800 |");
    has(&md, "result: failed (exit 1) node_not_confirmed");
    has(&md, "steps: bundle=ok nodes=FAILED");
    lacks(&md, "canary");
    has(&table("not json"), "no summary was printed");
    has(&table(""), "no summary was printed");
    has(&table("{}"), "result: ? (exit ?)\n");
    let md = table(r#"{"nodes":[{"platform":"p","state":"s"}]}"#);
    has(&md, "| p | s | ? |");
    // A deploy that succeeded has no reason code: the line says so by saying nothing.
    let ok = table(&redact(r#"{"result":"ok","exit":0,"reason":""}"#));
    has(&ok, "result: ok (exit 0)\n\n");
}

#[test]
fn summary_appends_and_survives_a_missing_file() {
    let dir = scratch("summary");
    let out = dir.join("step-summary.md");
    fs::write(&out, "earlier\n").unwrap();
    summary(&dir.join("absent.json"), &out).unwrap();
    let text = fs::read_to_string(&out).unwrap();
    assert!(text.starts_with("earlier\n"));
    has(&text, "no summary was printed");
    assert!(summary(&dir.join("absent.json"), &dir.join("no/dir/out")).is_err());
}

/// Proves the bundle reaches the program's standard input and its standard output is the
/// raw file, that its exit status is returned, and that what it writes to its standard error
/// is counted and not kept (`cat` of a file that is not there writes a message and exits 1).
/// `cat` plays `tailscale`.
#[test]
fn pipe_sends_the_bundle_in_and_keeps_the_output_and_the_status() {
    let dir = scratch("pipe");
    let (bundle, raw) = (dir.join("b.tar"), dir.join("raw"));
    fs::write(&bundle, "the bundle").unwrap();
    let ran = |program: &str| pipe(Path::new(program), &[], &bundle, &raw).unwrap();
    assert_eq!(ran("cat"), (0, 0));
    assert_eq!(fs::read_to_string(&raw).unwrap(), "the bundle");
    assert_eq!(ran("false"), (1, 0));
    let absent = [dir.join("absent").display().to_string()];
    let (status, unseen) = pipe(Path::new("cat"), &absent, &bundle, &raw).unwrap();
    assert_eq!(status, 1);
    assert!(unseen > 0);
    assert_eq!(fs::read_to_string(&raw).unwrap(), "");
    let missing = Path::new("no-such-program-for-gamma");
    assert!(pipe(missing, &[], &bundle, &raw).is_err());
    assert!(pipe(Path::new("cat"), &[], &dir.join("absent"), &raw).is_err());
    assert!(pipe(Path::new("cat"), &[], &bundle, &dir.join("no/dir/raw")).is_err());
}

/// Catches: a name that is not checked before it is printed into a command a person pastes,
/// and a placeholder left in the output.
#[test]
fn held_steps_check_every_name_and_fill_every_placeholder() {
    let lines = held_steps(REPO, HOST, USER, TAG).unwrap();
    let placeholders = [
        "{repo}",
        "{host}",
        "{user}",
        "{tag}",
        "{host_tag}",
        "{sudoers}",
    ];
    for placeholder in placeholders {
        let left = lines.iter().any(|l| l.contains(placeholder));
        assert!(!left, "{placeholder}");
    }
    assert!(lines.contains(&sudoers_line(USER).unwrap()));
    let refused =
        |repo: &str, host: &str, user: &str, tag: &str| usage(held_steps(repo, host, user, tag));
    has(&refused("kbf", HOST, USER, TAG), "--repo");
    has(&refused(REPO, "198.51.100.7", USER, TAG), "--host");
    has(&refused(REPO, HOST, "root", TAG), "--user");
    has(&refused(REPO, HOST, USER, "x"), "--tag");
    has(&refused(REPO, HOST, USER, HOST_TAG), "--tag");
}

/// Catches: the SSH rule's destination written as the device name (Tailscale accepts a tag,
/// the user's own devices or `autogroup:self` there, so the printed policy would be refused),
/// and the host not told to carry the tag the rules name.
#[test]
fn the_policy_names_the_host_by_its_tag_and_says_to_tag_it() {
    let text = held_steps(REPO, HOST, USER, TAG).unwrap().join("\n");
    has(&text, &format!("\"dst\": [\"{HOST_TAG}\"]"));
    lacks(&text, &format!("\"dst\": [\"{HOST}\"]"));
    has(&text, &format!("\"{HOST_TAG}\": [\"autogroup:admin\"]"));
    has(&text, &format!("tailscale set --advertise-tags={HOST_TAG}"));
}
