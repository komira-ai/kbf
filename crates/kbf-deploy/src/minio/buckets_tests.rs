use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::rc::Rc;

use super::*;
use kbf_host::plan::{Mode, execute};
use kbf_host::step::Step;
use kbf_host::testkit::{FakeExec, reply};

const USER: &str = "kbf-admin";
const PASSWORD: &str = "root:pass@word/1";
const MINTED_ID: &str = "KBFMINTEDKEY";
const MINTED_SECRET: &str = "minted/secret+key";
const MC: &str = "/opt/kbf/bin/mc";
const CONFIG_DIR: &str = "/srv/scratch/mc";
const KEY_FILE: &str = "/etc/kbf/s3.env";
const SERVER: Owner = Owner { uid: 998, gid: 997 };

fn p(s: &str) -> HostPath {
    HostPath::new(s).expect("valid test path")
}

fn name(s: &str) -> KbfName {
    KbfName::new(s).expect("valid test name")
}

fn admin() -> Admin {
    Admin {
        user: USER.to_owned(),
        password: PASSWORD.to_owned(),
    }
}

/// The kbf-linux kit's run: two buckets, a key file owned by `kbf-server`.
fn args() -> Buckets {
    Buckets {
        endpoint: HttpUrl::new("http://127.0.0.1:9000").expect("url"),
        buckets: vec![name("kbf-cas"), name("kbf-cas-2")],
        policy: name("kbf-linux-rw"),
        account: name("kbf-linux-server"),
        key_file: Some(p(KEY_FILE)),
        key_owner: Some("kbf-server".to_owned()),
        mc: p(MC),
        mc_config_dir: CONFIG_DIR.to_owned(),
        remove: false,
    }
}

/// What the fake MinIO holds.
#[derive(Default)]
struct Minio {
    buckets: BTreeSet<String>,
    /// Each policy as `mc admin policy info --json` prints it back.
    policies: BTreeMap<String, serde_json::Value>,
    /// Service accounts: access key id to account name.
    accounts: BTreeMap<String, String>,
    /// `mc --version` names no RELEASE, as GNU Midnight Commander's does not.
    not_mc: bool,
}

fn env_of<'a>(cmd: &'a Cmd, key: &str) -> Option<&'a str> {
    cmd.env
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

/// The policy as MinIO stores and prints it: the same statements, each list in
/// another order, with an empty `Sid`.
fn as_minio_prints(policy: &[u8]) -> serde_json::Value {
    let mut v: serde_json::Value = serde_json::from_slice(policy).expect("the plan sends JSON");
    for statement in v["Statement"].as_array_mut().expect("statements") {
        for list in ["Action", "Resource"] {
            statement[list].as_array_mut().expect("a list").reverse();
        }
        statement["Sid"] = "".into();
    }
    v
}

/// The bucket in an `mc` target, `<alias>/<bucket>`.
fn bucket(target: &str) -> &str {
    target.split_once('/').map_or(target, |(_, b)| b)
}

