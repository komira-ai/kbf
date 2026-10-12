use std::cell::RefCell;
use std::io;
use std::rc::Rc;

use super::*;
use kbf_host::step::Step;
use kbf_host::testkit::{Answer, FakeExec, reply};

const KUBECTL: &str = "/usr/local/bin/kubectl";
const KUBECONFIG: &str = "/srv/admin/kubeconfig";
const CACHE: &str = "/srv/scratch/kube";
/// The test config's context, tailnet and servers: none of them the example's.
const CONTEXT: &str = "farm";
const TAILNET: &str = "tail1234.ts.net";
const SERVERS: &str = "kbf-server-b.storage.example:8980";
/// `<KBF_FRONT_HOSTNAME>.<TAILNET>`.
const ADDRESS: &str = "kbf.tail1234.ts.net";
/// The checks and the server's validation, in the order they must run.
const CHECKS: &[&str] = &[
    "ingressclass",
    "operator-secret",
    "namespace",
    "ca-secret",
    "diff",
    "dry-run",
];

fn p(s: &str) -> HostPath {
    HostPath::new(s).expect("valid test path")
}

fn config_text() -> String {
    EXAMPLE
        .replace(
            "KBF_FRONT_KUBE_CONTEXT=example-context",
            &format!("KBF_FRONT_KUBE_CONTEXT={CONTEXT}"),
        )
        .replace(
            "KBF_FRONT_TAILNET_DNS=tail0000.ts.net",
            &format!("KBF_FRONT_TAILNET_DNS={TAILNET}"),
        )
        .replace(
            "KBF_SERVERS=\"kbf-server-a.storage.example:8980\"",
            &format!("KBF_SERVERS=\"{SERVERS}\""),
        )
}

fn args() -> Apply {
    let text = config_text();
    assert_eq!(text.matches(CONTEXT).count(), 1, "the context was replaced");
    assert_eq!(text.matches(TAILNET).count(), 1, "the tailnet was replaced");
    assert_eq!(
        text.matches(SERVERS).count(),
        1,
        "the servers were replaced"
    );
    Apply {
        config: EnvoyConfig::parse(&text).expect("valid config"),
        example: example(None).expect("kbf's example parses"),
        kubectl: p(KUBECTL),
        kubeconfig: p(KUBECONFIG),
        cache_dir: CACHE.to_owned(),
        wait: Wait {
            tries: 3,
            every: Duration::ZERO,
        },
    }
}

/// What the fake cluster holds, and every kubectl call made, by name, in order:
/// queries and applied steps alike, so a test sees the order they really ran in.
struct Cluster {
    ingress_class: bool,
    operator_secret: bool,
    namespace: bool,
    /// What the CA Secret's go-template prints; `None`: there is no such Secret.
    ca: Option<&'static str>,
    /// `kubectl diff` finds no difference (exit 0).
    holds_render: bool,
    diff_fails: bool,
    dry_run_refuses: bool,
    rollout_fails: bool,
    /// What each `get ingress` prints, in turn; the last one again after that. An
    /// entry `None` is a failed `get`.
    addresses: Vec<Option<&'static str>>,
    calls: Vec<&'static str>,
}

fn ready() -> Cluster {
    Cluster {
        ingress_class: true,
        operator_secret: true,
        namespace: true,
        ca: Some("yes"),
        holds_render: false,
        diff_fails: false,
        dry_run_refuses: false,
        rollout_fails: false,
        addresses: vec![Some(ADDRESS)],
        calls: Vec::new(),
    }
}

fn exists(found: bool, what: &str) -> io::Result<Output> {
    Ok(if found {
        reply(0, &format!("{what}\n"), "")
    } else {
        reply(
            1,
            "",
            &format!("Error from server (NotFound): {what} not found\n"),
        )
    })
}

