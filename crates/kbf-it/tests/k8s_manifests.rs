//! `deploy/k8s/` against the rules its comments state: the manifests are read as they
//! are (`kubectl` is not run: CI has no cluster and no kustomize binary), and what must
//! hold of the frontend and its permissions is checked on the parsed objects.
//!
//! Catches:
//! - a `kustomization.yaml` that lists a file that is missing, or leaves a manifest out;
//! - an image named by tag, or by anything but a 64-digit sha256 digest;
//! - a frontend Deployment with other than one replica (SQLite has one writer), a
//!   `RollingUpdate` that is not `maxUnavailable: 0` with `maxSurge: 1`, a grace
//!   period under a week, a liveness probe or a `preStop` hook (either one fails a busy
//!   frontend's builds), no readiness or startup probe, no host network, an unequal
//!   `runAsUser` and `fsGroup` (the handover socket checks the peer's uid), requests that
//!   differ from limits, or a state volume that is not a host path;
//! - a Role with a wildcard, a verb besides `get` and `patch`, more than the one
//!   Deployment, or a cluster-wide role or binding;
//! - a service account token mounted into the Envoy pod;
//! - an Envoy pod that cannot read a file its rendered `envoy.json` names (the
//!   certificates and keys): every path in the rendered config, and the config's own path
//!   in the container args, must lie under a read-only mount of a host directory;
//! - a Secret, Service, Ingress or PersistentVolumeClaim, and an IP address or tailnet
//!   name in a public file.
//!
//! The `*_is_refused` tests plant one defect each in the real files and require the check
//! to name it, so a check that stopped looking fails here.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

// Only part of the tree builder's surface is used here.
#[allow(dead_code)]
#[path = "workflows/yaml.rs"]
mod yaml;

use yaml::{Node, Value};

fn dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../deploy/k8s")
}

/// Every top-level `*.yaml` in `deploy/k8s`, by file name.
fn files() -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for entry in std::fs::read_dir(dir()).expect("deploy/k8s") {
        let path = entry.expect("entry").path();
        if path.extension().is_some_and(|x| x == "yaml") {
            let name = path
                .file_name()
                .expect("name")
                .to_string_lossy()
                .into_owned();
            out.insert(name, std::fs::read_to_string(&path).expect("read"));
        }
    }
    out
}

fn get<'a>(n: &'a Node, key: &str) -> Option<&'a Node> {
    match &n.value {
        Value::Map(m) => m
            .iter()
            .find(|(k, _)| k.as_str() == Some(key))
            .map(|(_, v)| v),
        _ => None,
    }
}

/// `a.b.c` through mappings.
fn at<'a>(n: &'a Node, path: &str) -> Option<&'a Node> {
    path.split('.').try_fold(n, |n, key| get(n, key))
}

fn seq(n: &Node) -> &[Node] {
    match &n.value {
        Value::Seq(items) => items,
        _ => &[],
    }
}

fn text<'a>(n: &'a Node, path: &str) -> Option<&'a str> {
    at(n, path).and_then(Node::as_str)
}

fn strings(n: Option<&Node>) -> Vec<&str> {
    n.map(|n| seq(n).iter().filter_map(Node::as_str).collect())
        .unwrap_or_default()
}

fn int(n: &Node, path: &str) -> Option<i64> {
    text(n, path).and_then(|t| t.parse().ok())
}

const WEEK: i64 = 7 * 24 * 3600;

