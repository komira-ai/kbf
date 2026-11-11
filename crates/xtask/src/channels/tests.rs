use std::path::Path;

use super::*;
use kbf_testkit::dir::TestDir;

fn scratch(name: &str) -> TestDir {
    TestDir::under(std::env::temp_dir(), &format!("xtask-channels-{name}"))
}

#[test]
fn a_plain_version_is_three_whole_numbers() {
    // Catches: a version with a prefix, a suffix, a leading zero or the wrong number of parts
    // reaching a package name or a pool path.
    for good in ["0.0.0", "1.2.3", "10.20.30", "0.1.250"] {
        assert!(is_plain_version(good), "{good}");
    }
    for bad in [
        "",
        "1",
        "1.2",
        "1.2.3.4.5",
        "v1.2.3",
        "1.2.3+abc",
        "01.2.3",
        "1.02.3",
        "1.2.03",
        "1..3",
        "1.2.x",
        "+1.2.3",
        "-1.2.3",
        " 1.2.3",
        "1.2.3\n",
    ] {
        assert!(!is_plain_version(bad), "{bad:?}");
    }
}

#[test]
fn a_version_orders_as_numbers_and_one_too_large_is_none() {
    // Catches: 1.10.0 sorting below 1.9.0, and an overflowing number wrapping.
    assert_eq!(numbers("1.10.0"), Some((1, 10, 0)));
    assert!(numbers("1.10.0") > numbers("1.9.0"));
    assert_eq!(numbers("1.2"), None);
    assert_eq!(numbers("99999999999999999999.0.0"), None);
    assert_eq!(numbers("1.99999999999999999999.0"), None);
    assert_eq!(numbers("1.0.99999999999999999999"), None);
}

#[test]
fn bucket_and_distribution_names_are_checked_by_their_alphabets() {
    // Catches: a flag that is not a name reaching an `aws` command line.
    assert!(is_bucket("apt-bucket.v2"));
    for bad in ["", "Apt", "a_b", "a b", "a/b", "--x y"] {
        assert!(!is_bucket(bad), "{bad:?}");
    }
    assert!(is_distribution_id("E1ABC23"));
    for bad in ["", "e1", "E-1", "E 1", "E1/"] {
        assert!(!is_distribution_id(bad), "{bad:?}");
    }
}

#[test]
fn lines_are_appended_to_the_output_file_and_an_unwritable_file_is_named() {
    // Catches: an output file replaced instead of appended to (the runner's file holds the
    // outputs of earlier steps), and a failed write that goes unreported.
    let dir = scratch("append");
    let file = dir.join("out");
    append_output(&file, &["a=1".to_owned()]).expect("first");
    append_output(&file, &["b=2".to_owned(), "c=3".to_owned()]).expect("second");
    assert_eq!(
        std::fs::read_to_string(&file).expect("read"),
        "a=1\nb=2\nc=3\n"
    );

    let Err(Error::Spawn { what, why }) = append_output(&dir.join("no/such/dir/out"), &[]) else {
        panic!("not a spawn error");
    };
    assert!(
        what.starts_with("append to ") && what.ends_with("no/such/dir/out"),
        "{what}"
    );
    assert!(!why.is_empty());
}

#[test]
fn each_error_says_what_it_is_and_only_a_usage_error_is_a_usage_error() {
    // Catches: a usage error exiting 1 (or a failure exiting 2), and a refusal that prints
    // one problem of several.
    assert_eq!(Error::Usage("u".to_owned()).to_string(), "u");
    assert_eq!(Error::Failed("f".to_owned()).to_string(), "f");
    assert_eq!(
        Error::Refused(vec!["one".to_owned(), "two".to_owned()]).to_string(),
        "one\ntwo"
    );
    let spawned = Error::Spawn {
        what: "run aws".to_owned(),
        why: "not found".to_owned(),
    };
    assert_eq!(spawned.to_string(), "run aws: not found");

    assert!(matches!(
        crate::Error::from(Error::Usage("u".to_owned())),
        crate::Error::Usage(why) if why == "u"
    ));
    for other in [
        Error::Failed("f".to_owned()),
        Error::Refused(vec!["r".to_owned()]),
        spawned,
    ] {
        let text = other.to_string();
        let wrapped = crate::Error::from(other);
        assert!(matches!(wrapped, crate::Error::Channels(_)), "{wrapped}");
        assert_eq!(wrapped.to_string(), text);
    }
}

#[test]
fn the_spawn_error_of_an_io_failure_names_what_was_being_done() {
    // Catches: an operating-system reason shown without the program or file it is about.
    let failed = std::io::Error::new(std::io::ErrorKind::NotFound, "gone");
    assert_eq!(
        Error::spawn("run gpg".to_owned())(failed),
        Error::Spawn {
            what: "run gpg".to_owned(),
            why: "gone".to_owned()
        }
    );
}