/// `kubectl`, which answers only with an empty environment and the plan's
/// `--kubeconfig`, `--cache-dir` and `--context` first; every `-f -` must carry the
/// render of `a`'s config, and the operator's Secret is found only in `a`'s
/// operator namespace.
fn answer(state: Rc<RefCell<Cluster>>, a: &Apply) -> Answer {
    let manifests = render(&a.config).manifests.into_bytes();
    let operator_namespace = a.config.operator_namespace.clone();
    Box::new(move |cmd: &Cmd| {
        let mut c = state.borrow_mut();
        assert_eq!(cmd.program.as_str(), KUBECTL);
        assert!(cmd.env.is_empty(), "kubectl gets no environment: {cmd}");
        let args: Vec<&str> = cmd.args.iter().map(String::as_str).collect();
        let prefix = [
            "--kubeconfig",
            KUBECONFIG,
            "--cache-dir",
            CACHE,
            "--context",
            CONTEXT,
        ];
        let Some(rest) = args.strip_prefix(&prefix[..]) else {
            return Ok(reply(1, "", "error: no context farm in this command\n"));
        };
        if rest.ends_with(&["-f", "-"]) {
            assert!(cmd.stdin == manifests, "-f - gets the render: {cmd}");
        }
        let manager = "--field-manager=kbf-front";
        match rest {
            ["get", "ingressclass", "tailscale", "-o", "name"] => {
                c.calls.push("ingressclass");
                exists(c.ingress_class, "ingressclass.networking.k8s.io/tailscale")
            }
            ["-n", opns, "get", "secret", "operator-oauth", "-o", "name"]
                if *opns == operator_namespace =>
            {
                c.calls.push("operator-secret");
                exists(c.operator_secret, "secret/operator-oauth")
            }
            ["get", "namespace", "kbf-front", "-o", "name"] => {
                c.calls.push("namespace");
                exists(c.namespace, "namespace/kbf-front")
            }
            [
                "-n",
                "kbf-front",
                "get",
                "secret",
                "kbf-front-ca",
                "-o",
                template,
            ] => {
                assert_eq!(
                    *template,
                    r#"go-template={{if index .data "ca.pem"}}yes{{end}}"#
                );
                c.calls.push("ca-secret");
                Ok(match c.ca {
                    Some(printed) => reply(0, printed, ""),
                    None => reply(1, "", "Error from server (NotFound): secrets not found\n"),
                })
            }
            ["diff", "--server-side", m, "-f", "-"] if *m == manager => {
                c.calls.push("diff");
                Ok(if c.diff_fails {
                    reply(2, "", "error: unable to recognize\n")
                } else if c.holds_render {
                    reply(0, "", "")
                } else {
                    reply(1, "+  replicas: 1\n", "")
                })
            }
            ["apply", "--server-side", m, "--dry-run=server", "-f", "-"] if *m == manager => {
                c.calls.push("dry-run");
                Ok(if c.dry_run_refuses {
                    reply(1, "", "Error from server (Invalid): spec.replicas\n")
                } else {
                    reply(
                        0,
                        "deployment.apps/kbf-envoy serverside-applied (server dry run)\n",
                        "",
                    )
                })
            }
            ["apply", "--server-side", m, "-f", "-"] if *m == manager => {
                c.calls.push("apply");
                c.holds_render = true;
                Ok(reply(
                    0,
                    "deployment.apps/kbf-envoy serverside-applied\n",
                    "",
                ))
            }
            [
                "-n",
                "kbf-front",
                "rollout",
                "status",
                "deployment/kbf-envoy",
                "--timeout=180s",
            ] => {
                c.calls.push("rollout");
                Ok(if c.rollout_fails {
                    reply(1, "", "error: timed out waiting for the condition\n")
                } else {
                    reply(0, "deployment \"kbf-envoy\" successfully rolled out\n", "")
                })
            }
            [
                "-n",
                "kbf-front",
                "get",
                "ingress",
                "kbf",
                "-o",
                "jsonpath={.status.loadBalancer.ingress[0].hostname}",
            ] => {
                c.calls.push("ingress");
                let now = if c.addresses.len() > 1 {
                    c.addresses.remove(0)
                } else {
                    c.addresses[0]
                };
                Ok(match now {
                    Some(address) => reply(0, address, ""),
                    None => reply(1, "", "Error from server (NotFound): ingresses not found\n"),
                })
            }
            _ => Ok(reply(2, "", "unexpected kubectl command\n")),
        }
    })
}

fn host(cluster: Cluster) -> (FakeExec, Rc<RefCell<Cluster>>) {
    host_for(cluster, &args())
}