/// What differs from the rules, one line each; empty when none does.
fn problems(files: &BTreeMap<String, String>) -> Vec<String> {
    let mut out = Vec::new();
    let mut docs: BTreeMap<&str, Node> = BTreeMap::new();
    for (name, body) in files {
        match yaml::load(body) {
            Ok(n) => {
                docs.insert(name, n);
            }
            Err(e) => out.push(format!("{name}: {e}")),
        }
    }
    let Some(kustomization) = docs.get("kustomization.yaml") else {
        out.push("no kustomization.yaml".into());
        return out;
    };
    let listed = strings(get(kustomization, "resources"));
    for r in &listed {
        if !files.contains_key(*r) {
            out.push(format!("kustomization.yaml lists {r}, which is not a file"));
        }
    }
    for name in files.keys() {
        if name != "kustomization.yaml" && !listed.contains(&name.as_str()) {
            out.push(format!("{name} is not in kustomization.yaml's resources"));
        }
    }
    images(kustomization, &mut out);

    let mut roles = 0;
    let mut kinds = Vec::new();
    for (name, doc) in &docs {
        if *name == "kustomization.yaml" {
            continue;
        }
        let kind = text(doc, "kind").unwrap_or("");
        kinds.push(kind);
        match kind {
            "Deployment" => match text(doc, "metadata.name") {
                Some("kbf-frontend") => frontend(doc, &mut out),
                Some("kbf-envoy") => envoy(doc, &mut out),
                other => out.push(format!("{name}: a Deployment named {other:?}")),
            },
            "Role" => {
                roles += 1;
                role(doc, &mut out);
            }
            "RoleBinding" => binding(doc, &mut out),
            "Namespace" | "ServiceAccount" => {}
            other => out.push(format!(
                "{name}: kind {other:?} is not one the install knows (no Secret, Service, Ingress, claim or cluster-wide role)"
            )),
        }
    }
    if roles != 1 {
        out.push(format!("{roles} Roles, not one"));
    }
    for name in ["Namespace", "ServiceAccount", "RoleBinding"] {
        if !kinds.contains(&name) {
            out.push(format!("no {name}"));
        }
    }
    for (name, body) in files {
        public_safe(name, body, &mut out);
    }
    out
}

fn images(k: &Node, out: &mut Vec<String>) {
    let list = get(k, "images").map(seq).unwrap_or_default();
    let names: Vec<_> = list.iter().filter_map(|i| text(i, "name")).collect();
    for want in ["kbf-frontend", "envoy"] {
        if !names.contains(&want) {
            out.push(format!("no images entry for {want}"));
        }
    }
    for i in list {
        let name = text(i, "name").unwrap_or("?");
        if get(i, "newTag").is_some() {
            out.push(format!("image {name} is set by tag, not digest"));
        }
        let ok = text(i, "digest")
            .and_then(|d| d.strip_prefix("sha256:"))
            .is_some_and(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()));
        if !ok {
            out.push(format!("image {name} has no sha256:<64 hex> digest"));
        }
    }
}

fn containers(d: &Node) -> &[Node] {
    at(d, "spec.template.spec.containers")
        .map(seq)
        .unwrap_or_default()
}