/// `mc` and `id` against `state`. `mc` answers only with the plan's config directory
/// and, in `MC_HOST_<alias>`, the admin's URL (alias `kbf`) or the minted key's
/// (alias `kbfkey`, while its account exists).
fn answer(state: Rc<RefCell<Minio>>) -> kbf_host::testkit::Answer {
    Box::new(move |cmd: &Cmd| {
        let mut m = state.borrow_mut();
        let args: Vec<&str> = cmd.args.iter().map(String::as_str).collect();
        if cmd.program.as_str() == "/usr/bin/id" {
            return Ok(match args[..] {
                ["-u", "--", "kbf-server"] => reply(0, "998\n", ""),
                ["-g", "--", "kbf-server"] => reply(0, "997\n", ""),
                _ => reply(1, "", "id: no such user\n"),
            });
        }
        if cmd.program.as_str() != MC {
            return Err(io::Error::from(io::ErrorKind::NotFound));
        }
        if args == ["--version"] {
            let version = if m.not_mc {
                "GNU Midnight Commander 4.8.30\n"
            } else {
                "mc version RELEASE.2025-08-13T08-35-41Z (commit-id=x)\n"
            };
            return Ok(reply(0, version, ""));
        }
        let as_key = cmd.args.iter().any(|a| a.starts_with("kbfkey/"));
        let (var, valid) = if as_key {
            let url = format!(
                "http://{MINTED_ID}:{}@127.0.0.1:9000",
                url_encode(MINTED_SECRET)
            );
            (
                "MC_HOST_kbfkey",
                m.accounts.contains_key(MINTED_ID).then_some(url),
            )
        } else {
            let url = format!("http://{USER}:{}@127.0.0.1:9000", url_encode(PASSWORD));
            ("MC_HOST_kbf", Some(url))
        };
        let given = (env_of(cmd, var), env_of(cmd, "MC_CONFIG_DIR"));
        if given != (valid.as_deref(), Some(CONFIG_DIR)) {
            return Ok(reply(1, "", "mc: <ERROR> Access Denied.\n"));
        }
        let done = Ok(reply(0, "", ""));
        let missing = Ok(reply(
            1,
            "{\"status\":\"error\"}",
            "mc: <ERROR> not found\n",
        ));
        match args[..] {
            ["ls", target] => {
                if m.buckets.contains(bucket(target)) {
                    done
                } else {
                    missing
                }
            }
            ["mb", target] => {
                m.buckets.insert(bucket(target).to_owned());
                done
            }
            ["rb", "--force", target] => {
                m.buckets.remove(bucket(target));
                done
            }
            ["admin", "policy", "info", "kbf", policy, ..] => match m.policies.get(policy) {
                Some(doc) => {
                    let info = serde_json::json!({
                        "status": "success",
                        "policy": policy,
                        "policyInfo": {"PolicyName": policy, "Policy": doc},
                    });
                    Ok(reply(0, &info.to_string(), ""))
                }
                None => missing,
            },
            ["admin", "policy", "create", "kbf", policy, "/dev/stdin"] => {
                let doc = as_minio_prints(&cmd.stdin);
                m.policies.insert(policy.to_owned(), doc);
                done
            }
            ["admin", "policy", "remove", "kbf", policy] => {
                m.policies.remove(policy);
                done
            }
            [
                "admin",
                "user",
                "svcacct",
                "add",
                "--json",
                "--policy",
                "/dev/stdin",
                "--name",
                account,
                "--description",
                _,
                "kbf",
                USER,
            ] => {
                as_minio_prints(&cmd.stdin);
                m.accounts.insert(MINTED_ID.to_owned(), account.to_owned());
                let minted = format!(
                    "{{\"status\":\"success\",\"accessKey\":\"{MINTED_ID}\",\
                     \"secretKey\":\"{MINTED_SECRET}\",\"accountStatus\":\"on\"}}"
                );
                Ok(reply(0, &minted, ""))
            }
            ["admin", "user", "svcacct", "rm", "kbf", id] => {
                m.accounts.remove(id);
                done
            }
            _ => Ok(reply(2, "", "unexpected mc command\n")),
        }
    })
}

/// Catches a fake MinIO that answers a command it does not know, or a key it was not
/// given, as a success: a typo in the plan's argv, or a key that never reached `mc`,
/// would then pass every test here.
#[test]
fn the_fake_minio_refuses_what_it_does_not_know() {
    let (exec, _state) = host(0);
    let mc = |a: &str| Cmd::new(p(MC)).arg(a).env("MC_CONFIG_DIR", CONFIG_DIR);
    let admin_url = format!("http://{USER}:{}@127.0.0.1:9000", url_encode(PASSWORD));
    let typo = mc("lss").env("MC_HOST_kbf", admin_url);
    let out = (exec.answer)(&typo).expect("answered");
    assert_eq!(out.status.code(), Some(2));
    let no_key = (exec.answer)(&mc("ls").arg("kbf/kbf-cas")).expect("answered");
    assert_eq!(
        String::from_utf8_lossy(&no_key.stderr),
        "mc: <ERROR> Access Denied.\n"
    );
}

/// A fake host as `euid`, over a fresh fake MinIO.
fn host(euid: u32) -> (FakeExec, Rc<RefCell<Minio>>) {
    let state = Rc::new(RefCell::new(Minio::default()));
    let mut exec = FakeExec::new(euid);
    exec.answer = answer(Rc::clone(&state));
    (exec, state)
}

/// Plan `args`, then print or apply the plan; everything printed.
fn run(exec: &mut FakeExec, args: &Buckets, mode: Mode) -> Result<String, MinioError> {
    let mut out = Vec::new();
    let plan = plan(&*exec, args, &admin(), &mut out)?;
    execute(&plan, args.run_as(), mode, exec, &mut out)?;
    Ok(String::from_utf8(out).expect("utf-8"))
}

fn key_bytes() -> Vec<u8> {
    format!("AWS_ACCESS_KEY_ID={MINTED_ID}\nAWS_SECRET_ACCESS_KEY={MINTED_SECRET}\n").into_bytes()
}

