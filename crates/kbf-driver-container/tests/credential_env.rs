//! The node's cold-store key does not reach an action through the runtime's
//! environment, against the stand-in `podman` and `crun` (`support/fake.rs`,
//! `support/fake_oci.rs`). Each stand-in runs the action with its own environment, as a
//! runtime that copies the host's into a container would (Podman's `env_host`), so
//! what the action sees is what the driver started the runtime with.
//!
//! One test in this binary: it sets variables of its own process, which is sound only
//! while no other thread reads the environment. `tests/podman_credential.rs` checks
//! the same on real rootless Podman, with the key file besides.

mod support;

use support::Spec;
use support::fake::{Fake, image};
use support::fake_oci::FakeOci;

/// The key's two variables, as the unit's `EnvironmentFile=` sets them.
const KEY: [(&str, &str); 2] = [
    ("AWS_ACCESS_KEY_ID", "kbf-test-cold-store-key-id"),
    ("AWS_SECRET_ACCESS_KEY", "kbf-test-cold-store-secret"),
];

/// A variable of the node that is not a credential: the stand-ins hand it on.
const NODE_VARIABLE: (&str, &str) = ("KBF_TEST_NODE_VARIABLE", "handed-on");

/// Act steps printing each of the key's variables, then [`NODE_VARIABLE`], each
/// `unset` when the action does not have it.
fn print_env() -> Vec<String> {
    let mut steps = Vec::new();
    for (name, _) in KEY.into_iter().chain([NODE_VARIABLE]) {
        if !steps.is_empty() {
            steps.push(";".to_owned());
        }
        steps.extend(["env", name, "unset"].map(str::to_owned));
    }
    steps
}

/// Catches Podman or `crun` started with the key in its environment (the "drop
/// `without_credentials`" mutant at each start site the lease goes through): the
/// stand-in hands it to the action, which prints it. [`NODE_VARIABLE`] is the
/// control: it reaches the action, so an `unset` for the key is the driver's doing,
/// not a stand-in that hands nothing on.
#[test]
fn the_runtime_is_started_without_the_cold_store_key() {
    // SAFETY: the only test of this binary, and the first thing it does: no other
    // thread of this process runs yet, so none reads the environment meanwhile.
    unsafe {
        for (name, value) in KEY.into_iter().chain([NODE_VARIABLE]) {
            std::env::set_var(name, value);
        }
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let want = "unset\nunset\nhanded-on\n";
    runtime.block_on(async {
        let spec = Spec::new(&image(), "unused");

        let fake = Fake::new("credential-env");
        let result = fake.run(1, &spec, &print_env()).await.expect("ran");
        let stdout = support::blob(&fake.cas, result.stdout_digest.as_ref());
        assert_eq!(String::from_utf8_lossy(&stdout), want, "podman");
        assert_eq!(result.exit_code, 0);
        fake.assert_clean(1);

        let oci = FakeOci::new("credential-env");
        let result = oci.run(1, &spec, &print_env()).await.expect("ran");
        let stdout = support::blob(&oci.cas, result.stdout_digest.as_ref());
        assert_eq!(String::from_utf8_lossy(&stdout), want, "crun");
        assert_eq!(result.exit_code, 0);
        oci.assert_clean(1);
    });
}