fn frontend(d: &Node, out: &mut Vec<String>) {
    let mut bad = |m: &str| out.push(format!("kbf-frontend: {m}"));
    if int(d, "spec.replicas") != Some(1) {
        bad("replicas is not 1");
    }
    match text(d, "spec.strategy.type") {
        Some("Recreate") => {}
        Some("RollingUpdate") => {
            if text(d, "spec.strategy.rollingUpdate.maxUnavailable") != Some("0")
                || text(d, "spec.strategy.rollingUpdate.maxSurge") != Some("1")
                || int(d, "spec.minReadySeconds").is_none_or(|s| s < 10)
            {
                bad("RollingUpdate without maxUnavailable 0, maxSurge 1 and minReadySeconds >= 10");
            }
        }
        _ => bad("no strategy type"),
    }
    let pod = |p: &str| text(d, &format!("spec.template.spec.{p}"));
    if int(d, "spec.template.spec.terminationGracePeriodSeconds").is_none_or(|s| s < WEEK) {
        bad("terminationGracePeriodSeconds is under a week");
    }
    if pod("hostNetwork") != Some("true") {
        bad("hostNetwork is not true");
    }
    if pod("dnsPolicy") != Some("ClusterFirstWithHostNet") {
        bad("dnsPolicy is not ClusterFirstWithHostNet");
    }
    if pod("serviceAccountName") != Some("kbf-frontend") {
        bad("serviceAccountName is not kbf-frontend");
    }
    let uid = int(d, "spec.template.spec.securityContext.runAsUser");
    if uid.is_none()
        || uid == Some(0)
        || uid != int(d, "spec.template.spec.securityContext.fsGroup")
    {
        bad("runAsUser is missing, root, or not fsGroup");
    }
    let cs = containers(d);
    if cs.len() != 1 {
        bad("not exactly one container");
    }
    for c in cs {
        for forbidden in ["livenessProbe", "lifecycle"] {
            if get(c, forbidden).is_some() {
                bad(&format!(
                    "has {forbidden}: a busy frontend must not be killed or hooked"
                ));
            }
        }
        for needed in ["readinessProbe", "startupProbe"] {
            if get(c, needed).is_none() {
                bad(&format!("has no {needed}"));
            }
        }
        let args = strings(get(c, "args"));
        if args.first() != Some(&"frontend") {
            bad("args do not start with the mode word `frontend`");
        }
        if !args.contains(&"--config=/etc/kbf/kbf.toml") {
            bad("args lack --config=/etc/kbf/kbf.toml");
        }
        resources(c, "kbf-frontend", out_ref(&mut bad));
    }
    let vols = at(d, "spec.template.spec.volumes")
        .map(seq)
        .unwrap_or_default();
    for v in vols {
        let name = text(v, "name").unwrap_or("?");
        if get(v, "persistentVolumeClaim").is_some() || get(v, "secret").is_some() {
            bad(&format!(
                "volume {name} is a claim or a Secret, not a host path"
            ));
        }
    }
    for need in ["state", "control", "etc-kbf"] {
        let v = vols.iter().find(|v| text(v, "name") == Some(need));
        if v.and_then(|v| text(v, "hostPath.type")) != Some("Directory") {
            bad(&format!(
                "volume {need} is not a hostPath of type Directory"
            ));
        }
    }
    if text(d, "spec.template.spec.automountServiceAccountToken") != Some("true") {
        bad("does not mount its service account token");
    }
}

fn out_ref(f: &mut impl FnMut(&str)) -> &mut dyn FnMut(&str) {
    f
}

fn resources(c: &Node, who: &str, bad: &mut dyn FnMut(&str)) {
    let req = at(c, "resources.requests");
    let lim = at(c, "resources.limits");
    for k in ["cpu", "memory"] {
        let r = req.and_then(|r| text(r, k));
        if r.is_none() || r != lim.and_then(|l| text(l, k)) {
            bad(&format!("{who}: {k} request is missing or not the limit"));
        }
    }
}

/// Every absolute path the rendered Envoy configuration names (`"filename": "..."`):
/// the golden `kbf-installer envoy render` output for the single-box topology, which is
/// what the pod's host directory holds.
fn rendered_paths() -> Vec<String> {
    let golden = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../kbf-installer/tests/golden/envoy/box/envoy.json");
    let body = std::fs::read_to_string(&golden).expect("golden envoy.json");
    let mut out = Vec::new();
    for part in body.split("\"filename\": \"").skip(1) {
        out.push(part.split('"').next().unwrap_or("").to_owned());
    }
    out
}

fn under(path: &str, mount: &str) -> bool {
    path.strip_prefix(mount)
        .is_some_and(|rest| rest.starts_with('/'))
}

fn envoy_files(d: &Node, out: &mut Vec<String>) {
    let vols = at(d, "spec.template.spec.volumes")
        .map(seq)
        .unwrap_or_default();
    let mut paths = rendered_paths();
    assert!(
        paths.len() >= 3,
        "the golden envoy.json names fewer than three files: {paths:?}"
    );
    for c in containers(d) {
        let args = strings(get(c, "args"));
        if let Some(i) = args.iter().position(|a| *a == "-c") {
            paths.extend(args.get(i + 1).map(|a| (*a).to_owned()));
        }
        // (mountPath, readOnly, backed by a host directory)
        let mounts: Vec<(&str, bool, bool)> = at(c, "volumeMounts")
            .map(seq)
            .unwrap_or_default()
            .iter()
            .map(|m| {
                let v = vols.iter().find(|v| text(v, "name") == text(m, "name"));
                (
                    text(m, "mountPath").unwrap_or(""),
                    text(m, "readOnly") == Some("true"),
                    v.and_then(|v| text(v, "hostPath.type")) == Some("Directory"),
                )
            })
            .collect();
        for p in &paths {
            if !mounts.iter().any(|(m, _, _)| under(p, m)) {
                out.push(format!(
                    "kbf-envoy: {p} is not under any volume mount: Envoy cannot read it"
                ));
            }
        }
        for (m, ro, host) in &mounts {
            if !ro || !host {
                out.push(format!(
                    "kbf-envoy: mount {m} is not a read-only host directory"
                ));
            }
        }
    }
}

