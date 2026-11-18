//! The Docker runtime: the arguments of each call, what each answer means, and that nothing
//! else of the environment reaches the client.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use super::config::FrontendConfig;
use super::runtime::{Container, Docker, Runtime, RuntimeError};
use crate::runner::{Cmd, Exec, Runner};

/// Answers in turn, and every command it was given.
struct Script {
    seen: Rc<RefCell<Vec<Cmd>>>,
    answers: RefCell<VecDeque<io::Result<Exec>>>,
}

impl Runner for Script {
    fn run(&self, cmd: &Cmd) -> io::Result<Exec> {
        self.seen.borrow_mut().push(cmd.clone());
        self.answers.borrow_mut().pop_front().expect("an answer")
    }
}

fn done(code: i32, stdout: &str, stderr: &str) -> io::Result<Exec> {
    Ok(Exec {
        code: Some(code),
        signal: None,
        stdout: stdout.into(),
        stderr: stderr.into(),
    })
}

fn docker(answers: Vec<io::Result<Exec>>) -> (Docker, Rc<RefCell<Vec<Cmd>>>) {
    let seen = Rc::new(RefCell::new(Vec::new()));
    let script = Script {
        seen: Rc::clone(&seen),
        answers: RefCell::new(answers.into()),
    };
    (
        Docker::new(Box::new(script), PathBuf::from("/usr/bin/docker")),
        seen,
    )
}

fn args(seen: &Rc<RefCell<Vec<Cmd>>>) -> Vec<String> {
    let seen = seen.borrow();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].program, "/usr/bin/docker");
    assert_eq!(
        seen[0].env,
        [("DOCKER_CONFIG".to_owned(), "/nonexistent/docker".to_owned())]
    );
    seen[0].args.clone()
}

fn err(r: Result<impl std::fmt::Debug, RuntimeError>) -> String {
    r.unwrap_err().0
}

/// Each call is one `docker` command with the arguments it needs and no shell; the client is
/// told to read no configuration of the account.
#[test]
fn each_call_is_one_docker_command_with_its_arguments() {
    let (d, seen) = docker(vec![done(0, "", "")]);
    d.load(Path::new("/work/image.tar")).unwrap();
    assert_eq!(args(&seen), ["image", "load", "--input", "/work/image.tar"]);
    assert_eq!(seen.borrow()[0].timeout, Some(Duration::from_secs(900)));

    let (d, seen) = docker(vec![done(0, "", "")]);
    d.set_restart("kbf-frontend", "no").unwrap();
    assert_eq!(args(&seen), ["update", "--restart", "no", "kbf-frontend"]);

    let (d, seen) = docker(vec![done(0, "", "")]);
    let argv = ["/usr/bin/kbf".to_owned(), "cluster".to_owned()];
    d.exec("kbf-frontend", &argv).unwrap();
    assert_eq!(
        args(&seen),
        ["exec", "kbf-frontend", "/usr/bin/kbf", "cluster"]
    );

    let (d, seen) = docker(vec![done(0, "", "")]);
    d.stop("kbf-frontend", 30).unwrap();
    assert_eq!(args(&seen), ["stop", "--time", "30", "kbf-frontend"]);
    // The client waits for the grace, and then for the kill.
    assert_eq!(seen.borrow()[0].timeout, Some(Duration::from_secs(150)));

    let (d, seen) = docker(vec![done(0, "", "")]);
    d.remove("kbf-frontend").unwrap();
    assert_eq!(args(&seen), ["rm", "--force", "kbf-frontend"]);

    let (d, seen) = docker(vec![done(0, "", "")]);
    d.remove_image("sha256:abc").unwrap();
    assert_eq!(args(&seen), ["image", "rm", "sha256:abc"]);

    let spec = FrontendConfig::parse(
        "[frontend]\nname = \"f\"\nrestart = \"unless-stopped\"\napi = \"127.0.0.1:1\"\ndrain_token_file = \"/t\"\n",
    )
    .unwrap()
    .run;
    let (d, seen) = docker(vec![done(0, "abc\n", "")]);
    d.run(&spec, "sha256:abc").unwrap();
    assert_eq!(args(&seen), spec.run_args("sha256:abc"));
}