/// `host`, with a kubectl that expects `a`'s render and operator namespace.
fn host_for(cluster: Cluster, a: &Apply) -> (FakeExec, Rc<RefCell<Cluster>>) {
    let state = Rc::new(RefCell::new(cluster));
    let mut exec = FakeExec::new(501);
    exec.answer = answer(Rc::clone(&state), a);
    (exec, state)
}

fn go(exec: &mut FakeExec, args: &Apply, mode: Mode) -> (Result<(), ApplyError>, String) {
    let mut out = Vec::new();
    let done = run(exec, args, mode, &mut out);
    (done, String::from_utf8(out).expect("utf-8"))
}

fn calls(state: &Rc<RefCell<Cluster>>) -> Vec<&'static str> {
    state.borrow().calls.clone()
}

const DRY_RUN: &str = "\
IngressClass tailscale: present
Secret tailscale/operator-oauth: present
namespace kbf-front: present
Secret kbf-front/kbf-front-ca: present, with ca.pem
kubectl diff, what --apply changes:
+  replicas: 1
kubectl apply --dry-run=server: accepted
DRY-RUN: run /usr/local/bin/kubectl --kubeconfig /srv/admin/kubeconfig --cache-dir /srv/scratch/kube --context farm apply --server-side --field-manager=kbf-front -f - (stdin BYTES bytes)
DRY-RUN: run /usr/local/bin/kubectl --kubeconfig /srv/admin/kubeconfig --cache-dir /srv/scratch/kube --context farm -n kbf-front rollout status deployment/kbf-envoy --timeout=180s
dry run: nothing changed. Re-run with --apply.
";

/// Catches a dry run that applies or waits for the Ingress, a check or the server's
/// validation dropped or run out of order, a rollout not planned, and printed lines
/// lost. Every kubectl call names the config's context (the fake answers no other).
#[test]
fn a_dry_run_reads_the_cluster_and_plans_the_apply_and_the_rollout() {
    let (mut exec, state) = host(ready());
    let a = args();
    let (done, out) = go(&mut exec, &a, Mode::DryRun);
    done.expect("a dry run on a ready cluster passes");
    assert_eq!(calls(&state), CHECKS);
    assert!(
        exec.log.is_empty(),
        "a dry run applies nothing: {:?}",
        exec.log
    );
    let bytes = render(&a.config).manifests.len().to_string();
    assert_eq!(out, DRY_RUN.replace("BYTES", &bytes));
}

/// The plan's order: catches the apply (or the rollout) run before any check or before
/// the server-side dry run passed, the rollout before the apply, and the Ingress
/// asked for before Envoy rolled out.
#[test]
fn apply_runs_only_after_every_check_and_the_server_dry_run() {
    let (mut exec, state) = host(ready());
    let (done, out) = go(&mut exec, &args(), Mode::Apply);
    done.expect("an apply on a ready cluster passes");
    let mut want = CHECKS.to_vec();
    want.extend(["apply", "rollout", "ingress"]);
    assert_eq!(calls(&state), want);
    assert_eq!(exec.log.len(), 2, "{:?}", exec.log);
    assert!(
        matches!(&exec.log[0], Step::Run(c) if c.args.contains(&"apply".to_owned())),
        "{:?}",
        exec.log
    );
    assert!(
        out.ends_with("the front is up: clients dial grpcs://kbf.tail1234.ts.net:443\n"),
        "{out}"
    );
}