fn envoy(d: &Node, out: &mut Vec<String>) {
    envoy_files(d, out);
    if text(d, "spec.template.spec.automountServiceAccountToken") != Some("false") {
        out.push("kbf-envoy: mounts a service account token".into());
    }
    if get(d, "spec.template.spec.serviceAccountName").is_some() {
        out.push("kbf-envoy: names a service account".into());
    }
    if text(d, "spec.template.spec.hostNetwork") != Some("true") {
        out.push("kbf-envoy: hostNetwork is not true".into());
    }
    for c in containers(d) {
        if get(c, "livenessProbe").is_some() {
            out.push("kbf-envoy: has a livenessProbe".into());
        }
        resources(c, "kbf-envoy", &mut |m| out.push(m.to_owned()));
    }
}

fn role(r: &Node, out: &mut Vec<String>) {
    if text(r, "metadata.namespace") != Some("kbf") {
        out.push("the Role is not in namespace kbf".into());
    }
    let rules = get(r, "rules").map(seq).unwrap_or_default();
    if rules.len() != 1 {
        out.push(format!("the Role has {} rules, not one", rules.len()));
    }
    for rule in rules {
        let mut verbs = strings(get(rule, "verbs"));
        verbs.sort_unstable();
        if verbs != ["get", "patch"] {
            out.push(format!("the Role's verbs are {verbs:?}, not get and patch"));
        }
        if strings(get(rule, "apiGroups")) != ["apps"]
            || strings(get(rule, "resources")) != ["deployments"]
            || strings(get(rule, "resourceNames")) != ["kbf-frontend"]
        {
            out.push("the Role is not apps/deployments named kbf-frontend only".into());
        }
    }
}

fn binding(b: &Node, out: &mut Vec<String>) {
    if text(b, "roleRef.kind") != Some("Role") {
        out.push("the RoleBinding does not bind a namespaced Role".into());
    }
    let subjects = get(b, "subjects").map(seq).unwrap_or_default();
    if subjects.len() != 1
        || text(&subjects[0], "kind") != Some("ServiceAccount")
        || text(&subjects[0], "name") != Some("kbf-frontend")
        || text(&subjects[0], "namespace") != Some("kbf")
    {
        out.push("the RoleBinding's subject is not the kbf-frontend service account".into());
    }
}

/// No address of a machine and no tailnet name in a file that is public.
fn public_safe(name: &str, body: &str, out: &mut Vec<String>) {
    for (i, line) in body.lines().enumerate() {
        let code = line.split('#').next().unwrap_or("");
        let ip = code
            .split(|c: char| !c.is_ascii_digit() && c != '.')
            .any(|w| {
                let parts: Vec<_> = w.split('.').collect();
                parts.len() == 4
                    && parts
                        .iter()
                        .all(|p| !p.is_empty() && p.parse::<u8>().is_ok())
            });
        // 127.0.0.1 is the loopback the probes name, not a machine.
        let ip = ip && !code.contains("127.0.0.1");
        if ip || code.contains(".ts.net") {
            out.push(format!("{name}:{}: an address or tailnet name", i + 1));
        }
    }
}

fn real() -> BTreeMap<String, String> {
    files()
}

fn with(file: &str, from: &str, to: &str) -> BTreeMap<String, String> {
    let mut f = real();
    let body = f.get_mut(file).unwrap_or_else(|| panic!("{file}"));
    assert!(
        body.contains(from),
        "{file} has no {from:?}: the mutant would change nothing"
    );
    *body = body.replacen(from, to, 1);
    f
}

