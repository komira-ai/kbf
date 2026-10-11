//! `kbf-deploy minio nodeport`: open a MinIO's S3 port (and only it) to hosts off its
//! Kubernetes cluster, with one NodePort Service beside the MinIO Service.
//!
//! The Service it adds:
//!
//! - name `<service>-kbf-np` in the MinIO Service's namespace, labelled
//!   `komira.dev/purpose=kbf-pilot`;
//! - type NodePort, `--node-port` (30000 to 32767);
//! - one port, `s3`, 9000 to 9000: the S3 API, never the console;
//! - the selector copied from the MinIO Service, so it reaches the same pods.
//!
//! Read-only checks first, each refusing on failure: the MinIO Service exists, has a
//! port named `s3` at 9000 and a selector, and no other Service holds the node port.
//! When the Service is missing or differs, the plan applies the manifest (`kubectl
//! apply -f -`), after a `kubectl apply --dry-run=server` of it has passed; that check
//! runs in a dry run too. A Service that already matches plans nothing.
//!
//! `--remove` deletes the Service, and only one that carries the label.
//!
//! `kubectl` gets `--kubeconfig` and `--cache-dir` on its argv and an empty
//! environment, so a kubeconfig that runs an exec credential plugin is not supported.
//! The plan applies only as a user other than root.

use std::collections::BTreeMap;
use std::io::Write;

use serde::{Deserialize, Serialize};

use super::{K8sName, MinioError, checked, note, read_json};
use crate::exec::Exec;
use crate::path::HostPath;
use crate::plan::{Plan, Planner};
use crate::step::Cmd;

/// The label the Service carries; `--remove` deletes only a Service that has it.
pub const LABEL: &str = "komira.dev/purpose";
pub const LABEL_VALUE: &str = "kbf-pilot";
const S3_PORT: u16 = 9000;

/// What `minio nodeport` is asked to do.
pub struct NodePort {
    pub kubectl: HostPath,
    pub kubeconfig: HostPath,
    pub context: Option<String>,
    /// kubectl's `--cache-dir`: a private scratch directory, removed after the run.
    pub cache_dir: String,
    pub namespace: K8sName,
    pub service: K8sName,
    pub node_port: u16,
    pub remove: bool,
}

impl NodePort {
    fn name(&self) -> String {
        format!("{}-kbf-np", self.service)
    }

    fn kubectl(&self, args: &[&str]) -> Cmd {
        let mut cmd = Cmd::new(self.kubectl.clone())
            .arg("--kubeconfig")
            .arg(self.kubeconfig.as_str())
            .arg("--cache-dir")
            .arg(&self.cache_dir);
        if let Some(context) = &self.context {
            cmd = cmd.arg("--context").arg(context);
        }
        for arg in args {
            cmd = cmd.arg(*arg);
        }
        cmd
    }
}

/// The parts of a Service this kit reads and writes.
#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Service {
    #[serde(default)]
    api_version: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    metadata: Meta,
    #[serde(default)]
    spec: Spec,
}

#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Meta {
    #[serde(default)]
    name: String,
    #[serde(default)]
    namespace: String,
    #[serde(default)]
    labels: BTreeMap<String, String>,
    #[serde(default)]
    annotations: BTreeMap<String, String>,
}

#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Spec {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    selector: BTreeMap<String, String>,
    #[serde(default)]
    ports: Vec<Port>,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Port {
    #[serde(default)]
    name: String,
    #[serde(default)]
    protocol: String,
    port: u16,
    #[serde(default)]
    target_port: serde_json::Value,
    #[serde(default)]
    node_port: Option<u16>,
}

#[derive(Deserialize)]
struct List {
    items: Vec<Service>,
}

