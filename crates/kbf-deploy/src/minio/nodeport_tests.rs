use std::cell::RefCell;
use std::rc::Rc;

use serde_json::{Value, json};

use super::*;
use crate::plan::{Mode, RunAs, execute};
use crate::step::Step;
use crate::testkit::{Answer, FakeExec, reply};

const KUBECTL: &str = "/usr/local/bin/kubectl";
const KUBECONFIG: &str = "/srv/admin/kubeconfig";
const CACHE: &str = "/srv/scratch/kube";

fn p(s: &str) -> HostPath {
    HostPath::new(s).expect("valid test path")
}

fn args() -> NodePort {
    NodePort {
        kubectl: p(KUBECTL),
        kubeconfig: p(KUBECONFIG),
        context: None,
        cache_dir: CACHE.to_owned(),
        namespace: K8sName::new("minio").expect("name"),
        service: K8sName::new("minio-hdd").expect("name"),
        node_port: 30900,
        remove: false,
    }
}

/// The MinIO Service, as `kubectl get -o json` prints it.
fn minio() -> Value {
    json!({
        "apiVersion": "v1", "kind": "Service",
        "metadata": {"name": "minio-hdd", "namespace": "minio", "uid": "u-1"},
        "spec": {
            "type": "ClusterIP", "clusterIP": "192.0.2.9",
            "selector": {"app": "minio-hdd", "tier": "storage"},
            "ports": [
                {"name": "api", "port": 9001, "targetPort": 9001, "protocol": "TCP"},
                {"name": "s3", "port": 9000, "targetPort": 9000, "protocol": "TCP"}
            ]
        }
    })
}

/// What the fake cluster holds.
struct Cluster {
    services: Vec<Value>,
    dry_run_refuses: bool,
}

/// `kubectl`, which answers only with the plan's `--kubeconfig` and `--cache-dir`
/// first (and `--context farm` after them, when given).
fn answer(state: Rc<RefCell<Cluster>>) -> Answer {
    Box::new(move |cmd: &Cmd| {
        let mut c = state.borrow_mut();
        let args: Vec<&str> = cmd.args.iter().map(String::as_str).collect();
        let Some(rest) = args.strip_prefix(&["--kubeconfig", KUBECONFIG, "--cache-dir", CACHE][..])
        else {
            return Ok(reply(1, "", "error: no configuration has been provided\n"));
        };
        let rest = match rest {
            ["--context", "farm", rest @ ..] => rest,
            rest => rest,
        };
        let named = |ns: &str, name: &str| {
            c.services
                .iter()
                .position(|s| s["metadata"]["namespace"] == ns && s["metadata"]["name"] == name)
        };
        match rest {
            ["get", "services", "-A", "-o", "json"] => {
                Ok(reply(0, &json!({"items": c.services}).to_string(), ""))
            }
            ["-n", ns, "get", "service", name, "-o", "json"] => match named(ns, name) {
                Some(i) => Ok(reply(0, &c.services[i].to_string(), "")),
                None => Ok(reply(1, "", "Error from server (NotFound)\n")),
            },
            ["apply", "--dry-run=server", "-f", "-"] => {
                serde_json::from_slice::<Value>(&cmd.stdin).expect("a manifest");
                if c.dry_run_refuses {
                    Ok(reply(1, "", "Error from server (Invalid): nodePort\n"))
                } else {
                    Ok(reply(
                        0,
                        "service/minio-hdd-kbf-np created (server dry run)\n",
                        "",
                    ))
                }
            }
            ["apply", "-f", "-"] => {
                let mut svc: Value = serde_json::from_slice(&cmd.stdin).expect("a manifest");
                svc["spec"]["clusterIP"] = "192.0.2.10".into();
                let ns = svc["metadata"]["namespace"]
                    .as_str()
                    .expect("ns")
                    .to_owned();
                let name = svc["metadata"]["name"].as_str().expect("name").to_owned();
                match named(&ns, &name) {
                    Some(i) => c.services[i] = svc,
                    None => c.services.push(svc),
                }
                Ok(reply(0, "service/minio-hdd-kbf-np configured\n", ""))
            }
            ["-n", ns, "delete", "service", name] => {
                let i = named(ns, name).expect("deletes only what exists");
                c.services.remove(i);
                Ok(reply(0, "", ""))
            }
            _ => Ok(reply(2, "", "unexpected kubectl command\n")),
        }
    })
}

fn host(services: Vec<Value>) -> (FakeExec, Rc<RefCell<Cluster>>) {
    let state = Rc::new(RefCell::new(Cluster {
        services,
        dry_run_refuses: false,
    }));
    let mut exec = FakeExec::new(501);
    exec.answer = answer(Rc::clone(&state));
    (exec, state)
}

fn run(exec: &mut FakeExec, args: &NodePort, mode: Mode) -> Result<String, MinioError> {
    let mut out = Vec::new();
    let plan = plan(&*exec, args, &mut out)?;
    execute(&plan, RunAs::User, mode, exec, &mut out)?;
    Ok(String::from_utf8(out).expect("utf-8"))
}