/// Catches a first run that misses a step (a bucket, the policy, the key) or acts in
/// a dry run, an apply that writes the key with the wrong mode or owner, and a re-run
/// that plans again: a policy compared byte for byte would be created on every run,
/// since MinIO prints it back in another order.
#[test]
fn a_first_run_plans_buckets_policy_and_key_and_a_rerun_plans_nothing() {
    let (mut exec, state) = host(0);
    let n = policy_json(&args().buckets).expect("policy").len();
    let env = "(env MC_HOST_kbf, MC_CONFIG_DIR)";
    let dry = run(&mut exec, &args(), Mode::DryRun).expect("dry run");
    assert_eq!(
        dry,
        format!(
            "DRY-RUN: run {MC} mb kbf/kbf-cas {env}\n\
             DRY-RUN: run {MC} mb kbf/kbf-cas-2 {env}\n\
             DRY-RUN: run {MC} admin policy create kbf kbf-linux-rw /dev/stdin (stdin {n} bytes) {env}\n\
             DRY-RUN: run {MC} admin user svcacct add --json --policy /dev/stdin --name \
             kbf-linux-server --description \"kbf-deploy: kbf-linux-server\" kbf kbf-admin \
             (stdin {n} bytes) {env} > {KEY_FILE} (0600, owner 998:997, through s3-env)\n\
             dry run: nothing changed. Re-run with --apply.\n"
        )
    );
    assert!(exec.log.is_empty(), "a dry run applied a step");
    assert!(state.borrow().buckets.is_empty(), "a dry run made a bucket");

    run(&mut exec, &args(), Mode::Apply).expect("apply");
    {
        let m = state.borrow();
        assert_eq!(
            m.buckets,
            BTreeSet::from(["kbf-cas".to_owned(), "kbf-cas-2".to_owned()])
        );
        assert!(m.policies.contains_key("kbf-linux-rw"));
        assert_eq!(
            m.accounts.get(MINTED_ID).map(String::as_str),
            Some("kbf-linux-server")
        );
    }
    let want = Entry::File {
        perms: Perms::new(0o600),
        owner: SERVER,
        bytes: key_bytes(),
    };
    assert_eq!(exec.entries.get(&p(KEY_FILE)), Some(&want));

    exec.queries.borrow_mut().clear();
    let again = run(&mut exec, &args(), Mode::DryRun).expect("re-run");
    assert_eq!(
        again,
        "bucket kbf-cas: exists, kept\n\
         bucket kbf-cas-2: exists, kept\n\
         policy kbf-linux-rw: as wanted, kept\n\
         key /etc/kbf/s3.env: exists, kept\n\
         nothing to do\n"
    );
    let listed: Vec<String> = exec
        .queries
        .borrow()
        .iter()
        .filter(|c| env_of(c, "MC_HOST_kbfkey").is_some())
        .map(|c| c.args.join(" "))
        .collect();
    assert_eq!(
        listed,
        ["ls kbfkey/kbf-cas", "ls kbfkey/kbf-cas-2"],
        "the key was not checked"
    );
}

/// Every command line a whole lifecycle made, queries and steps alike.
fn argvs(exec: &FakeExec) -> Vec<String> {
    let steps = exec.log.iter().filter_map(|s| match s {
        Step::Run(cmd) | Step::Capture { cmd, .. } => Some(cmd.clone()),
        _ => None,
    });
    exec.queries
        .borrow()
        .iter()
        .cloned()
        .chain(steps)
        .map(|c| format!("{} {}", c.program, c.args.join(" ")))
        .collect()
}