/// The plan for `args`, asking the cluster (through `exec`) what it has. What the plan
/// found, and the manifest, go to `out`.
pub fn plan<E: Exec, W: Write>(exec: &E, args: &NodePort, out: &mut W) -> Result<Plan, MinioError> {
    let ns = args.namespace.to_string();
    let name = args.name();
    let full = format!("{ns}/{name}");
    let services: List = read_json(
        exec,
        &args.kubectl(&["get", "services", "-A", "-o", "json"]),
    )?;
    let ours = services
        .items
        .iter()
        .find(|s| s.metadata.namespace == ns && s.metadata.name == name);
    let mut planner = Planner::new(exec);
    if args.remove {
        match ours {
            None => note(out, format_args!("no Service {full}"))?,
            Some(s) if s.metadata.labels.get(LABEL).map(String::as_str) != Some(LABEL_VALUE) => {
                return Err(MinioError::NotOurs(full));
            }
            Some(_) => planner.run(args.kubectl(&["-n", &ns, "delete", "service", &name])),
        }
        return Ok(planner.finish());
    }

    let minio = format!("{ns}/{}", args.service);
    let source: Service = read_json(
        exec,
        &args.kubectl(&[
            "-n",
            &ns,
            "get",
            "service",
            &args.service.to_string(),
            "-o",
            "json",
        ]),
    )?;
    if !source
        .spec
        .ports
        .iter()
        .any(|p| p.name == "s3" && p.port == S3_PORT)
    {
        return Err(MinioError::NoS3Port(minio));
    }
    if source.spec.selector.is_empty() {
        return Err(MinioError::NoSelector(minio));
    }
    let holders: Vec<String> = services
        .items
        .iter()
        .filter(|s| {
            s.spec
                .ports
                .iter()
                .any(|p| p.node_port == Some(args.node_port))
        })
        .map(|s| format!("{}/{}", s.metadata.namespace, s.metadata.name))
        .filter(|held| *held != full)
        .collect();
    if !holders.is_empty() {
        return Err(MinioError::Held {
            port: args.node_port,
            by: holders.join(", "),
        });
    }

    let wanted = manifest(args, &name, source.spec.selector);
    let manifest = serde_json::to_vec_pretty(&wanted).map_err(MinioError::Serialize)?;
    let same = ours.is_some_and(|s| {
        s.spec == wanted.spec && s.metadata.labels.get(LABEL) == wanted.metadata.labels.get(LABEL)
    });
    if same {
        note(out, format_args!("Service {full}: as wanted, kept"))?;
        return Ok(planner.finish());
    }
    note(out, format_args!("Service {full}, the manifest applied:"))?;
    note(out, format_args!("{}", String::from_utf8_lossy(&manifest)))?;
    let check = args
        .kubectl(&["apply", "--dry-run=server", "-f", "-"])
        .stdin(manifest.clone());
    checked(exec, &check)?;
    note(
        out,
        format_args!("kubectl apply --dry-run=server: accepted"),
    )?;
    planner.run(args.kubectl(&["apply", "-f", "-"]).stdin(manifest));
    Ok(planner.finish())
}

fn manifest(args: &NodePort, name: &str, selector: BTreeMap<String, String>) -> Service {
    let why = format!(
        "kbf-deploy minio nodeport: MinIO's S3 API (port {S3_PORT} only) for hosts off the \
         cluster. Remove with kbf-deploy minio nodeport --remove --apply."
    );
    Service {
        api_version: "v1".to_owned(),
        kind: "Service".to_owned(),
        metadata: Meta {
            name: name.to_owned(),
            namespace: args.namespace.to_string(),
            labels: BTreeMap::from([(LABEL.to_owned(), LABEL_VALUE.to_owned())]),
            annotations: BTreeMap::from([("komira.dev/why".to_owned(), why)]),
        },
        spec: Spec {
            kind: "NodePort".to_owned(),
            selector,
            ports: vec![Port {
                name: "s3".to_owned(),
                protocol: "TCP".to_owned(),
                port: S3_PORT,
                target_port: S3_PORT.into(),
                node_port: Some(args.node_port),
            }],
        },
    }
}

#[cfg(test)]
#[path = "nodeport_tests.rs"]
mod tests;