fn refused(f: &BTreeMap<String, String>, needle: &str) {
    let p = problems(f);
    assert!(
        p.iter().any(|l| l.contains(needle)),
        "wanted a problem containing {needle:?}, got {p:?}"
    );
}

/// Catches a manifest set that breaks one of the rules above as committed.
#[test]
fn the_manifests_hold_the_rules() {
    assert_eq!(problems(&real()), Vec::<String>::new());
}

/// Catches a check that passes on an empty directory.
#[test]
fn all_seven_manifests_are_read() {
    let names: Vec<_> = real().into_keys().collect();
    assert_eq!(
        names,
        [
            "envoy-deployment.yaml",
            "frontend-deployment.yaml",
            "kustomization.yaml",
            "namespace.yaml",
            "role.yaml",
            "rolebinding.yaml",
            "serviceaccount.yaml",
        ]
    );
}

#[test]
fn a_second_replica_is_refused() {
    refused(
        &with("frontend-deployment.yaml", "replicas: 1", "replicas: 2"),
        "replicas is not 1",
    );
}

#[test]
fn a_short_grace_period_is_refused() {
    refused(
        &with(
            "frontend-deployment.yaml",
            "terminationGracePeriodSeconds: 2592000",
            "terminationGracePeriodSeconds: 30",
        ),
        "under a week",
    );
}

#[test]
fn the_mode_flag_form_is_refused() {
    refused(
        &with(
            "frontend-deployment.yaml",
            "            - frontend\n",
            "            - --mode=frontend\n",
        ),
        "do not start with the mode word",
    );
}

#[test]
fn a_frontend_without_its_config_flag_is_refused() {
    refused(
        &with(
            "frontend-deployment.yaml",
            "            - --config=/etc/kbf/kbf.toml\n",
            "",
        ),
        "args lack --config",
    );
}

#[test]
fn a_liveness_probe_is_refused() {
    refused(
        &with(
            "frontend-deployment.yaml",
            "          readinessProbe:",
            "          livenessProbe:\n            httpGet: {path: /readyz, port: 8982}\n          readinessProbe:",
        ),
        "livenessProbe",
    );
}

#[test]
fn a_prestop_hook_is_refused() {
    refused(
        &with(
            "frontend-deployment.yaml",
            "          readinessProbe:",
            "          lifecycle:\n            preStop: {exec: {command: [sleep, \"5\"]}}\n          readinessProbe:",
        ),
        "lifecycle",
    );
}

#[test]
fn a_rolling_update_that_may_stop_the_old_pod_early_is_refused() {
    refused(
        &with(
            "frontend-deployment.yaml",
            "type: Recreate",
            "type: RollingUpdate",
        ),
        "RollingUpdate without",
    );
}

#[test]
fn a_pod_network_is_refused() {
    refused(
        &with(
            "frontend-deployment.yaml",
            "hostNetwork: true",
            "hostNetwork: false",
        ),
        "hostNetwork",
    );
}

#[test]
fn different_uid_and_fsgroup_are_refused() {
    refused(
        &with(
            "frontend-deployment.yaml",
            "fsGroup: 65532",
            "fsGroup: 1000",
        ),
        "not fsGroup",
    );
}

#[test]
fn root_is_refused() {
    refused(
        &with(
            "frontend-deployment.yaml",
            "runAsUser: 65532",
            "runAsUser: 0",
        ),
        "root",
    );
}

#[test]
fn a_limit_above_the_request_is_refused() {
    let f = with(
        "frontend-deployment.yaml",
        "            limits:\n              cpu: \"4\"",
        "            limits:\n              cpu: \"8\"",
    );
    refused(&f, "cpu request");
}

#[test]
fn a_claim_for_the_state_is_refused() {
    refused(
        &with(
            "frontend-deployment.yaml",
            "          hostPath:\n            path: /var/lib/kbf-server\n            type: Directory",
            "          persistentVolumeClaim:\n            claimName: state",
        ),
        "claim or a Secret",
    );
}