/// A path below a scratch directory that holds nothing, for a program that is not there.
fn absent(dir: &Path, name: &str) -> std::path::PathBuf {
    dir.join(name)
}

#[test]
fn the_verbs_are_dispatched_with_their_flags() {
    // Catches: a verb wired to the wrong function, or a flag dropped on the way: each verb
    // runs against programs that are not there and fails naming the one it was given.
    let dir = scratch("run");
    let out = dir.join("out");

    let gate = run(Verb::Gate {
        label: "a-job".to_owned(),
        settings: vec!["XTASK_CHANNELS_TEST_NEVER_SET=".to_owned()],
        secrets: Vec::new(),
        requires: Vec::new(),
        output: Some(out.clone()),
    })
    .expect("gate");
    assert!(
        gate[0].starts_with("::notice title=The a-job job is off::"),
        "{gate:?}"
    );
    assert_eq!(
        std::fs::read_to_string(&out).expect("output"),
        "enabled=false\n"
    );

    let aws = absent(&dir, "no-aws");
    let missing = |what: Result<Vec<String>, Error>| match what {
        Err(Error::Spawn { what, .. }) => what,
        other => panic!("not a spawn error: {other:?}"),
    };
    let what = missing(run(Verb::CheckApt {
        arch: "amd64".to_owned(),
        version: "1.2.3".to_owned(),
        commit: "0123456789abcdef0123456789abcdef01234567".to_owned(),
        apt_url: "https://packages.komira.ai/apt".to_owned(),
        root: dir.to_path_buf(),
        apt_get: absent(&dir, "no-apt-get"),
        dpkg_query: absent(&dir, "no-dpkg-query"),
        dpkg: absent(&dir, "no-dpkg"),
        curl: absent(&dir, "no-curl"),
    }));
    assert!(what.ends_with("no-dpkg"), "{what}");

    let aws_wired = run(Verb::Assume {
        role_arn: "arn:aws:iam::123456789012:role/r".to_owned(),
        session_name: "kbf-release-42".to_owned(),
        region: "us-east-1".to_owned(),
        env_file: dir.join("env"),
        token_url: String::new(),
        aws: absent(&dir, "no-aws"),
    });
    assert_eq!(
        aws_wired.unwrap_err().to_string(),
        "the runner offers no OIDC token (the job needs `permissions: id-token: write`)"
    );
    assert!(!dir.join("env").exists());

    let what = missing(run(Verb::Apt {
        verb: apt::Verb::Read {
            suite: apt::Suite::Testing,
            bucket: "apt-bucket".to_owned(),
            out: dir.join("read"),
            aws,
        },
    }));
    assert!(what.ends_with("no-aws"), "{what}");
}

/// What the three real-program wrappers say of `program` run with `args`: its answer, or the
/// error that it could not be started. The program is this test binary, which lists its tests
/// for `--list` and refuses an option it does not know.
fn through_each_wrapper(program: &Path, args: &[&str]) -> [Result<aws::Answer, Error>; 3] {
    let args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
    [
        aws::Aws::run(&aws::RealAws(program.to_path_buf()), &args),
        apt::sign::Gpg::run(&apt::sign::RealGpg(program.to_path_buf()), &args),
        check::Host::run(&check::RealHost, program, &args),
    ]
}

#[test]
fn the_real_programs_answer_with_their_status_and_both_streams_and_a_missing_one_is_named() {
    // Catches: a failing `aws`, `gpg` or `apt-get` read as a pass, an answer that drops stdout
    // or stderr, and a program that is not there reported without its name.
    let me = std::env::current_exe().expect("this test binary");
    for answer in through_each_wrapper(&me, &["--list", "--format", "terse"]) {
        let answer = answer.expect("started");
        assert!(answer.ok);
        assert!(
            answer
                .stdout
                .contains("channels::tests::a_plain_version_is_three_whole_numbers: test"),
            "{}",
            answer.stdout
        );
    }
    for answer in through_each_wrapper(&me, &["--no-such-option-of-the-test-harness"]) {
        let answer = answer.expect("started");
        assert!(!answer.ok);
        assert!(
            answer.stderr.contains("no-such-option"),
            "{}",
            answer.stderr
        );
    }

    let dir = scratch("real-missing");
    let gone = dir.join("gone");
    for answer in through_each_wrapper(&gone, &[]) {
        let Err(Error::Spawn { what, why }) = answer else {
            panic!("not a spawn error");
        };
        assert_eq!(what, gone.display().to_string());
        assert_eq!(why, "No such file or directory (os error 2)");
    }
}