/// Catches a check whose failure is ignored, an apply (or diff, which reads the
/// render against the cluster) that runs before the checks, and a refusal that does
/// not say what is missing. Each row is an `--apply` run, which must stop before the
/// diff and apply nothing.
#[test]
fn a_missing_prerequisite_refuses_before_anything_is_applied() {
    type Plant = fn(&mut Cluster);
    let rows: [(&str, Plant, &str, usize); 5] = [
        (
            "no IngressClass",
            |c| c.ingress_class = false,
            "no IngressClass tailscale (the Tailscale operator is not installed); nothing was applied. kubectl: Error from server (NotFound): ingressclass.networking.k8s.io/tailscale not found",
            1,
        ),
        (
            "no operator Secret",
            |c| c.operator_secret = false,
            "no Secret tailscale/operator-oauth (the operator's OAuth client); nothing was applied. kubectl: Error from server (NotFound): secret/operator-oauth not found",
            2,
        ),
        (
            "no namespace",
            |c| c.namespace = false,
            "no namespace kbf-front (the front's namespace); nothing was applied. kubectl: Error from server (NotFound): namespace/kbf-front not found",
            3,
        ),
        (
            "no CA Secret",
            |c| c.ca = None,
            "no Secret kbf-front/kbf-front-ca (the servers' CA); nothing was applied. kubectl: Error from server (NotFound): secrets not found",
            4,
        ),
        (
            "a CA Secret without ca.pem",
            |c| c.ca = Some(""),
            "Secret kbf-front/kbf-front-ca has no key ca.pem (the CA the servers' certificates chain to); nothing was applied",
            4,
        ),
    ];
    for (name, plant, message, asked) in rows {
        let mut cluster = ready();
        plant(&mut cluster);
        let (mut exec, state) = host(cluster);
        let (done, _) = go(&mut exec, &args(), Mode::Apply);
        let e = done.expect_err(name);
        assert_eq!(e.to_string(), message, "{name}");
        assert_eq!(e.exit_code(), 2, "{name}");
        assert_eq!(calls(&state), CHECKS[..asked], "{name}");
        assert!(exec.log.is_empty(), "{name}: {:?}", exec.log);
    }
}

/// Catches the server-side dry run's answer ignored: the apply would then send
/// objects the API server already said it refuses.
#[test]
fn a_refused_server_dry_run_applies_nothing() {
    let mut cluster = ready();
    cluster.dry_run_refuses = true;
    let (mut exec, state) = host(cluster);
    let (done, _) = go(&mut exec, &args(), Mode::Apply);
    let e = done.expect_err("refused");
    assert_eq!(
        e.to_string(),
        "the server-side dry run refused the objects; nothing was applied: Error from server (Invalid): spec.replicas"
    );
    assert_eq!(e.exit_code(), 2);
    assert_eq!(calls(&state), CHECKS);
    assert!(exec.log.is_empty(), "{:?}", exec.log);
}

/// Catches a diff that errored (exit 2) read as "no change" or as "a change".
#[test]
fn a_failed_diff_refuses() {
    let mut cluster = ready();
    cluster.diff_fails = true;
    let (mut exec, state) = host(cluster);
    let (done, _) = go(&mut exec, &args(), Mode::Apply);
    let e = done.expect_err("refused");
    assert_eq!(
        e.to_string(),
        "kubectl diff failed (exit status: 2); nothing was applied: error: unable to recognize"
    );
    assert_eq!(e.exit_code(), 2);
    assert_eq!(calls(&state), CHECKS[..5]);
    assert!(exec.log.is_empty(), "{:?}", exec.log);
}

/// Catches a re-run that applies again (and restarts nothing but still rolls out):
/// once the cluster holds the render, the plan is empty, and `--apply` only checks
/// the Ingress's address.
#[test]
fn a_re_run_after_an_apply_plans_nothing() {
    let (mut exec, state) = host(ready());
    let a = args();
    go(&mut exec, &a, Mode::Apply).0.expect("first apply");
    state.borrow_mut().calls.clear();
    let (done, out) = go(&mut exec, &a, Mode::Apply);
    done.expect("second apply");
    let mut want = CHECKS.to_vec();
    want.push("ingress");
    assert_eq!(calls(&state), want);
    assert_eq!(
        exec.log.len(),
        2,
        "only the first run's steps: {:?}",
        exec.log
    );
    assert!(
        out.contains("kubectl diff: the cluster holds the render\n"),
        "{out}"
    );
    assert!(out.contains("\nnothing to do\n"), "{out}");
}

/// Catches a config that still has the example's context, tailnet or servers being
/// applied (to whatever cluster that context names), and the refusal coming after
/// kubectl has run.
#[test]
fn the_example_settings_are_refused_before_kubectl_runs() {
    let rows = [
        (
            "KBF_FRONT_KUBE_CONTEXT",
            format!("KBF_FRONT_KUBE_CONTEXT={CONTEXT}\n"),
            "KBF_FRONT_KUBE_CONTEXT=example-context\n",
        ),
        (
            "KBF_FRONT_TAILNET_DNS",
            format!("KBF_FRONT_TAILNET_DNS={TAILNET}\n"),
            "KBF_FRONT_TAILNET_DNS=tail0000.ts.net\n",
        ),
        (
            "KBF_SERVERS",
            format!("KBF_SERVERS=\"{SERVERS}\"\n"),
            "KBF_SERVERS=\"kbf-server-a.storage.example:8980\"\n",
        ),
    ];
    for (key, ours, example_line) in rows {
        let text = config_text().replace(&ours, example_line);
        let mut a = args();
        a.config = EnvoyConfig::parse(&text).expect("valid config");
        let (mut exec, state) = host(ready());
        let (done, out) = go(&mut exec, &a, Mode::Apply);
        let e = done.expect_err(key);
        assert_eq!(
            e.to_string(),
            format!("{key} is still the example config's value: fill it in")
        );
        assert_eq!(e.exit_code(), 2);
        assert_eq!((calls(&state).len(), out.as_str()), (0, ""), "{key}");
    }
}

