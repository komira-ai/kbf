use super::*;

/// What the daemon's flags ask of its host, for a check before it starts (`kbf`'s
/// startup self-checks read it): the files it reads and the programs it runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostNeeds {
    /// `--ca-cert`.
    pub ca_cert: PathBuf,
    /// `--cert`.
    pub cert: PathBuf,
    /// `--key`.
    pub key: PathBuf,
    /// `--node-id`: the one name the certificate must carry.
    pub node_id: String,
    /// `--data-dir`, as the daemon resolves it.
    pub data_dir: PathBuf,
    /// Set when the container driver runs: what it needs.
    pub container: Option<ContainerNeeds>,
}

/// What the container driver asks of its host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContainerNeeds {
    /// `--oci-runtime`: the program that runs the containers.
    pub oci_runtime: PathBuf,
}

impl Cli {
    /// What these flags ask of the host.
    #[must_use]
    pub fn host_needs(&self) -> HostNeeds {
        let container = self
            .drivers
            .contains(&Driver::Container)
            .then(|| ContainerNeeds {
                oci_runtime: self.oci_runtime.clone(),
            });
        HostNeeds {
            ca_cert: self.daemon.ca_cert.clone(),
            cert: self.daemon.cert.clone(),
            key: self.daemon.key.clone(),
            node_id: self.daemon.node_id.clone(),
            data_dir: self.data_dir(),
            container,
        }
    }
}
