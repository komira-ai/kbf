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

/// A frontend's tables with the task's two places: the proxy and the read listener.
const FRONTEND: &str = r#"mode = "frontend"

[executor]
executor = "js-agent"

[task-runs]
task-put-endpoint = "http://192.0.2.10:9000"
task-put-bucket = "kbf"
task-read-url = "https://192.0.2.20:8444"
"#;

/// The head every rendering starts with.
const HEAD: &str = "# The egress of kbf's tasks, from /etc/kbf/kbf.toml. Put these lines in \
                    /etc/js-agent/flags on\n# every agent of the farm (js-agent --runtime \
                    crun): a job of kbf's reaches these and\n# nothing else.\n\
                    --egress=allow-list\n";

/// The rendered lines name exactly the tier store's proxy and the read listener, under the
/// ceiling `allow-list`, and nothing else.
/// Catches: a destination the task needs left off, one it does not need added, or a ceiling
/// other than `allow-list`.
#[test]
fn a_frontend_with_a_proxy_and_a_read_listener_allows_exactly_those_two() {
    let want = format!(
        "{HEAD}# task-put-endpoint = http://192.0.2.10:9000\n\
         --egress-allow=192.0.2.10:9000\n\
         # task-read-url = https://192.0.2.20:8444\n\
         --egress-allow=192.0.2.20:8444\n"
    );
    assert_eq!(render_text(FRONTEND).unwrap(), want);
}

/// A store named by the removed `job-store-url`, with `executor-locate` on or off, is no
/// destination: the frontend signs no store read grants, so a task never dials it.
/// Catches: a route left open to a store no task reads from.
#[test]
fn a_store_url_left_in_the_file_is_not_allowed() {
    let with_store =
        format!("{FRONTEND}\n[job-read]\njob-store-url = \"http://192.0.2.30:9100\"\n");
    for locate in ["", "executor-locate = true\n", "executor-locate = false\n"] {
        let text = with_store.replace(
            "executor = \"js-agent\"\n",
            &format!("executor = \"js-agent\"\n{locate}"),
        );
        assert_eq!(render_text(&text), render_text(FRONTEND), "{locate}");
    }
}

/// Two keys naming one address and port give one line whose comment names both.
/// Catches: an endpoint listed twice, which the agent would hold against its limit of 16.
#[test]
fn one_endpoint_named_twice_is_allowed_once() {
    let text = FRONTEND.replace("https://192.0.2.20:8444", "https://192.0.2.10:9000");
    assert_eq!(
        render_text(&text).unwrap(),
        format!(
            "{HEAD}# task-put-endpoint = http://192.0.2.10:9000, task-read-url = \
             https://192.0.2.10:9000\n--egress-allow=192.0.2.10:9000\n"
        )
    );
}

/// A url without a port has its scheme's: 80 for `http`, 443 for `https`; a path after the
/// address does not change the endpoint.
#[test]
fn a_url_without_a_port_has_its_schemes_port() {
    let text = FRONTEND
        .replace("http://192.0.2.10:9000", "http://192.0.2.10/")
        .replace("https://192.0.2.20:8444", "https://192.0.2.20/a/b");
    let out = render_text(&text).unwrap();
    assert!(out.contains("\n--egress-allow=192.0.2.10:80\n"), "{out}");
    assert!(out.ends_with("\n--egress-allow=192.0.2.20:443\n"), "{out}");
}

/// A frontend file without the proxy, or without the read listener, is refused naming the
/// missing key.
#[test]
fn a_missing_destination_is_refused_naming_its_key() {
    let no_put = FRONTEND.replace("task-put-endpoint = \"http://192.0.2.10:9000\"\n", "");
    assert_eq!(
        render_text(&no_put).unwrap_err().to_string(),
        "--config /etc/kbf/kbf.toml: no `task-put-endpoint`: a task PUTs its packs there"
    );
    let no_read = FRONTEND.replace("task-read-url = \"https://192.0.2.20:8444\"\n", "");
    assert_eq!(
        render_text(&no_read).unwrap_err().to_string(),
        "--config /etc/kbf/kbf.toml: no `task-read-url`: a task reads its inputs there"
    );
}

/// Each url an agent cannot allow is refused with the key, the url and the reason, and
/// nothing is rendered.
#[test]
fn an_address_an_agent_cannot_allow_is_refused() {
    let cases: &[(&str, &str)] = &[
        ("ftp://192.0.2.10:9000", "not an http:// or https:// url"),
        ("192.0.2.10:9000", "not an http:// or https:// url"),
        (
            "http://[2001:db8::1]:9000",
            "an IPv6 address: a job's network is IPv4 only",
        ),
        (
            "http://store.example:9000",
            "the host is not an IPv4 address: a job's network has no DNS, so the agent allows \
             addresses only",
        ),
        (
            "http://192.0.2.10:x",
            "the port is not a number from 1 to 65535",
        ),
        (
            "http://192.0.2.10:0",
            "the port is not a number from 1 to 65535",
        ),
        (
            "http://192.0.2.10:65536",
            "the port is not a number from 1 to 65535",
        ),
        (
            "http://192.0.2.10:",
            "the port is not a number from 1 to 65535",
        ),
    ];
    let unreachable = "a loopback, unspecified, link-local, multicast or broadcast address, \
                       which no job reaches through the host";
    // Built from their octets: loopback, unspecified, the cloud metadata address,
    // all-hosts multicast and the limited broadcast.
    let addresses = [
        Ipv4Addr::LOCALHOST,
        Ipv4Addr::UNSPECIFIED,
        Ipv4Addr::new(169, 254, 169, 254),
        Ipv4Addr::new(224, 0, 0, 1),
        Ipv4Addr::BROADCAST,
    ];
    let unreachable_cases: Vec<(String, &str)> = addresses
        .iter()
        .map(|a| (format!("http://{a}:9000"), unreachable))
        .collect();
    let all = cases
        .iter()
        .map(|(u, w)| ((*u).to_owned(), *w))
        .chain(unreachable_cases);
    for (url, why) in all {
        let text = FRONTEND.replace("http://192.0.2.10:9000", &url);
        assert_eq!(
            render_text(&text).unwrap_err().to_string(),
            format!("--config /etc/kbf/kbf.toml: `task-put-endpoint` = {url:?}: {why}"),
            "{url}"
        );
    }
}

/// A file that is not TOML, a key of the wrong type, and a key in two tables are refused
/// naming the file and the key.
#[test]
fn a_file_the_frontend_would_refuse_is_refused() {
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

    let number = FRONTEND.replace("\"https://192.0.2.20:8444\"", "8444");
    assert_eq!(
        render_text(&number).unwrap_err().to_string(),
        "--config /etc/kbf/kbf.toml: `task-read-url` is not a string"
    );

    let twice = format!("{FRONTEND}\n[listener]\ntask-read-url = \"https://192.0.2.21:8444\"\n");
    assert_eq!(
        render_text(&twice).unwrap_err().to_string(),
        "--config /etc/kbf/kbf.toml: `task-read-url` is in more than one table ([listener] \
         and [task-runs])"
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
    std::fs::write(&bad, FRONTEND.replace("192.0.2.20", "frontend.example")).unwrap();
    let (code, out, err) = ran(&bad);
    assert_eq!(code, ExitCode::from(2));
    assert_eq!(out, "");
    assert!(
        err.starts_with(&format!(
            "REFUSED: --config {}: `task-read-url` = ",
            bad.display()
        )),
        "{err}"
    );
}