/// An image is found by its id, and one the runtime does not have is none, not an error.
#[test]
fn an_image_is_found_by_its_id_and_a_missing_one_is_none() {
    let (d, seen) = docker(vec![done(0, "sha256:abc\n", "")]);
    assert_eq!(
        d.image_id("kbf-frontend:x").unwrap().as_deref(),
        Some("sha256:abc")
    );
    assert_eq!(
        args(&seen),
        ["image", "inspect", "--format", "{{.Id}}", "kbf-frontend:x"]
    );
    let (d, _) = docker(vec![done(1, "", "Error: No such image: kbf-frontend:x\n")]);
    assert_eq!(d.image_id("kbf-frontend:x").unwrap(), None);
    let (d, _) = docker(vec![done(1, "", "Cannot connect to the Docker daemon\n")]);
    assert!(err(d.image_id("kbf-frontend:x")).contains("Cannot connect"));
}

/// A container is read from one inspect line; anything that is not three fields is an error.
#[test]
fn a_container_is_read_from_one_line() {
    let (d, seen) = docker(vec![done(
        0,
        "sha256:i kbf-frontend:c true unless-stopped\n",
        "",
    )]);
    assert_eq!(
        d.container("kbf-frontend").unwrap(),
        Some(Container {
            image_id: "sha256:i".to_owned(),
            image_ref: "kbf-frontend:c".to_owned(),
            running: true,
            restart: "unless-stopped".to_owned(),
        })
    );
    assert_eq!(
        args(&seen),
        [
            "container",
            "inspect",
            "--format",
            "{{.Image}} {{.Config.Image}} {{.State.Running}} {{.HostConfig.RestartPolicy.Name}}",
            "kbf-frontend",
        ]
    );
    let (d, _) = docker(vec![done(0, "sha256:i kbf-frontend:c false\n", "")]);
    let made = d.container("kbf-frontend").unwrap().unwrap();
    assert!(!made.running);
    assert_eq!(made.restart, "", "an empty policy name is read as none");
    let (d, _) = docker(vec![done(
        1,
        "",
        "Error: No such container: kbf-frontend\n",
    )]);
    assert_eq!(d.container("kbf-frontend").unwrap(), None);
    for said in ["", "a b", "a b maybe", "a b true no extra"] {
        let (d, _) = docker(vec![done(0, said, "")]);
        assert!(
            err(d.container("kbf-frontend")).contains("not what was asked"),
            "{said:?}"
        );
    }
}

/// Stopping or removing what is not there is done already; any other failure is an error that
/// carries the first line docker said, cut.
#[test]
fn what_is_not_there_is_stopped_and_removed_already() {
    let missing = "Error response from daemon: No such container: kbf-frontend\n";
    let (d, _) = docker(vec![done(1, "", missing)]);
    d.stop("kbf-frontend", 30).unwrap();
    let (d, _) = docker(vec![done(1, "", missing)]);
    d.remove("kbf-frontend").unwrap();
    let (d, _) = docker(vec![done(1, "", "Error: No such image: x\n")]);
    d.remove_image("x").unwrap();

    let long = format!("{}\nsecond line\n", "x".repeat(500));
    let (d, _) = docker(vec![done(125, "", &long)]);
    let why = err(d.stop("kbf-frontend", 30));
    assert!(
        why.starts_with("docker stop: exit status Some(125): xxx"),
        "{why}"
    );
    assert!(why.len() < 300 && !why.contains("second"), "{why}");

    let (d, _) = docker(vec![Ok(Exec {
        code: None,
        signal: None,
        stdout: Vec::new(),
        stderr: Vec::new(),
    })]);
    assert!(err(d.set_restart("f", "no")).contains("exit status None"));
}

/// A docker that cannot be started is an error that names the verb.
#[test]
fn a_docker_that_cannot_be_started_is_an_error() {
    let (d, _) = docker(vec![Err(io::Error::other("no such file"))]);
    assert_eq!(err(d.load(Path::new("/x"))), "docker image: no such file");
    let (d, _) = docker(vec![Err(io::Error::other("no such file"))]);
    assert_eq!(err(d.exec("f", &[])), "docker exec: no such file");
}

fn check_spec() -> super::config::RunSpec {
    FrontendConfig::parse(
        "[frontend]\nname = \"f\"\nrestart = \"unless-stopped\"\nnetwork = \"bridge\"\nuser = \"65532:65532\"\ngroup_add = [\"990\", \"991\"]\nports = [\"127.0.0.1:8982:8982\"]\nmounts = [\"/etc/kbf:/etc/kbf:ro\", \"/var/lib/kbf:/var/lib/kbf\"]\ncommand = [\"--config\", \"/etc/kbf/kbf.toml\"]\napi = \"127.0.0.1:1\"\ndrain_token_file = \"/t\"\n",
    )
    .unwrap()
    .run
}

