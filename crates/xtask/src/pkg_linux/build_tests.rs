//! The build of the packages (`pkg-build`) against a model of nfpm: a package built twice from
//! the same inputs is the same bytes, and one that is not, or a failing nfpm, is refused.

use std::cell::RefCell;
use std::os::unix::fs::MetadataExt;

use super::tests::scratch;
use super::*;
use kbf_testkit::dir::TestDir;

/// The environment and arguments of one call of the model.
type Call = (Vec<(String, String)>, Vec<String>);

/// A model of nfpm: records its calls and writes `<dir>/kbf-installer.<format>` holding
/// what `bytes` says of the call.
struct Model {
    calls: RefCell<Vec<Call>>,
}

impl Model {
    fn new() -> Model {
        Model {
            calls: RefCell::new(Vec::new()),
        }
    }

    /// The nfpm closure; `leave` says what a call leaves in its target directory:
    /// the file names, with the bytes the source's time and the environment give.
    fn nfpm<'a>(
        &'a self,
        source: &'a Path,
        leave: impl Fn(&str, &str) -> Vec<(String, String)> + 'a,
    ) -> impl FnMut(&[(&str, &str)], &[&str]) -> Result<Out, Error> + 'a {
        move |env, args| {
            let env: Vec<(String, String)> = env
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect();
            self.calls
                .borrow_mut()
                .push((env.clone(), args.iter().map(|a| (*a).to_owned()).collect()));
            let at = args.iter().position(|a| *a == "-p").expect("-p");
            let format = args[at + 1];
            let dir = Path::new(args[args.len() - 1]);
            let mtime = std::fs::metadata(source)
                .expect("source")
                .mtime()
                .to_string();
            let epoch = env[0].1.clone();
            for (name, text) in leave(format, &format!("{epoch}/{mtime}")) {
                std::fs::write(dir.join(name), text).expect("package");
            }
            Ok(Out {
                ok: true,
                ..Out::default()
            })
        }
    }
}

/// A config naming one source file, and the build over it.
fn built(
    name: &str,
    leave: impl Fn(&str, &str) -> Vec<(String, String)>,
) -> (Result<Vec<String>, Error>, TestDir, Model) {
    let dir = scratch(name);
    let source = dir.join("kbf-installer");
    std::fs::write(&source, "binary").expect("source");
    let config_path = dir.join("nfpm.yaml");
    let config = format!(
        "contents:\n  - dst: \"/x\"\n    src: {}\n",
        quoted(&source.to_string_lossy())
    );
    std::fs::write(&config_path, config).expect("config");
    let model = Model::new();
    let out = dir.join("out");
    let build = Build {
        config: &config_path,
        out: &out,
        epoch: 77,
    };
    let result = super::build(&mut model.nfpm(&source, leave), &build);
    // The caller holds the directory, which the output is in, until it is done.
    (result, dir, model)
}

/// Package bytes that depend on the environment's time only.
fn by_epoch(format: &str, seen: &str) -> Vec<(String, String)> {
    let epoch = seen.split('/').next().expect("epoch");
    vec![(
        format!("kbf-installer.{format}"),
        format!("{format} at {epoch}"),
    )]
}

#[test]
fn a_package_that_depends_on_the_epoch_alone_is_built_twice_and_the_same() {
    let (result, dir, model) = built("build-same", by_epoch);
    let out = dir.join("out");
    let printed = result.expect("reproducible");
    assert_eq!(printed.len(), 2);
    assert!(printed[0].ends_with("kbf-installer.deb"), "{printed:?}");
    assert!(printed[1].ends_with("kbf-installer.rpm"), "{printed:?}");
    let calls = model.calls.borrow();
    assert_eq!(calls.len(), 4, "two formats, two builds each");
    for (env, args) in calls.iter() {
        assert_eq!(env, &[("SOURCE_DATE_EPOCH".to_owned(), "77".to_owned())]);
        assert_eq!(&args[..3], ["package", "-f", args[2].as_str()]);
    }
    assert!(out.join("kbf-installer.deb").is_file());
    assert!(out.join("kbf-installer.rpm").is_file());
    assert!(
        !out.join("again").exists(),
        "the second builds are not left"
    );
}

