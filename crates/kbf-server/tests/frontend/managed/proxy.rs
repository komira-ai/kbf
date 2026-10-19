//! A candidate behind the proxy (`--proxy-auth-file`): the stand-in on its operator port
//! gates calls as the server it stands in for does.

use super::*;

/// Catches a candidate whose operator-port stand-in is served without the proxy secret the
/// flags name: behind the proxy a call that presents the secret is held to the gate's
/// rules, so one with a malformed `x-kbf-role` is refused `403`, as the real server
/// refuses it; served without the secret, the gate would take the call as direct and
/// answer it. Readiness still answers a plain local probe.
///
/// Mutant: the stand-in served with no secret (`None`) answers `200` to the malformed call.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_candidate_behind_the_proxy_gates_its_operator_port() {
    const SECRET: [u8; 32] = [0x5a; 32];
    let scratch = Scratch::new("proxy-candidate");
    let secret = scratch.0.join("proxy-secret");
    std::fs::write(&secret, SECRET).expect("secret file");
    std::fs::set_permissions(&secret, std::os::unix::fs::PermissionsExt::from_mode(0o600))
        .expect("mode");
    let node = Node::start().await;
    let store = shared();
    let _holder = Child::start(&managed_flags(&node, &[]), &store).await;
    let worker = format!("--worker-listen=127.0.0.1:{}", free_port());
    let behind = format!("--proxy-auth-file={}", secret.display());
    let candidate = Child::start(
        &managed_flags(&node, &["--await-takeover", &worker, &behind]),
        &store,
    )
    .await;
    assert_eq!(candidate.role().await["role"], "candidate");
    until("the candidate is ready", async || {
        readyz(candidate.api).await == 200
    })
    .await;

    let malformed = format!(
        "GET /readyz HTTP/1.1\r\nHost: kbf\r\nx-kbf-proxy-auth: {}\r\nx-kbf-role: no-such-role\r\n\
         Connection: close\r\n\r\n",
        hex::encode(SECRET)
    );
    let (code, body) = exchange(candidate.api, &malformed).await;
    assert_eq!(
        code, 403,
        "the stand-in answered a call the gate refuses: {body}"
    );
}