/// THE secret test: argv is world-readable (`ps`, /proc/<pid>/cmdline) and printed
/// steps are kept in logs. Catches the admin password or a minted secret key on any
/// command line or printed line through a whole lifecycle (first run, apply, re-run
/// with its key check, rollback), for example `mc alias set kbf URL USER PASSWORD`
/// instead of `MC_HOST_kbf` in the environment.
#[test]
fn the_recorded_argv_holds_no_key() {
    let (mut exec, _state) = host(0);
    let mut printed = run(&mut exec, &args(), Mode::DryRun).expect("dry run");
    printed += &run(&mut exec, &args(), Mode::Apply).expect("apply");
    printed += &run(&mut exec, &args(), Mode::DryRun).expect("re-run");
    let rollback = Buckets {
        remove: true,
        ..args()
    };
    printed += &run(&mut exec, &rollback, Mode::Apply).expect("remove");

    let lines = argvs(&exec);
    assert!(
        lines.len() > 20,
        "the lifecycle ran {} commands",
        lines.len()
    );
    let secrets = [
        PASSWORD.to_owned(),
        url_encode(PASSWORD),
        MINTED_SECRET.to_owned(),
        url_encode(MINTED_SECRET),
    ];
    for secret in &secrets {
        for line in &lines {
            assert!(!line.contains(secret.as_str()), "a key on argv: {line}");
        }
        assert!(
            !printed.contains(secret.as_str()),
            "a key printed:\n{printed}"
        );
    }
    // The keys did reach mc, in its environment: the fake answers nothing without them.
    let first = exec.queries.borrow()[1].clone();
    let url = env_of(&first, "MC_HOST_kbf").map(str::to_owned);
    assert_eq!(
        url,
        Some(format!("http://kbf-admin:{}@127.0.0.1:9000", secrets[1]))
    );
}

/// Catches a re-run that keeps a key file with a mode or owner kbf-server's unit
/// cannot use (or others can read), and a policy kept although it names other
/// buckets.
#[test]
fn a_changed_policy_or_key_mode_is_planned_again() {
    let (mut exec, state) = host(0);
    let one = Buckets {
        buckets: vec![name("kbf-cas")],
        ..args()
    };
    {
        let mut m = state.borrow_mut();
        m.buckets.insert("kbf-cas".to_owned());
        let other = policy_json(&[name("kbf-other")]).expect("policy");
        m.policies
            .insert("kbf-linux-rw".to_owned(), as_minio_prints(&other));
        m.accounts
            .insert(MINTED_ID.to_owned(), "kbf-linux-server".to_owned());
    }
    exec.entries.insert(
        p(KEY_FILE),
        Entry::File {
            perms: Perms::new(0o644),
            owner: Owner { uid: 0, gid: 0 },
            bytes: key_bytes(),
        },
    );
    let n = policy_json(&one.buckets).expect("policy").len();
    let dry = run(&mut exec, &one, Mode::DryRun).expect("dry run");
    assert_eq!(
        dry,
        format!(
            "bucket kbf-cas: exists, kept\n\
             key /etc/kbf/s3.env: exists, kept\n\
             DRY-RUN: run {MC} admin policy create kbf kbf-linux-rw /dev/stdin (stdin {n} bytes) \
             (env MC_HOST_kbf, MC_CONFIG_DIR)\n\
             DRY-RUN: write /etc/kbf/s3.env (0600, owner 998:997, {} bytes)\n\
             dry run: nothing changed. Re-run with --apply.\n",
            key_bytes().len()
        )
    );
}

/// Catches a kept key file that no longer works (its account was removed): kbf-server
/// would start with it and fail on every blob.
#[test]
fn a_key_that_cannot_list_a_bucket_is_refused() {
    let (mut exec, state) = host(0);
    state.borrow_mut().buckets.insert("kbf-cas".to_owned());
    exec.entries.insert(
        p(KEY_FILE),
        Entry::File {
            perms: Perms::new(0o600),
            owner: SERVER,
            bytes: key_bytes(),
        },
    );
    let err = run(&mut exec, &args(), Mode::DryRun).expect_err("the account is gone");
    assert_eq!(
        err.to_string(),
        "the key in /etc/kbf/s3.env cannot list bucket kbf-cas"
    );
}

/// Catches a rollback that leaves a bucket, the account (its key would keep working),
/// the key file or the policy, or one that plans a removal of something absent (that
/// step would fail the apply).
#[test]
fn remove_takes_buckets_account_key_file_and_policy() {
    let (mut exec, state) = host(0);
    run(&mut exec, &args(), Mode::Apply).expect("apply");
    let rollback = Buckets {
        remove: true,
        ..args()
    };
    let env = "(env MC_HOST_kbf, MC_CONFIG_DIR)";
    let dry = run(&mut exec, &rollback, Mode::DryRun).expect("dry run");
    assert_eq!(
        dry,
        format!(
            "DRY-RUN: run {MC} rb --force kbf/kbf-cas {env}\n\
             DRY-RUN: run {MC} rb --force kbf/kbf-cas-2 {env}\n\
             DRY-RUN: run {MC} admin user svcacct rm kbf {MINTED_ID} {env}\n\
             DRY-RUN: remove /etc/kbf/s3.env\n\
             DRY-RUN: run {MC} admin policy remove kbf kbf-linux-rw {env}\n\
             dry run: nothing changed. Re-run with --apply.\n"
        )
    );
    run(&mut exec, &rollback, Mode::Apply).expect("remove");
    {
        let m = state.borrow();
        assert!(m.buckets.is_empty(), "a bucket was left");
        assert!(m.policies.is_empty(), "the policy was left");
        assert!(m.accounts.is_empty(), "the account was left");
    }
    assert!(!exec.entries.contains_key(&p(KEY_FILE)));
    let again = run(&mut exec, &rollback, Mode::DryRun).expect("re-run");
    assert_eq!(
        again,
        "bucket kbf-cas: absent\n\
         bucket kbf-cas-2: absent\n\
         key /etc/kbf/s3.env: absent\n\
         policy kbf-linux-rw: absent\n\
         nothing to do\n"
    );
}

