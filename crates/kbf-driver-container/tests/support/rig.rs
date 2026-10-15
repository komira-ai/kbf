//! What the conformance tests (`tests/conformance.rs`) need of a test rig, so one test
//! body runs against every container runtime the driver has: the stand-in `podman`
//! ([`Fake`]) and the stand-in `crun` of the OCI backend ([`FakeOci`]).

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use kbf_daemon::{Runtime, RuntimeError};
use kbf_driver_container::{MemoryCas, PodmanConfig, PodmanRuntime, StartError};
use kbf_proto::reapi::ActionResult;
use kbf_types::Resources;

use super::fake::Fake;
use super::fake_oci::FakeOci;
use super::{Spec, store_action, work};

pub trait Rig: Sized {
    /// The runtime's name in a failure message.
    const NAME: &'static str;

    fn with(name: &str, configure: impl FnOnce(&mut PodmanConfig)) -> Self;

    fn new(name: &str) -> Self {
        Self::with(name, |_| {})
    }

    fn runtime(&self) -> Arc<PodmanRuntime<MemoryCas>>;
    fn cas(&self) -> Arc<MemoryCas>;
    fn config(&self) -> PodmanConfig;
    fn action(&self, steps: &[&str]);
    fn lease_dir(&self, seq: u64) -> PathBuf;
    fn lease_cgroup(&self, seq: u64) -> PathBuf;
    fn assert_clean(&self, seq: u64);
    fn assert_action_running(&self, seq: u64);
    fn assert_action_gone(&self, seq: u64);
    fn restart(&self) -> Result<PodmanRuntime<MemoryCas>, StartError>;
    fn wait_for_start(&self) -> impl Future<Output = ()>;
    /// The runtime fails the next start itself, before the action runs.
    fn runtime_breaks(&self);
    /// The next start fails as `crun` does when the program's exec finds it busy.
    fn busy_once(&self);
    /// The pid of the last action, as the stand-in published it.
    fn action_pid(&self) -> String;
    /// The verbs the runtime was run with, in order.
    fn calls(&self) -> Vec<String>;
    /// What the stand-in recorded, in order.
    fn events(&self) -> Vec<String>;
    /// How many times the runtime was asked to run the container.
    fn starts(&self) -> usize;

    fn run(
        &self,
        seq: u64,
        spec: &Spec,
        steps: &[&str],
    ) -> impl Future<Output = Result<ActionResult, RuntimeError>> {
        self.action(steps);
        let action = store_action(&self.cas(), spec);
        let runtime = self.runtime();
        async move {
            runtime
                .run(work(seq, action, Resources::new(2000, 1 << 30)))
                .await
        }
    }
}

impl Rig for Fake {
    const NAME: &'static str = "podman";

    fn with(name: &str, configure: impl FnOnce(&mut PodmanConfig)) -> Self {
        Fake::with(name, configure)
    }

    fn runtime(&self) -> Arc<PodmanRuntime<MemoryCas>> {
        Arc::clone(&self.runtime)
    }

    fn cas(&self) -> Arc<MemoryCas> {
        Arc::clone(&self.cas)
    }

    fn config(&self) -> PodmanConfig {
        self.config.clone()
    }

    fn action(&self, steps: &[&str]) {
        Fake::action(self, steps);
    }

    fn lease_dir(&self, seq: u64) -> PathBuf {
        Fake::lease_dir(self, seq)
    }

    fn lease_cgroup(&self, seq: u64) -> PathBuf {
        Fake::lease_cgroup(self, seq)
    }

    fn assert_clean(&self, seq: u64) {
        Fake::assert_clean(self, seq);
    }

    fn assert_action_running(&self, seq: u64) {
        Fake::assert_action_running(self, seq);
    }

    fn assert_action_gone(&self, seq: u64) {
        Fake::assert_action_gone(self, seq);
    }

    fn restart(&self) -> Result<PodmanRuntime<MemoryCas>, StartError> {
        Fake::restart(self)
    }

    fn wait_for_start(&self) -> impl Future<Output = ()> {
        Fake::wait_for_start(self)
    }

    fn runtime_breaks(&self) {
        self.knob("create-fails", "");
    }

    fn busy_once(&self) {
        self.knob("busy-once", "");
    }

    fn action_pid(&self) -> String {
        std::fs::read_to_string(self.state.join("pid"))
            .expect("pid")
            .trim()
            .to_owned()
    }

    fn calls(&self) -> Vec<String> {
        Fake::calls(self)
    }

    fn events(&self) -> Vec<String> {
        Fake::events(self)
    }

    fn starts(&self) -> usize {
        Fake::calls(self).iter().filter(|c| *c == "start").count()
    }
}

impl Rig for FakeOci {
    const NAME: &'static str = "oci";

    fn with(name: &str, configure: impl FnOnce(&mut PodmanConfig)) -> Self {
        FakeOci::with(name, configure)
    }

    fn runtime(&self) -> Arc<PodmanRuntime<MemoryCas>> {
        Arc::clone(&self.runtime)
    }

    fn cas(&self) -> Arc<MemoryCas> {
        Arc::clone(&self.cas)
    }

    fn config(&self) -> PodmanConfig {
        self.config.clone()
    }

    fn action(&self, steps: &[&str]) {
        FakeOci::action(self, steps);
    }

    fn lease_dir(&self, seq: u64) -> PathBuf {
        FakeOci::lease_dir(self, seq)
    }

    fn lease_cgroup(&self, seq: u64) -> PathBuf {
        FakeOci::lease_cgroup(self, seq)
    }

    fn assert_clean(&self, seq: u64) {
        FakeOci::assert_clean(self, seq);
    }

    fn assert_action_running(&self, seq: u64) {
        FakeOci::assert_action_running(self, seq);
    }

    fn assert_action_gone(&self, seq: u64) {
        FakeOci::assert_action_gone(self, seq);
    }

    fn restart(&self) -> Result<PodmanRuntime<MemoryCas>, StartError> {
        FakeOci::restart(self)
    }

    fn wait_for_start(&self) -> impl Future<Output = ()> {
        FakeOci::wait_for_start(self)
    }

    fn runtime_breaks(&self) {
        self.knob("crun-error", "cannot create the container");
    }

    fn busy_once(&self) {
        self.knob("busy-once", "");
    }

    fn action_pid(&self) -> String {
        std::fs::read_to_string(self.state.join("pid"))
            .expect("pid")
            .trim()
            .to_owned()
    }

    fn calls(&self) -> Vec<String> {
        FakeOci::calls(self)
    }

    fn events(&self) -> Vec<String> {
        FakeOci::events(self)
    }

    fn starts(&self) -> usize {
        FakeOci::calls(self).iter().filter(|c| *c == "run").count()
    }
}
