//! The front-door proxy in pool mode in front of two real `kbf` frontends, each over a real
//! Postgres database of its own: one `Execute` is held open on a
//! frontend, that frontend is drained through the proxy's admin API (`admin_listen`), and the
//! open `Execute` is left alone while every new one goes to the other frontend.
//!
//! Everything but the proxy library, which runs in this process the way the installer's verb
//! runs it, is a child process: a `kbf storage` node (shared by both frontends) and two `kbf
//! frontend` processes, which are this test binary run through a link named `kbf` (the trick of
//! `proxy_soak.rs`). Each frontend keeps its state in a Postgres database of the test's own,
//! with the installer's schema applied. Like every
//! Postgres test it needs a server and fails without one: `KBF_TEST_POSTGRES_URL` names a server
//! the test may create a database on.
//!
//! The streams are `Execution.Execute` calls, the stream a Buck2 build holds open per action: no
//! worker runs them, so each is queued on its frontend and the stream stays open and idle. The
//! frontend counts them itself (`open_executions` of `GET /v1/frontend/drain`), which makes the
//! figures below the child's own. No worker means no result: "the Execute completes" here is the
//! client ending it (as Buck2 does when it cancels), and what is proved is that the drain does
//! not end it, reset it or move it, and that it can end on the drained frontend afterwards.
//!
//! What this proves: with two frontends ready the proxy places an `Execute` on one of them; a
//! drain of that one answers the list with it `draining` and its stream counted; the stream stays
//! open and quiet, on the frontend that has it; the next `Execute` goes to the other frontend and
//! the drained one is given nothing more; ending the old stream brings the drained frontend's
//! own count and the proxy's to 0; an undrain makes it `up` again. The per-member series of
//! `/metrics` show the drain.

#![cfg(target_os = "linux")]

mod proxy_support;

use std::io::{Read as _, Write as _};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::os::fd::OwnedFd;

use kbf_proxy::Proxy;
use kbf_proxy::config::Frontend;
use kbf_proxy::control::NoUpdates;
use kbf_state::postgres::test_database;
use kbf_testkit::dir::TestDir;
use proxy_support::{
    Child, Open, actions, api_get, connect, free_base, frontend_args, link_kbf, open_executions,
    proxy_config, proxy_get, storage_node, store, until, until_ready, within,
};
use serde_json::Value;

const LIST: &str = "/v1/proxy/upstreams";

/// `url` with the password of its user information removed: the flag refuses one, as the
/// command line is world-readable, and the server the job runs trusts its loopback clients.
fn without_password(url: &str) -> String {
    let (scheme, rest) = url.split_once("://").expect("a URL");
    match rest.split_once('@') {
        Some((user, tail)) => {
            let user = user.split_once(':').map_or(user, |(name, _)| name);
            format!("{scheme}://{user}@{tail}")
        }
        None => url.to_owned(),
    }
}