#[test]
fn a_package_that_carries_the_time_of_its_sources_is_refused() {
    // The defect `SOURCE_DATE_EPOCH` is for: nfpm taking the sources' own times.
    let (result, _, _) = built("build-mtime", |format, seen| {
        let mtime = seen.split('/').nth(1).expect("mtime");
        vec![(
            format!("kbf-installer.{format}"),
            format!("{format} at {mtime}"),
        )]
    });
    let err = result.expect_err("not reproducible").to_string();
    assert!(
        err.contains("kbf-installer.deb is not reproducible"),
        "{err}"
    );
}

#[test]
fn nfpm_failing_or_leaving_the_wrong_files_is_refused() {
    let (result, _, _) = built("build-none", |_, _| Vec::new());
    assert!(
        result
            .expect_err("none")
            .to_string()
            .contains("expected nfpm to leave one .deb")
    );
    let (result, _, _) = built("build-two", |format, _| {
        vec![
            (format!("a.{format}"), String::new()),
            (format!("b.{format}"), String::new()),
        ]
    });
    assert!(
        result
            .expect_err("two")
            .to_string()
            .contains("expected nfpm to leave one .deb")
    );
}

#[test]
fn nfpm_that_fails_is_reported_with_its_message() {
    let dir = scratch("build-fails");
    let config = dir.join("nfpm.yaml");
    std::fs::write(&config, "contents: []\n").expect("config");
    let mut fails = |_: &[(&str, &str)], _: &[&str]| {
        Ok(Out {
            ok: false,
            stdout: String::new(),
            stderr: " bad yaml \n".to_owned(),
        })
    };
    let err = build(
        &mut fails,
        &Build {
            config: &config,
            out: &dir.join("out"),
            epoch: 1,
        },
    )
    .expect_err("fails");
    assert_eq!(
        err,
        Error::Failed("nfpm package -p deb: bad yaml".to_owned())
    );
}

#[test]
fn a_config_with_a_broken_src_line_or_a_missing_source_is_refused() {
    let dir = scratch("build-config");
    let config = dir.join("nfpm.yaml");
    let mut never = |_: &[(&str, &str)], _: &[&str]| -> Result<Out, Error> { panic!("not run") };
    let build_with = |text: &str, never: &mut Nfpm| {
        std::fs::write(&config, text).expect("config");
        build(
            never,
            &Build {
                config: &config,
                out: &dir.join("out"),
                epoch: 1,
            },
        )
        .expect_err("refused")
        .to_string()
    };
    assert!(build_with("    src: not json\n", &mut never).contains("not a src line"));
    assert!(
        build_with("    src: \"/nonexistent/x\"\n", &mut never)
            .contains("set the time of /nonexistent/x")
    );
    let missing = dir.join("absent.yaml");
    let err = build(
        &mut never,
        &Build {
            config: &missing,
            out: &dir,
            epoch: 1,
        },
    )
    .expect_err("absent");
    assert!(err.to_string().contains("absent.yaml"), "{err}");
}

#[test]
fn the_nfpm_process_gets_the_given_environment_and_nothing_else() {
    let mut env = nfpm_process(Path::new("/usr/bin/env"));
    let out = env(&[("A", "1")], &[]).expect("env");
    assert!(out.ok);
    assert_eq!(out.stdout, "A=1\n");
    let mut no = nfpm_process(Path::new("/nonexistent/nfpm"));
    let err = no(&[], &[]).expect_err("not a program");
    assert!(err.to_string().contains("/nonexistent/nfpm"), "{err}");
    let mut fails = nfpm_process(Path::new("/usr/bin/false"));
    assert!(!fails(&[], &[]).expect("false").ok);
}