/// The Service this kit adds for `args()`, as the manifest prints it.
const MANIFEST: &str = r#"{
  "apiVersion": "v1",
  "kind": "Service",
  "metadata": {
    "name": "minio-hdd-kbf-np",
    "namespace": "minio",
    "labels": {
      "komira.dev/purpose": "kbf-pilot"
    },
    "annotations": {
      "komira.dev/why": "kbf-deploy minio nodeport: MinIO's S3 API (port 9000 only) for hosts off the cluster. Remove with kbf-deploy minio nodeport --remove --apply."
    }
  },
  "spec": {
    "type": "NodePort",
    "selector": {
      "app": "minio-hdd",
      "tier": "storage"
    },
    "ports": [
      {
        "name": "s3",
        "protocol": "TCP",
        "port": 9000,
        "targetPort": 9000,
        "nodePort": 30900
      }
    ]
  }
}"#;

/// Catches a manifest that opens the console port or another port, loses the MinIO
/// Service's selector (the NodePort would reach no pod, or the wrong ones), or is
/// applied without the server's dry run first; and a re-run that applies again.
#[test]
fn a_missing_service_is_applied_after_a_server_dry_run_and_kept_after() {
    let (mut exec, state) = host(vec![minio()]);
    let prefix = format!("{KUBECTL} --kubeconfig {KUBECONFIG} --cache-dir {CACHE}");
    let dry = run(&mut exec, &args(), Mode::DryRun).expect("dry run");
    assert_eq!(
        dry,
        format!(
            "Service minio/minio-hdd-kbf-np, the manifest applied:\n{MANIFEST}\n\
             kubectl apply --dry-run=server: accepted\n\
             DRY-RUN: run {prefix} apply -f - (stdin {} bytes)\n\
             dry run: nothing changed. Re-run with --apply.\n",
            MANIFEST.len()
        )
    );
    let queries: Vec<String> = exec
        .queries
        .borrow()
        .iter()
        .map(|c| c.args[4..].join(" "))
        .collect();
    assert_eq!(
        queries,
        [
            "get services -A -o json",
            "-n minio get service minio-hdd -o json",
            "apply --dry-run=server -f -",
        ]
    );
    assert_eq!(
        state.borrow().services.len(),
        1,
        "a dry run applied the Service"
    );

    run(&mut exec, &args(), Mode::Apply).expect("apply");
    assert_eq!(state.borrow().services.len(), 2);
    let again = run(&mut exec, &args(), Mode::DryRun).expect("re-run");
    assert_eq!(
        again,
        "Service minio/minio-hdd-kbf-np: as wanted, kept\nnothing to do\n"
    );
}

/// Catches a kept Service that no longer matches: another selector (the MinIO pods
/// were relabelled), or one without the label `--remove` looks for.
#[test]
fn a_service_that_differs_is_applied_again() {
    let mut wanted: Value = serde_json::from_str(MANIFEST).expect("json");
    wanted["spec"]["selector"] = json!({"app": "minio-old"});
    let mut unlabelled: Value = serde_json::from_str(MANIFEST).expect("json");
    unlabelled["metadata"]["labels"] = json!({});
    for ours in [wanted, unlabelled] {
        let (mut exec, state) = host(vec![minio(), ours]);
        let dry = run(&mut exec, &args(), Mode::DryRun).expect("dry run");
        assert!(dry.contains("DRY-RUN: run "), "{dry}");
        run(&mut exec, &args(), Mode::Apply).expect("apply");
        let got = state.borrow().services[1].clone();
        assert_eq!(
            got["spec"]["selector"],
            json!({"app": "minio-hdd", "tier": "storage"})
        );
        assert_eq!(
            got["metadata"]["labels"],
            json!({"komira.dev/purpose": "kbf-pilot"})
        );
    }
}

/// Another NodePort Service, holding `port`.
fn other(ns: &str, name: &str, port: u16) -> Value {
    json!({
        "metadata": {"name": name, "namespace": ns},
        "spec": {"type": "NodePort", "ports": [{"port": 80, "nodePort": port}]}
    })
}

