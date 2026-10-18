use std::cell::RefCell;
use std::path::{Path, PathBuf};

use super::*;

const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/gamma-tests")
        .join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("scratch");
    dir
}

fn dist(name: &str) -> (PathBuf, PathBuf) {
    let root = scratch(name);
    let dist = root.join("dist");
    fs::create_dir_all(&dist).unwrap();
    for t in tarballs(SHA) {
        fs::write(dist.join(&t), format!("bytes of {t}")).unwrap();
    }
    (root, dist)
}

/// The bundle a download writes: one file named like gh's, holding the file's name.
fn writes_one(file: &Path, into: &Path) -> Result<(), Error> {
    let name = file.file_name().unwrap().to_string_lossy().into_owned();
    fs::write(
        into.join("sha256:abc.jsonl"),
        format!("attestation of {name}"),
    )
    .unwrap();
    Ok(())
}

fn entries(tar: &Path) -> Vec<(String, String)> {
    let bytes = fs::read(tar).unwrap();
    let mut a = tar::Archive::new(&bytes[..]);
    a.entries()
        .unwrap()
        .map(|e| {
            let mut e = e.unwrap();
            let name = e.path().unwrap().to_string_lossy().into_owned();
            let mut body = String::new();
            std::io::Read::read_to_string(&mut e, &mut body).unwrap();
            (name, body)
        })
        .collect()
}

/// Proves the bundle holds exactly the three tarballs the farm runs, each beside its own
/// attestation, named as the release store names them. Catches a tarball paired with
/// another's bundle, a missing platform, or an extra file (kbf-server) that the store
/// would refuse.
#[test]
fn prepare_packs_each_tarball_with_its_own_attestation() {
    let (root, dist) = dist("pack");
    let out = root.join("bundle.tar");
    let lines = prepare(&writes_one, SHA, "h", "u", &dist, &root, &out).unwrap();
    assert_eq!(lines[0], "::add-mask::h");
    assert_eq!(lines[1], "::add-mask::u");
    let got = entries(&out);
    let names: Vec<_> = got.iter().map(|(n, _)| n.as_str()).collect();
    let want = [
        format!("kbf-{SHA}-linux-x86_64.tar.gz"),
        format!("kbf-{SHA}-linux-x86_64.tar.gz.attestation"),
        format!("kbf-{SHA}-linux-arm64.tar.gz"),
        format!("kbf-{SHA}-linux-arm64.tar.gz.attestation"),
        format!("kbf-daemon-{SHA}-darwin-arm64.tar.gz"),
        format!("kbf-daemon-{SHA}-darwin-arm64.tar.gz.attestation"),
    ];
    assert_eq!(names, want.iter().map(String::as_str).collect::<Vec<_>>());
    for pair in got.chunks(2) {
        assert_eq!(pair[0].1, format!("bytes of {}", pair[0].0));
        assert_eq!(pair[1].1, format!("attestation of {}", pair[0].0));
    }
}

#[test]
fn prepare_refuses_what_it_cannot_pack() {
    let (root, dist) = dist("refuse");
    let out = root.join("bundle.tar");
    let run = |download: &Download<'_>, commit: &str, host: &str, user: &str| {
        for n in 0..3 {
            let _ = fs::remove_dir_all(root.join(format!("gamma-attestation-{n}")));
        }
        prepare(download, commit, host, user, &dist, &root, &out)
    };
    assert!(matches!(
        run(&writes_one, "abc", "h", "u"),
        Err(Error::Usage(_))
    ));
    assert!(matches!(
        run(&writes_one, &SHA.to_uppercase(), "h", "u"),
        Err(Error::Usage(_))
    ));
    assert!(matches!(
        run(&writes_one, SHA, "", "u"),
        Err(Error::Usage(_))
    ));
    assert!(matches!(
        run(&writes_one, SHA, "h", " "),
        Err(Error::Usage(_))
    ));
    // No bundle, two bundles, an empty one and an oversized one.
    let none = |_: &Path, _: &Path| Ok(());
    assert!(matches!(run(&none, SHA, "h", "u"), Err(Error::Failed(m)) if m.contains("found 0")));
    let two = |_: &Path, into: &Path| {
        fs::write(into.join("a"), "x").unwrap();
        fs::write(into.join("b"), "x").unwrap();
        Ok(())
    };
    assert!(matches!(run(&two, SHA, "h", "u"), Err(Error::Failed(m)) if m.contains("found 2")));
    let empty = |_: &Path, into: &Path| {
        fs::write(into.join("a"), "").unwrap();
        Ok(())
    };
    assert!(
        matches!(run(&empty, SHA, "h", "u"), Err(Error::Failed(m)) if m.contains("empty or over"))
    );
    let big = |_: &Path, into: &Path| {
        fs::write(into.join("a"), vec![b'x'; MAX_ATTESTATION + 1]).unwrap();
        Ok(())
    };
    assert!(
        matches!(run(&big, SHA, "h", "u"), Err(Error::Failed(m)) if m.contains("empty or over"))
    );
    // A download that fails stops the pack.
    let fails = |_: &Path, _: &Path| Err(Error::Failed("no attestation".to_owned()));
    assert!(run(&fails, SHA, "h", "u").is_err());
    // A tarball missing from dist.
    fs::remove_file(dist.join(format!("kbf-{SHA}-linux-arm64.tar.gz"))).unwrap();
    assert!(run(&writes_one, SHA, "h", "u").is_err());
}