/// Catches a render that fails `envoy check` being applied anyway, or refused without
/// its `FAIL` line. The render here is the real one with one defect planted, since
/// `render` itself passes every check.
#[test]
fn a_render_that_fails_a_check_is_refused_with_its_fail_line() {
    let manifests = render(&args().config).manifests;
    let class = "\"ingressClassName\": \"tailscale\"";
    assert_eq!(manifests.matches(class).count(), 1);
    let planted = manifests.replace(class, "\"ingressClassName\": \"nginx\"");
    let mut out = Vec::new();
    let e = check_render(&planted, "tailscale", &mut out).expect_err("refused");
    assert_eq!(
        String::from_utf8(out).expect("utf-8"),
        "FAIL ingress-class: ingressClassName nginx is not the tailscale IngressClass\n"
    );
    assert_eq!(
        e.to_string(),
        "the render fails 1 check(s), each on a FAIL line; nothing was applied"
    );
    assert_eq!(e.exit_code(), 2);

    let mut out = Vec::new();
    check_render(&manifests, "tailscale", &mut out).expect("the real render passes");
    assert!(out.is_empty());
    let e = check_render("{", "tailscale", &mut out).expect_err("not JSON");
    assert!(e.to_string().starts_with("the render is not JSON: "), "{e}");
}

/// Catches the address check passing on any address, on none, or before the operator
/// has given one; a trailing `.` refused; and a failed `get` (the Ingress not shown
/// yet) read as a wrong address. Each of those exits 1.
#[test]
fn the_ingress_must_get_the_configs_address() {
    /// What each `get ingress` prints, the error expected, and how many were asked.
    type Row = (Vec<Option<&'static str>>, Result<(), &'static str>, usize);
    let rows: [Row; 5] = [
        (
            vec![None, Some(""), Some("kbf.tail1234.ts.net.")],
            Ok(()),
            3,
        ),
        (vec![Some("\n"), Some(ADDRESS)], Ok(()), 2),
        (
            vec![Some("kbf-1.tail1234.ts.net")],
            Err(
                "the Ingress's address is kbf-1.tail1234.ts.net, the config expects kbf.tail1234.ts.net (another device holds the name, or KBF_FRONT_TAILNET_DNS is wrong)",
            ),
            1,
        ),
        (
            vec![Some("")],
            Err(
                "the Ingress kbf-front/kbf has no address after 0 s (kubectl -n kbf-front describe ingress kbf, and the operator's logs)",
            ),
            3,
        ),
        (
            vec![None],
            Err(
                "the Ingress kbf-front/kbf has no address after 0 s (kubectl -n kbf-front describe ingress kbf, and the operator's logs)",
            ),
            3,
        ),
    ];
    for (addresses, want, asked) in rows {
        let mut cluster = ready();
        cluster.addresses = addresses.clone();
        let (mut exec, state) = host(cluster);
        let (done, _) = go(&mut exec, &args(), Mode::Apply);
        match (done, want) {
            (Ok(()), Ok(())) => {}
            (Err(e), Err(message)) => {
                assert_eq!(e.to_string(), message, "{addresses:?}");
                assert_eq!(e.exit_code(), 1, "{addresses:?}");
            }
            (done, want) => panic!("{addresses:?}: {done:?}, wanted {want:?}"),
        }
        let asked_for = calls(&state).iter().filter(|c| **c == "ingress").count();
        assert_eq!(asked_for, asked, "{addresses:?}");
    }
}

