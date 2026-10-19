//! The admin port behind the proxy (`--proxy-auth-file`): its calls need the proxy's secret.
//! Without the flag the port is loopback-only and takes no credential, as before.

use super::*;

const SECRET: [u8; 32] = [0x5a; 32];

/// A secret file in `scratch`, 0600: its path and the header value the proxy presents.
fn secret_file(scratch: &Scratch) -> (String, String) {
    let file = scratch.0.join("proxy-secret");
    std::fs::write(&file, SECRET).expect("secret file");
    std::fs::set_permissions(&file, std::os::unix::fs::PermissionsExt::from_mode(0o600))
        .expect("mode");
    (
        format!("--proxy-auth-file={}", file.display()),
        hex::encode(SECRET),
    )
}

/// One admin call, with `x-kbf-proxy-auth: <secret>` when there is one.
async fn admin_call(child: &Child, method: &str, path: &str, secret: Option<&str>) -> (u16, Value) {
    let header = secret.map_or_else(String::new, |s| format!("x-kbf-proxy-auth: {s}\r\n"));
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: kbf\r\n{header}Connection: close\r\n\
         Content-Length: 0\r\n\r\n"
    );
    exchange(child.admin, &request).await
}

/// Catches an admin port served without the proxy secret: behind the proxy a local process
/// that is not the proxy could quiesce the frontend. A call with no header, and one with a
/// wrong secret, are refused `403` with the operator API's error shape and do nothing; the
/// right secret quiesces. `GET role` needs the secret too: it is read-only, but the proxy
/// presents the secret on every call and one rule is easier to hold than two, and a role and
/// generation are not for every local process.
///
/// Mutants: the admin router served with no secret (`None`) takes the call with no header
/// as direct and quiesces; a layer that also admits `Trust::Unverified` does the same.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_admin_port_behind_the_proxy_needs_its_secret() {
    let scratch = Scratch::new("admin-secret");
    let (flag, secret) = secret_file(&scratch);
    let node = Node::start().await;
    let store = shared();
    let child = Child::start(&managed_flags(&node, &[&flag]), &store).await;

    let (code, body) = admin_call(&child, "POST", "/v1/admin/quiesce", None).await;
    assert_eq!(code, 403, "a call with no secret was taken: {body}");
    assert!(body["error"].is_string(), "{body}");
    let (code, body) =
        admin_call(&child, "POST", "/v1/admin/quiesce", Some(&"00".repeat(32))).await;
    assert_eq!(code, 403, "a call with a wrong secret was taken: {body}");
    assert!(body["error"].is_string(), "{body}");
    let (code, body) = admin_call(&child, "GET", "/v1/admin/role", None).await;
    assert_eq!(code, 403, "role answered with no secret: {body}");
    meanwhile().await;
    let (code, view) = admin_call(&child, "GET", "/v1/admin/role", Some(&secret)).await;
    assert_eq!(code, 200, "{view}");
    assert_eq!(view["role"], "active", "a refused quiesce acted: {view}");

    let (code, view) = admin_call(&child, "POST", "/v1/admin/quiesce", Some(&secret)).await;
    assert_eq!(code, 200, "{view}");
    assert_eq!(view["role"], "quiesced", "{view}");
}

/// Without `--proxy-auth-file` the admin port is as it was: loopback-only, no credential.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_admin_port_without_a_proxy_takes_calls_with_no_secret() {
    let node = Node::start().await;
    let store = shared();
    let child = Child::start(&managed_flags(&node, &[]), &store).await;

    let (code, view) = admin_call(&child, "GET", "/v1/admin/role", None).await;
    assert_eq!(code, 200, "{view}");
    let (code, view) = admin_call(&child, "POST", "/v1/admin/quiesce", None).await;
    assert_eq!(code, 200, "{view}");
    assert_eq!(view["role"], "quiesced", "{view}");
}
