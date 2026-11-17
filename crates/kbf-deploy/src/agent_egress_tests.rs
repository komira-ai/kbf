//! `job-service agent-egress`: the lines rendered from a frontend's `kbf.toml`, and each
//! refusal by its whole message. The renderer is tested on text; the verb reads a file in a
//! scratch directory.

use std::path::Path;

use kbf_testkit::dir::TestDir;

use super::*;

/// The path the texts are rendered as coming from.
const FILE: &str = "/etc/kbf/kbf.toml";

fn render_text(text: &str) -> Result<String, EgressError> {
    render(text, Path::new(FILE))
}

/// A put-dir frontend's tables.
const FRONTEND: &str = r#"mode = "frontend"

[executor]
executor = "js-agent"

[task-runs]
task-out-root = "/var/lib/kbf/out"
"#;

/// The head every rendering is.
const HEAD: &str = "# The egress of kbf's jobs, from /etc/kbf/kbf.toml. Put these lines in \
                    /etc/js-agent/flags on\n# every agent of the farm (the Linux js-agent, \
                    which runs crun): a put-dir job has no\n# destination, so no allow \
                    line follows.\n--egress=allow-list\n";

/// A put-dir frontend's file renders the ceiling `allow-list` and no destination.
/// Catches: a route left open for a job that has none, or a ceiling other than `allow-list`.
#[test]
fn a_put_dir_frontend_allows_no_destination() {
    let out = render_text(FRONTEND).unwrap();
    assert_eq!(out, HEAD);
    assert!(!out.contains("--egress-allow"), "{out}");
}

/// A file that is not TOML is refused naming the file.
#[test]
fn a_file_that_is_not_toml_is_refused() {
    let err = render_text("[task-runs\n").unwrap_err();
    assert!(
        matches!(&err, EgressError::Syntax { file, .. } if file == FILE),
        "{err:?}"
    );
    assert!(
        err.to_string()
            .starts_with("--config /etc/kbf/kbf.toml: not TOML: "),
        "{err}"
    );
}

/// The verb reads the file it is given and prints the lines (exit 0, nothing on stderr); a
/// file that cannot be read, or one it refuses, is `REFUSED: ...` on stderr with exit 2 and
/// nothing on stdout.
#[test]
fn the_verb_prints_the_lines_or_refuses_with_exit_2() {
    let dir = TestDir::under(std::env::temp_dir(), "kbf-deploy-agent-egress");
    let config = dir.path().join("kbf.toml");
    std::fs::write(&config, FRONTEND).unwrap();
    let ran = |config: &Path| {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = run(
            &AgentEgressArgs {
                config: config.to_path_buf(),
            },
            &mut out,
            &mut err,
        );
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    };

    let (code, out, err) = ran(&config);
    assert_eq!(code, ExitCode::SUCCESS, "{err}");
    assert_eq!(out, render(FRONTEND, &config).unwrap());
    assert_eq!(err, "");

    let missing = dir.path().join("missing.toml");
    let (code, out, err) = ran(&missing);
    assert_eq!(code, ExitCode::from(2));
    assert_eq!(out, "");
    assert_eq!(
        err,
        format!(
            "REFUSED: --config {}: cannot be read: No such file or directory (os error 2)\n",
            missing.display()
        )
    );

    let bad = dir.path().join("bad.toml");
    std::fs::write(&bad, "[task-runs\n").unwrap();
    let (code, out, err) = ran(&bad);
    assert_eq!(code, ExitCode::from(2));
    assert_eq!(out, "");
    assert!(
        err.starts_with(&format!("REFUSED: --config {}: not TOML: ", bad.display())),
        "{err}"
    );
}