/// Catches the wait's bound not reported in seconds (tries times the pause).
#[test]
fn the_default_wait_is_120_seconds() {
    assert_eq!(Wait::DEFAULT.tries, 60);
    assert_eq!(Wait::DEFAULT.every, Duration::from_secs(2));
    let mut a = args();
    a.wait = Wait {
        tries: 2,
        every: Duration::from_millis(1500),
    };
    let mut cluster = ready();
    cluster.addresses = vec![Some("")];
    let (mut exec, _) = host(cluster);
    let e = go(&mut exec, &a, Mode::Apply).0.expect_err("no address");
    assert!(e.to_string().contains(" has no address after 2 s "), "{e}");
}

/// Catches an apply as root (kubectl would then read root's files) and a rollout that
/// failed being reported as done or as refused: a failed step exits 1.
#[test]
fn root_is_refused_and_a_failed_rollout_exits_1() {
    let (mut exec, state) = host(ready());
    exec.euid = 0;
    let e = go(&mut exec, &args(), Mode::Apply).0.expect_err("root");
    assert!(
        matches!(e, ApplyError::Host(kbf_host::HostError::IsRoot)),
        "{e}"
    );
    assert_eq!(e.exit_code(), 2);
    assert!(exec.log.is_empty(), "{:?}", exec.log);
    assert_eq!(calls(&state), CHECKS);

    let mut cluster = ready();
    cluster.rollout_fails = true;
    let (mut exec, state) = host(cluster);
    let e = go(&mut exec, &args(), Mode::Apply).0.expect_err("rollout");
    assert!(e.to_string().starts_with("step 1 (run "), "{e}");
    assert_eq!(e.exit_code(), 1);
    assert!(!calls(&state).contains(&"ingress"));
}

/// Catches a kubectl that cannot start being read as a missing prerequisite, or its
/// error losing the command.
#[test]
fn a_kubectl_that_cannot_start_is_named() {
    let mut exec = FakeExec::new(501);
    exec.answer = Box::new(|_| Err(io::Error::from(io::ErrorKind::NotFound)));
    let e = go(&mut exec, &args(), Mode::DryRun)
        .0
        .expect_err("no kubectl");
    assert_eq!(
        e.to_string(),
        "/usr/local/bin/kubectl --kubeconfig /srv/admin/kubeconfig --cache-dir /srv/scratch/kube --context farm get ingressclass tailscale -o name: entity not found"
    );
    assert_eq!(e.exit_code(), 2);
}

/// Catches `--refuse-example FILE` ignored for kbf's example, or a file that cannot be
/// read accepted.
#[test]
fn the_example_is_the_given_file_or_kbfs() {
    let dir = kbf_host::testkit::scratch("envoy-apply-example");
    let file = dir.join("config.example");
    std::fs::write(&file, config_text()).expect("write");
    let given = example(Some(&file)).expect("the file parses");
    assert_eq!(given.kube_context, CONTEXT);
    assert_eq!(
        example(None).expect("kbf's").kube_context,
        "example-context"
    );
    let e = example(Some(&dir.join("absent"))).expect_err("absent");
    assert!(e.to_string().contains("absent"), "{e}");
}

/// Catches the config's `KBF_TS_OPERATOR_NAMESPACE` not reaching the render check
/// (a hard-coded `tailscale` there refuses this render, whose NetworkPolicy admits
/// `ts-operator`) or the operator Secret's lookup: every other test uses the
/// example's `tailscale`, where a literal and the config agree.
#[test]
fn the_configs_operator_namespace_reaches_the_checks() {
    let text = config_text().replace(
        "KBF_TS_OPERATOR_NAMESPACE=tailscale",
        "KBF_TS_OPERATOR_NAMESPACE=ts-operator",
    );
    let mut a = args();
    a.config = EnvoyConfig::parse(&text).expect("valid config");
    assert_eq!(a.config.operator_namespace, "ts-operator", "replaced");
    let (mut exec, state) = host_for(ready(), &a);
    let (done, out) = go(&mut exec, &a, Mode::Apply);
    done.expect("an apply with operator namespace ts-operator passes");
    let mut want = CHECKS.to_vec();
    want.extend(["apply", "rollout", "ingress"]);
    assert_eq!(calls(&state), want);
    assert!(
        out.contains("Secret ts-operator/operator-oauth: present\n"),
        "{out}"
    );
}