/// One request to the proxy's admin API: the status code and the JSON body.
fn admin(addr: SocketAddr, method: &str, path: &str) -> (u16, Value) {
    let mut stream = TcpStream::connect(addr).expect("connect to the admin API");
    let head = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\n");
    let request = format!("{head}Connection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).expect("send");
    let mut text = String::new();
    stream.read_to_string(&mut text).expect("read");
    let (head, body) = text.split_once("\r\n\r\n").expect("a head and a body");
    let code = head.split_whitespace().nth(1).expect("a status line");
    let body = serde_json::from_str(body).expect("a JSON body");
    (code.parse().expect("a status code"), body)
}

/// The entry of member `name` in the admin API's list.
fn member(list: &Value, name: &str) -> Value {
    let members = list.as_array().expect("a list");
    members
        .iter()
        .find(|m| m["name"] == name)
        .expect("the member is listed")
        .clone()
}

/// The Execute streams the proxy counts on member `name`, from the admin API.
fn proxy_count(admin_addr: SocketAddr, name: &str) -> u64 {
    let (_, list) = admin(admin_addr, "GET", LIST);
    member(&list, name)["open_streams"]["reapi"]
        .as_u64()
        .expect("a count")
}

/// A `kbf frontend` on the ports from `base`, over the Postgres database `flag` names.
fn frontend(dir: &std::path::Path, name: &str, storage: &str, base: u16, flag: &str) -> Child {
    // `frontend_args` gives a SQLite file, which this test does not use: its flag is swapped.
    let mut args = frontend_args(storage, base, &dir.join(name).join("unused.db"));
    args.retain(|arg| !arg.starts_with("--state-store="));
    args.push(flag.to_owned());
    Child::start(dir, name, &args)
}

/// Asserts that, with real frontends on a real Postgres database each, a drained frontend keeps
/// its open `Execute` (no reset, not moved, counted until it ends), that new `Execute`s go to the
/// other frontend, and that an undrain lists it as `up` again. The open `Execute` ends by the
/// client cancelling it: no worker runs it, so it has no result of its own to complete with.
#[test]
fn a_drained_frontend_keeps_its_execute_and_new_ones_go_to_the_other() {
    // One database for each frontend, with the installer's schema: the frontends share
    // nothing but the storage node, so each one's state is its own.
    let (database_a, database_b) = (test_database(), test_database());

    // Declared before the children, so it drops after them: they are reaped before it is removed.
    let scratch = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "proxy-pool");
    let dir = scratch.path().to_path_buf();
    link_kbf(&dir);
    let node = storage_node(&dir, "node");
    let storage = node.field("storage=");
    let base = free_base();
    let flag_a = format!("--state-store={}", without_password(&database_a.url()));
    let flag_b = format!("--state-store={}", without_password(&database_b.url()));
    let mut a = frontend(&dir, "frontend-a", &storage, base, &flag_a);
    let mut b = frontend(&dir, "frontend-b", &storage, base + 10, &flag_b);
    let (api_a, api_b) = (
        format!("127.0.0.1:{}", base + 2),
        format!("127.0.0.1:{}", base + 12),
    );
    assert_eq!(
        (api_get(&api_a, "/readyz").0, api_get(&api_b, "/readyz").0),
        (200, 200)
    );

    let public = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("public socket");
    let public_addr: SocketAddr = public.local_addr().expect("addr");
    let admin_socket = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("admin socket");
    let admin_addr = admin_socket.local_addr().expect("addr");
    drop(admin_socket);
    let (mut proxy, pki) = proxy_config(&dir, base);
    proxy.upstream = None;
    proxy.frontend = vec![
        Frontend {
            name: "a".to_owned(),
            base_port: base,
        },
        Frontend {
            name: "b".to_owned(),
            base_port: base + 10,
        },
    ];
    proxy.admin_listen = Some(admin_addr);
    proxy.check().expect("a valid pool table");
    let (handle, runner) = Proxy::bind(proxy, OwnedFd::from(public)).expect("bind");
    assert!(
        handle.startup_hold().is_none(),
        "a pool starts with no hold"
    );
    let serving = std::thread::spawn(move || runner.run(NoUpdates));

    let client_runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    client_runtime.block_on(async {
        // The actions are in the one store both frontends read.
        let (requests, blobs) = actions("pool", 3);
        within("the actions", store(&a.field("reapi="), blobs)).await;
        let (client, driver) = connect(&public_addr.to_string(), &pki.ca_cert).await;
        until_ready(&client).await;
        until("both frontends ready", || {
            let (_, list) = admin(admin_addr, "GET", LIST);
            member(&list, "a")["ready"] == true && member(&list, "b")["ready"] == true
        })
        .await;

        // An Execute through the proxy lands on one frontend, which counts it and so does the
        // proxy; the other has none.
        let mut first = Open::start(&client, &requests[0])
            .await
            .expect("the first Execute starts");
        let counts = (open_executions(&api_a), open_executions(&api_b));
        let mut seen = [counts.0, counts.1];
        seen.sort_unstable();
        assert_eq!(seen, [0, 1], "one frontend: {counts:?}");
        let (held, held_api, other, other_api) = if counts.0 == 1 {
            ("a", &api_a, "b", &api_b)
        } else {
            ("b", &api_b, "a", &api_a)
        };
        assert_eq!(proxy_count(admin_addr, held), 1);
        assert_eq!(proxy_count(admin_addr, other), 0);

        // The drain answers the list with the frontend draining and its stream counted.
        let drain = format!("{LIST}/{held}:drain");
        let (code, list) = admin(admin_addr, "POST", &drain);
        assert_eq!(code, 200, "{list}");
        let drained = member(&list, held);
        assert_eq!(drained["state"], "draining");
        assert_eq!(drained["open_streams"]["reapi"], 1);
        assert_eq!(member(&list, other)["state"], "up");
        // A second drain is the same answer.
        let (code, again) = admin(admin_addr, "POST", &drain);
        assert_eq!((code, &again), (200, &list));

        // The open Execute is left alone, where it is.
        assert!(
            first.quiet().await,
            "the drain ended or woke the open Execute"
        );
        assert_eq!(open_executions(held_api), 1);
        assert_eq!(open_executions(other_api), 0);

        // The next Execute goes to the other frontend, and the drained one is given nothing.
        let second = Open::start(&client, &requests[1])
            .await
            .expect("a new Execute starts on the other frontend");
        assert_eq!(open_executions(other_api), 1);
        assert_eq!(
            open_executions(held_api),
            1,
            "the drained frontend took one more"
        );
        assert_eq!(proxy_count(admin_addr, other), 1);
        assert!(first.quiet().await);

        // The drain shows in the metrics of the public port.
        let (code, metrics) = proxy_get(&client, "/metrics").await;
        assert_eq!(code, 200);
        let line = format!("\nkbf_proxy_member_up{{member=\"{held}\"}} 0\n");
        assert!(metrics.contains(&line), "{metrics}");

        // The old Execute ends (the client cancels it) on the drained frontend, and both counts
        // fall to 0.
        drop(first);
        until("the drained frontend's Execute ends", || {
            open_executions(held_api) == 0
        })
        .await;
        until("the proxy's count falls to 0", || {
            proxy_count(admin_addr, held) == 0
        })
        .await;
        assert_eq!(open_executions(other_api), 1, "the other one is untouched");

        // An undrain brings the frontend back.
        let (code, list) = admin(admin_addr, "POST", &format!("{LIST}/{held}:undrain"));
        assert_eq!(code, 200, "{list}");
        assert_eq!(member(&list, held)["state"], "up");
        let (code, nothing) = admin(admin_addr, "POST", &format!("{LIST}/nobody:drain"));
        assert_eq!(code, 404, "{nothing}");

        assert!(!driver.is_finished(), "one connection carried every stream");
        drop(second);
        until("the other frontend's Execute ends", || {
            open_executions(other_api) == 0
        })
        .await;
    });
    handle.shutdown();
    serving
        .join()
        .expect("the runner thread")
        .expect("the proxy ran");
    assert!(a.stop().is_some_and(|status| status.success()), "A exits 0");
    assert!(b.stop().is_some_and(|status| status.success()), "B exits 0");
    drop((node, database_a, database_b));
}