/// Catches a check that is skipped or too loose: a MinIO Service whose S3 port is not
/// named `s3` at 9000 (the NodePort would forward to the wrong port), one with no
/// selector (it would select nothing), a node port another Service holds (the apply
/// would fail or take it over), a server dry run that refused, and kubectl answers
/// that fail or are not JSON.
#[test]
fn each_refusal_stops_the_plan() {
    type Setup = fn(&mut Cluster, &mut FakeExec);
    let prefix = format!("{KUBECTL} --kubeconfig {KUBECONFIG} --cache-dir {CACHE}");
    let failed = format!("{KUBECTL} exited with exit status: 1");
    let cases: [(&str, Setup, String); 7] = [
        (
            "port 9001",
            |c, _| c.services[0]["spec"]["ports"][1]["port"] = 9001.into(),
            "Service minio/minio-hdd has no port named s3 at port 9000".to_owned(),
        ),
        (
            "unnamed",
            |c, _| c.services[0]["spec"]["ports"][1]["name"] = "web".into(),
            "Service minio/minio-hdd has no port named s3 at port 9000".to_owned(),
        ),
        (
            "selector",
            |c, _| c.services[0]["spec"]["selector"] = json!({}),
            "Service minio/minio-hdd has no selector".to_owned(),
        ),
        (
            "held",
            |c, _| c.services.push(other("farm", "reapi", 30900)),
            "nodePort 30900 is held by farm/reapi: pick another with --node-port".to_owned(),
        ),
        (
            "dry run",
            |c, _| c.dry_run_refuses = true,
            format!(
                "{prefix} apply --dry-run=server -f - (stdin {} bytes): {failed}: \
                 Error from server (Invalid): nodePort",
                MANIFEST.len()
            ),
        ),
        (
            "missing",
            |c, _| drop(c.services.remove(0)),
            format!(
                "{prefix} -n minio get service minio-hdd -o json: {failed}: \
                 Error from server (NotFound)"
            ),
        ),
        (
            "not json",
            |_, e| e.answer = Box::new(|_| Ok(reply(0, "No resources found", ""))),
            format!(
                "{prefix} get services -A -o json printed no JSON this kit reads: \
                 expected value at line 1 column 1"
            ),
        ),
    ];
    for (what, setup, want) in cases {
        let (mut exec, state) = host(vec![minio(), other("farm", "web", 30080)]);
        setup(&mut state.borrow_mut(), &mut exec);
        let err = run(&mut exec, &args(), Mode::DryRun).expect_err(what);
        assert_eq!(err.to_string(), want, "{what}");
    }
}

/// Catches a rollback that deletes a Service this kit did not make (one of that name
/// without the label), or plans a delete of nothing (the apply would fail).
#[test]
fn remove_deletes_only_the_labelled_service() {
    let rollback = NodePort {
        remove: true,
        ..args()
    };
    let (mut exec, state) = host(vec![minio()]);
    assert_eq!(
        run(&mut exec, &rollback, Mode::DryRun).expect("nothing there"),
        "no Service minio/minio-hdd-kbf-np\nnothing to do\n"
    );
    run(&mut exec, &args(), Mode::Apply).expect("apply");
    let dry = run(&mut exec, &rollback, Mode::DryRun).expect("dry run");
    assert_eq!(
        dry,
        format!(
            "DRY-RUN: run {KUBECTL} --kubeconfig {KUBECONFIG} --cache-dir {CACHE} -n minio \
             delete service minio-hdd-kbf-np\n\
             dry run: nothing changed. Re-run with --apply.\n"
        )
    );
    run(&mut exec, &rollback, Mode::Apply).expect("remove");
    assert_eq!(state.borrow().services, vec![minio()]);

    let mut foreign: Value = serde_json::from_str(MANIFEST).expect("json");
    foreign["metadata"]["labels"] = json!({"app": "someone-else"});
    let (mut exec, _state) = host(vec![minio(), foreign]);
    let err = run(&mut exec, &rollback, Mode::DryRun).expect_err("not ours");
    assert_eq!(
        err.to_string(),
        "Service minio/minio-hdd-kbf-np does not carry komira.dev/purpose=kbf-pilot: left alone"
    );
}

/// Catches `--context` dropped (kubectl would act on the kubeconfig's current
/// context, possibly another cluster) or put before `--kubeconfig`.
#[test]
fn a_context_follows_the_kubeconfig_on_every_command() {
    let farm = NodePort {
        context: Some("farm".to_owned()),
        ..args()
    };
    let (mut exec, _state) = host(vec![minio()]);
    run(&mut exec, &farm, Mode::Apply).expect("apply");
    let queries = exec.queries.borrow().clone();
    assert_eq!(queries.len(), 3);
    for cmd in queries {
        assert_eq!(cmd.args[4..6], ["--context", "farm"], "{cmd}");
    }
    let apply = farm.kubectl(&["apply", "-f", "-"]).stdin(MANIFEST.into());
    assert_eq!(exec.log, [Step::Run(apply)]);
}

/// Catches a fake cluster that answers a command it does not know, or one without the
/// kubeconfig, as a success: a typo in the plan's argv would then pass these tests.
#[test]
fn the_fake_cluster_refuses_what_it_does_not_know() {
    let (exec, _state) = host(vec![]);
    let bare = (exec.answer)(&Cmd::new(p(KUBECTL)).arg("get")).expect("answered");
    assert_eq!(bare.status.code(), Some(1));
    let typo = args().kubectl(&["gte", "services"]);
    assert_eq!(
        (exec.answer)(&typo).expect("answered").status.code(),
        Some(2)
    );
}