#[test]
fn a_host_path_that_kubelet_would_create_is_refused() {
    refused(
        &with(
            "frontend-deployment.yaml",
            "path: /run/kbf-frontend\n            type: Directory",
            "path: /run/kbf-frontend\n            type: DirectoryOrCreate",
        ),
        "control is not a hostPath",
    );
}

#[test]
fn a_wildcard_verb_is_refused() {
    refused(
        &with("role.yaml", "[\"get\", \"patch\"]", "[\"*\"]"),
        "verbs",
    );
}

#[test]
fn a_list_verb_is_refused() {
    refused(
        &with(
            "role.yaml",
            "[\"get\", \"patch\"]",
            "[\"get\", \"patch\", \"list\"]",
        ),
        "verbs",
    );
}

#[test]
fn a_role_for_every_deployment_is_refused() {
    refused(
        &with("role.yaml", "    resourceNames: [\"kbf-frontend\"]\n", ""),
        "named kbf-frontend only",
    );
}

#[test]
fn a_cluster_role_is_refused() {
    let f = with("role.yaml", "kind: Role", "kind: ClusterRole");
    refused(&f, "ClusterRole");
}

#[test]
fn a_binding_to_another_account_is_refused() {
    refused(
        &with(
            "rolebinding.yaml",
            "  - kind: ServiceAccount\n    name: kbf-frontend",
            "  - kind: ServiceAccount\n    name: default",
        ),
        "subject",
    );
}

#[test]
fn a_token_in_the_envoy_pod_is_refused() {
    refused(
        &with(
            "envoy-deployment.yaml",
            "automountServiceAccountToken: false",
            "automountServiceAccountToken: true",
        ),
        "kbf-envoy: mounts a service account token",
    );
}

/// The Envoy pod mounting only `/etc/kbf/envoy` (the defect found in review) cannot read
/// the certificates and keys `envoy.json` names under `/etc/kbf/tls`.
#[test]
fn an_envoy_pod_that_cannot_read_its_certificates_is_refused() {
    let f = with(
        "envoy-deployment.yaml",
        "mountPath: /etc/kbf\n",
        "mountPath: /etc/kbf/envoy\n",
    );
    refused(&f, "/etc/kbf/tls/reapi.key is not under any volume mount");
    refused(&f, "/etc/kbf/tls/ca.pem is not under any volume mount");
}

/// A writable mount would let a compromised Envoy rewrite the certificates.
#[test]
fn a_writable_envoy_mount_is_refused() {
    refused(
        &with(
            "envoy-deployment.yaml",
            "              readOnly: true\n",
            "",
        ),
        "kbf-envoy: mount /etc/kbf is not a read-only host directory",
    );
}

#[test]
fn an_image_by_tag_is_refused() {
    refused(
        &with(
            "kustomization.yaml",
            "    newName: envoyproxy/envoy\n",
            "    newName: envoyproxy/envoy\n    newTag: v1\n",
        ),
        "by tag",
    );
}

#[test]
fn a_short_digest_is_refused() {
    refused(
        &with(
            "kustomization.yaml",
            "sha256:7369b033550a",
            "sha256:7369b033550",
        ),
        "digest",
    );
}

#[test]
fn an_unlisted_manifest_is_refused() {
    let mut f = real();
    f.insert("extra.yaml".into(), "kind: Namespace\n".into());
    refused(&f, "not in kustomization.yaml");
}

#[test]
fn a_secret_is_refused() {
    let mut f = real();
    f.insert("secret.yaml".into(), "kind: Secret\n".into());
    let k = f.get_mut("kustomization.yaml").expect("kustomization");
    *k = k.replacen("  - role.yaml\n", "  - role.yaml\n  - secret.yaml\n", 1);
    refused(&f, "Secret");
}

#[test]
fn an_address_is_refused() {
    refused(
        &with(
            "frontend-deployment.yaml",
            "host: 127.0.0.1\n              port: 8982\n              path: /readyz\n            periodSeconds: 5\n            failureThreshold: 3",
            "host: 192.0.2.3\n              port: 8982\n              path: /readyz\n            periodSeconds: 5\n            failureThreshold: 3",
        ),
        "an address",
    );
}