/// Catches a run without `--key-file` that mints an account nobody can use, and a
/// plan without `--key-owner` that would demand root (it changes nothing root owns).
#[test]
fn without_a_key_file_no_account_is_made() {
    let (mut exec, state) = host(501);
    let no_key = Buckets {
        buckets: vec![name("kbf-cas")],
        key_file: None,
        key_owner: None,
        ..args()
    };
    state.borrow_mut().buckets.insert("kbf-cas".to_owned());
    run(&mut exec, &no_key, Mode::Apply).expect("apply as a user");
    let again = run(&mut exec, &no_key, Mode::DryRun).expect("re-run");
    assert_eq!(
        again,
        "bucket kbf-cas: exists, kept\n\
         policy kbf-linux-rw: as wanted, kept\n\
         no --key-file: no service account made\n\
         nothing to do\n"
    );
    assert!(state.borrow().accounts.is_empty());
    let rollback = Buckets {
        remove: true,
        ..no_key
    };
    run(&mut exec, &rollback, Mode::Apply).expect("remove as a user");
    assert!(state.borrow().policies.is_empty());
}

/// Catches a key file for a user's own kbf-server (no `--key-owner`) that is given an
/// owner, or a plan for it that demands root: it is minted as the user, mode 0600.
#[test]
fn a_key_file_without_an_owner_is_the_running_users() {
    let (mut exec, state) = host(501);
    state.borrow_mut().buckets.insert("kbf-cas".to_owned());
    let mine = Buckets {
        buckets: vec![name("kbf-cas")],
        key_file: Some(p("/srv/kbf-pilot/s3.env")),
        key_owner: None,
        ..args()
    };
    let dry = run(&mut exec, &mine, Mode::DryRun).expect("dry run");
    assert!(
        dry.contains("> /srv/kbf-pilot/s3.env (0600, through s3-env)\n"),
        "{dry}"
    );
    run(&mut exec, &mine, Mode::Apply).expect("apply as the user");
    let want = Entry::File {
        perms: Perms::new(0o600),
        owner: kbf_host::testkit::RUNNER,
        bytes: key_bytes(),
    };
    assert_eq!(exec.entries.get(&p("/srv/kbf-pilot/s3.env")), Some(&want));
}