/// The check is a throwaway container with the real one's network, user, groups and mounts and
/// its command followed by `--check`; it has no name, restart policy or published port, and the
/// call is bounded in time.
#[test]
fn the_check_is_a_throwaway_container_with_the_configuration_and_check_flag() {
    let (d, seen) = docker(vec![done(0, "kbf frontend: ok\n", "")]);
    let checked = d.check(&check_spec(), "sha256:new").unwrap();
    assert!(checked.passed);
    assert_eq!(checked.ended, "exit status 0");
    assert_eq!(
        args(&seen),
        [
            "run",
            "--rm",
            "--network",
            "bridge",
            "--user",
            "65532:65532",
            "--group-add",
            "990",
            "--group-add",
            "991",
            "--volume",
            "/etc/kbf:/etc/kbf:ro",
            "--volume",
            "/var/lib/kbf:/var/lib/kbf",
            "sha256:new",
            "--config",
            "/etc/kbf/kbf.toml",
            "--check",
        ]
    );
    assert_eq!(seen.borrow()[0].timeout, Some(Duration::from_secs(60)));
}

/// A configuration with none of the optional parts gives the shortest line: the image and
/// `--check`.
#[test]
fn a_check_with_no_options_is_the_image_and_the_check_flag() {
    let spec = FrontendConfig::parse(
        "[frontend]\nname = \"f\"\nrestart = \"always\"\napi = \"127.0.0.1:1\"\ndrain_token_file = \"/t\"\n",
    )
    .unwrap()
    .run;
    assert_eq!(
        spec.check_args("sha256:x"),
        ["run", "--rm", "sha256:x", "--check"]
    );
}

/// A check that exits non-zero is an answer, not a runtime error: status 2 (refused) and 75
/// (not ready) carry what the program printed, standard error first, on one line with control
/// characters replaced; a signal says which.
#[test]
fn a_check_that_says_no_is_an_answer_with_its_output() {
    let (d, _) = docker(vec![done(
        2,
        "out line\n",
        "kbf: --history-objects was removed\n\n  fix kbf.toml\x1b\n",
    )]);
    let checked = d.check(&check_spec(), "sha256:new").unwrap();
    assert!(!checked.passed);
    assert_eq!(checked.ended, "exit status 2");
    assert_eq!(
        checked.output,
        "kbf: --history-objects was removed / fix kbf.toml? / out line"
    );

    let (d, _) = docker(vec![done(75, "", "certificate not yet valid\n")]);
    let checked = d.check(&check_spec(), "sha256:new").unwrap();
    assert_eq!(
        (checked.passed, checked.ended.as_str()),
        (false, "exit status 75")
    );

    let (d, _) = docker(vec![Ok(Exec {
        code: None,
        signal: Some(9),
        stdout: Vec::new(),
        stderr: Vec::new(),
    })]);
    let checked = d.check(&check_spec(), "sha256:new").unwrap();
    assert!(!checked.passed);
    assert_eq!(checked.ended, "signal 9 (SIGKILL)");
    assert_eq!(checked.output, "");
}

/// The output of a check is cut at 1000 characters.
#[test]
fn the_output_of_a_check_is_cut() {
    let (d, _) = docker(vec![done(2, "", &"y".repeat(5000))]);
    let checked = d.check(&check_spec(), "sha256:new").unwrap();
    assert_eq!(checked.output, "y".repeat(1000));
}

/// Docker's own failure (exit 125) and a docker that cannot be started are runtime errors, not
/// an answer of the release.
#[test]
fn docker_failing_to_run_the_check_is_a_runtime_error() {
    let (d, _) = docker(vec![done(
        125,
        "",
        "docker: Error response from daemon: no such image\n",
    )]);
    let why = err(d.check(&check_spec(), "sha256:new"));
    assert_eq!(
        why,
        "docker run: exit status Some(125): docker: Error response from daemon: no such image"
    );
    let (d, _) = docker(vec![Err(io::Error::other("timed out"))]);
    assert_eq!(
        err(d.check(&check_spec(), "sha256:new")),
        "docker run: timed out"
    );
}