/// Proves a run does not mix up the scratch of one file with another's: each download is
/// handed a directory of its own.
#[test]
fn each_download_gets_its_own_empty_directory() {
    let (root, dist) = dist("dirs");
    let seen = RefCell::new(Vec::new());
    let record = |f: &Path, into: &Path| {
        seen.borrow_mut().push(into.to_path_buf());
        writes_one(f, into)
    };
    prepare(&record, SHA, "h", "u", &dist, &root, &root.join("o.tar")).unwrap();
    let mut dirs = seen.into_inner();
    assert_eq!(dirs.len(), 3);
    dirs.sort();
    dirs.dedup();
    assert_eq!(dirs.len(), 3);
    // A scratch directory left with a file in it is refused, not reused.
    let again = prepare(
        &writes_one,
        SHA,
        "h",
        "u",
        &dist,
        &root,
        &root.join("o.tar"),
    );
    assert!(matches!(again, Err(Error::Failed(m)) if m.contains("is not empty")));
}

#[test]
fn the_ssh_command_names_the_target_and_the_verb_once() {
    let args = ssh_args(SHA, "host", "user");
    assert_eq!(args[..3], ["ssh", "user@host", "sudo"]);
    assert_eq!(
        args[3..],
        [
            "kbf-installer",
            "deploy",
            "--apply",
            "--json",
            "--commit",
            SHA,
            "--bundle",
            "-"
        ]
    );
}

/// Proves the bundle reaches the program's standard input and its standard output is the
/// summary file, and that its exit status decides the verb. `cat` plays `tailscale`.
#[test]
fn deploy_pipes_the_bundle_in_and_the_summary_out() {
    let root = scratch("pipe");
    let (bundle, summary) = (root.join("b.tar"), root.join("s.json"));
    fs::write(&bundle, "the bundle").unwrap();
    assert_eq!(
        pipe(Path::new("cat"), &[], &bundle, &summary).unwrap(),
        Some(0)
    );
    assert_eq!(fs::read_to_string(&summary).unwrap(), "the bundle");
    assert_eq!(
        pipe(Path::new("false"), &[], &bundle, &summary).unwrap(),
        Some(1)
    );
    assert!(pipe(Path::new("/no/such/program"), &[], &bundle, &summary).is_err());
    assert!(pipe(Path::new("cat"), &[], &root.join("absent"), &summary).is_err());
}

#[test]
fn deploy_fails_with_the_status_and_never_names_the_host() {
    let root = scratch("deploy");
    let (bundle, summary) = (root.join("b.tar"), root.join("s.json"));
    fs::write(&bundle, "x").unwrap();
    // `false` ignores the ssh arguments and exits 1, as a failed deploy does.
    let err = deploy(
        Path::new("false"),
        SHA,
        "secret-host",
        "secret-user",
        &bundle,
        &summary,
    )
    .unwrap_err();
    let shown = err.to_string();
    assert!(shown.contains("exited 1"), "{shown}");
    assert!(!shown.contains("secret"), "{shown}");
    // `true` is a deploy that exited 0.
    assert!(deploy(Path::new("true"), SHA, "h", "u", &bundle, &summary).is_ok());
    assert!(matches!(
        deploy(Path::new("true"), "abc", "h", "u", &bundle, &summary),
        Err(Error::Usage(_))
    ));
    assert!(matches!(
        deploy(Path::new("true"), SHA, "", "u", &bundle, &summary),
        Err(Error::Usage(_))
    ));
}

/// Proves the table shows platform, state and seconds, the result and the steps, and no
/// node id (an id may be a host name).
#[test]
fn the_table_names_no_node() {
    let json = format!(
        r#"{{"commit":"{SHA}","result":"failed","exit":1,"reason":"node_not_confirmed",
        "nodes":[{{"id":"mac-studio-7","platform":"darwin-arm64","state":"timed_out","secs":1800}}],
        "steps":[{{"name":"bundle","ok":true,"ms":1}},{{"name":"nodes","ok":false,"ms":2}}]}}"#
    );
    let md = table(&json);
    assert!(md.contains("| darwin-arm64 | timed_out | 1800 |"), "{md}");
    assert!(md.contains("result: failed (exit 1) node_not_confirmed"));
    assert!(md.contains("steps: bundle=ok nodes=FAILED"));
    assert!(!md.contains("mac-studio"));
    assert!(table("not json").contains("no summary was printed"));
    assert!(table("").contains("no summary was printed"));
    assert!(table("{}").contains("result: ?"));
}

#[test]
fn summary_appends_and_survives_a_missing_file() {
    let root = scratch("summary");
    let out = root.join("step-summary.md");
    fs::write(&out, "earlier\n").unwrap();
    summary(&root.join("absent.json"), &out).unwrap();
    let text = fs::read_to_string(&out).unwrap();
    assert!(text.starts_with("earlier\n") && text.contains("no summary was printed"));
    assert!(summary(&root.join("absent.json"), &root.join("no/dir/out")).is_err());
}