/// Catches a plan made against a MinIO that is down (each `mc ls` would fail and the
/// plan would make every bucket), a `mc` that is another program, a key file in a
/// checkout or at a directory, a damaged key file kept as if it were good, an unknown
/// owner, and a tool that cannot start read as "absent".
#[test]
fn each_refusal_stops_the_plan() {
    type Setup = fn(&mut FakeExec, &mut Buckets, &mut Minio);
    let cases: [(&str, Setup, &str); 11] = [
        (
            "503",
            |e, _, _| e.http = Ok(503),
            "http://127.0.0.1:9000/minio/health/live is not live: it answered 503",
        ),
        (
            "refused",
            |e, _, _| e.http = Err(io::ErrorKind::ConnectionRefused),
            "http://127.0.0.1:9000/minio/health/live is not live: connection refused",
        ),
        (
            "not mc",
            |_, _, m| m.not_mc = true,
            "/opt/kbf/bin/mc is not the MinIO client: its --version names no RELEASE",
        ),
        (
            "no mc",
            |_, a, _| a.mc = p("/opt/kbf/bin/gone"),
            "/opt/kbf/bin/gone --version: entity not found",
        ),
        (
            "git",
            |e, a, _| {
                e.entries.insert(p("/srv/repo/.git"), Entry::Other);
                a.key_file = Some(p("/srv/repo/kbf/s3.env"));
            },
            "/srv/repo/kbf/s3.env is inside a git work tree (/srv/repo/.git exists): a key \
             file never goes into a checkout",
        ),
        (
            // The key file's own directory is a checkout's top: a loop that stops one
            // directory short of the file still passes the case above.
            "git beside",
            |e, a, _| {
                e.entries.insert(p("/srv/repo/kbf/.git"), Entry::Other);
                a.key_file = Some(p("/srv/repo/kbf/s3.env"));
            },
            "/srv/repo/kbf/s3.env is inside a git work tree (/srv/repo/kbf/.git exists): a \
             key file never goes into a checkout",
        ),
        (
            "dir",
            |e, _, _| {
                let dir = Entry::Dir {
                    perms: Perms::new(0o700),
                    owner: SERVER,
                };
                e.entries.insert(p(KEY_FILE), dir);
            },
            "/etc/kbf/s3.env is not a regular file",
        ),
        (
            "damaged",
            |e, _, _| {
                let file = Entry::File {
                    perms: Perms::new(0o600),
                    owner: SERVER,
                    bytes: b"AWS_ACCESS_KEY_ID=A\n".to_vec(),
                };
                e.entries.insert(p(KEY_FILE), file);
            },
            "/etc/kbf/s3.env: AWS_SECRET_ACCESS_KEY is not set (config file)",
        ),
        (
            "owner",
            |_, a, _| a.key_owner = Some("nobody-here".to_owned()),
            "no user nobody-here on this host (id -u, id -g)",
        ),
        (
            "unreadable",
            |_, a, _| a.key_file = Some(p("/unreadable")),
            "cannot read /unreadable: permission denied",
        ),
        (
            "id",
            |e, _, _| {
                e.answer = Box::new(|c: &Cmd| match c.program.as_str() {
                    "/usr/bin/id" => Err(io::Error::from(io::ErrorKind::NotFound)),
                    _ => answer(Rc::new(RefCell::new(Minio::default())))(c),
                });
            },
            "/usr/bin/id -u -- kbf-server: entity not found",
        ),
    ];
    for (what, setup, want) in cases {
        let (mut exec, state) = host(0);
        let mut a = args();
        setup(&mut exec, &mut a, &mut state.borrow_mut());
        let err = run(&mut exec, &a, Mode::DryRun).expect_err(what);
        assert_eq!(err.to_string(), want, "{what}");
        assert!(exec.log.is_empty(), "{what}: a refused plan applied a step");
    }
}

/// Catches a policy wider than kbf-server needs: one more action (say
/// `s3:DeleteBucket`) or a resource outside the buckets would pass every other test.
#[test]
fn the_policy_allows_only_object_access_to_the_buckets() {
    let json = policy_json(&[name("kbf-cas"), name("kbf-cas-2")]).expect("policy");
    assert_eq!(
        String::from_utf8(json).expect("utf-8"),
        r#"{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Action":["s3:GetBucketLocation","s3:ListBucket"],"Resource":["arn:aws:s3:::kbf-cas","arn:aws:s3:::kbf-cas-2"]},{"Effect":"Allow","Action":["s3:DeleteObject","s3:GetObject","s3:PutObject"],"Resource":["arn:aws:s3:::kbf-cas-2/*","arn:aws:s3:::kbf-cas/*"]}]}"#
    );
}

/// Catches a key file written from output that is not a key pair (an empty file would
/// be kept by the next run) or with a value that breaks its `KEY=VALUE` line, and a
/// refusal that quotes the output (it holds the secret).
#[test]
fn s3_env_takes_only_a_whole_key_pair() {
    let ok = br#"{"status":"success","accessKey":"AK1","secretKey":"s/k+1"}"#;
    assert_eq!(
        (S3_ENV.apply)(ok),
        Ok(b"AWS_ACCESS_KEY_ID=AK1\nAWS_SECRET_ACCESS_KEY=s/k+1\n".to_vec())
    );
    let refused = [
        &b"mc: <ERROR> s/k+1"[..],
        br#"{"accessKey":"AK1"}"#,
        br#"{"accessKey":"","secretKey":"s/k+1"}"#,
        br#"{"accessKey":"AK1","secretKey":"s k"}"#,
        br#"{"accessKey":"AK1","secretKey":"s\nk"}"#,
    ];
    for out in refused {
        let err = (S3_ENV.apply)(out).expect_err("not a key pair");
        assert_eq!(
            err,
            "mc printed no accessKey and secretKey: check `mc admin user svcacct ls`"
        );
    }
}
