//! `GET /v1/invocations/{id}/failures` (`docs/design/invocations-api.md` 4.7), over a
//! real listener with the records fed to the recorder directly and the Action, Command
//! and log blobs stored in the cache. Each test names the defect it catches.

mod pages;

use kbf_invocation::{Event, InvocationId, LeaseEnd, Outcome, Owner};
use kbf_proto::reapi::{Action, Command, Digest as WireDigest, Platform, command, platform};
use kbf_sched::RequeueReason;
use kbf_types::{Digest, Failure, LostWhy};
use pages::{
    OPEN, PAGES_ONLY, Server, at, digest, ended, finished, granted, hit, listing, queued, requeued,
    started, text,
};
use prost::Message;
use serde_json::{Value, json};

const INV: &str = "b";

fn wire(d: &Digest) -> WireDigest {
    WireDigest {
        hash: d.hash_hex(),
        size_bytes: i64::try_from(d.size_bytes).expect("small"),
    }
}

/// An Execute of `action` on `main` by invocation `id` that queued operation `op`, naming
/// `target` and `mnemonic`.
fn queued_as(target: &str, mnemonic: &str, when: u64, action: Digest, op: u64) -> Event {
    let Event::Call(mut c) = queued(INV, "main", at(when), action, op) else {
        unreachable!("queued is a call")
    };
    c.metadata.target = Some(kbf_invocation::Text::sent(target));
    c.metadata.mnemonic = Some(kbf_invocation::Text::sent(mnemonic));
    Event::Call(c)
}

/// The recorder's note that `action` names `command`.
fn names_command(action: Digest, command: Digest) -> Event {
    Event::Command {
        id: InvocationId::new(INV),
        owner: Owner::default(),
        action,
        command,
        shape: None,
    }
}

/// The Command blob of a compile, with a secret in its environment; its digest.
async fn compile(server: &Server) -> Digest {
    let command = Command {
        arguments: ["cc", "-c", "a.c", "-o", "out/a.o"]
            .map(str::to_owned)
            .into(),
        environment_variables: vec![command::EnvironmentVariable {
            name: "PATH".to_owned(),
            value: "/usr/bin:/secret-path-value".to_owned(),
        }],
        working_directory: "src".to_owned(),
        output_paths: vec!["out/a.o".to_owned()],
        ..Command::default()
    };
    server.store(&command.encode_to_vec()).await
}

/// An Action blob naming `command`, a platform and a 30 s timeout; its digest.
async fn action_of(server: &Server, command: &Digest) -> Digest {
    let action = Action {
        command_digest: Some(wire(command)),
        timeout: Some(prost_types::Duration {
            seconds: 30,
            nanos: 0,
        }),
        platform: Some(Platform {
            properties: vec![platform::Property {
                name: "OSFamily".to_owned(),
                value: "linux".to_owned(),
            }],
        }),
        ..Action::default()
    };
    server.store(&action.encode_to_vec()).await
}

/// A 40 KiB log of numbered lines ending in `last`.
fn long_log(last: &str) -> Vec<u8> {
    let mut log = String::new();
    for i in 0..4_000 {
        log.push_str(&format!("line {i:06}\n"));
    }
    log.push_str(last);
    log.into_bytes()
}

/// The failures of `INV`, from a page.
fn failures(page: &Value) -> &[Value] {
    page["failures"].as_array().expect("failures")
}

/// Catches (4.7): the failures view that lists a passing action, an action still in
/// flight or one that passed after a rerun; that lists them out of the order the farm saw
/// them, or groups them by kind; that drops a kind (a non-zero exit with an OK status, an
/// infrastructure failure, a timeout, a refusal); an `outcome` or `failure` misread (the
/// class and the daemon's reason of the last attempt, `null` for a run that exited
/// non-zero); attempts that lose a requeue; a command that shows an environment value, or
/// loses its platform, timeout or working directory; a log tail that is not the end of
/// the log, is longer than 16 KiB, or does not say it is cut; and the invocation's answer
/// and its `failures` link missing.
#[tokio::test]
async fn a_failed_build_lists_each_failure_with_its_reason_command_and_log_tail() {
    let server = Server::with(OPEN);
    let command = compile(&server).await;
    let failing = action_of(&server, &command).await;
    let stderr = server.store(&long_log("error: a.c:3: boom\n")).await;
    let stdout = server.store(b"compiling a.c\n").await;
    let result = digest(900);
    let (ok_action, flight, rerun) = (digest(1), digest(2), digest(3));
    server
        .feed([
            // Op 1 exits 1 on its second lease, after one requeue; its Command is known.
            queued_as("//app:lib", "CxxCompile", 0, failing, 1),
            names_command(failing, command),
            granted(1, 1, "node-a", at(10)),
            requeued(1, 1, RequeueReason::Silent, at(20)),
            granted(1, 2, "node-b", at(30)),
            started(1, 2, at(31)),
            finished(
                1,
                2,
                Outcome::Ok { exit_code: 1 },
                Some(listing(result, 1, &[], (Some(stdout), Some(stderr)))),
                at(40),
            ),
            // Op 2 passes: not a failure.
            queued_as("//app:ok", "CxxCompile", 50, ok_action, 2),
            granted(2, 1, "node-a", at(51)),
            finished(
                2,
                1,
                Outcome::Ok { exit_code: 0 },
                Some(listing(digest(901), 0, &[], (None, None))),
                at(60),
            ),
            // Op 3 is still running: not listed yet.
            queued_as("//app:slow", "CxxCompile", 70, flight, 3),
            granted(3, 1, "node-a", at(71)),
            // Op 4 passes after a rerun: not a failure.
            queued_as("//app:flaky", "CxxCompile", 80, rerun, 4),
            granted(4, 1, "node-a", at(81)),
            requeued(4, 1, RequeueReason::Silent, at(82)),
            granted(4, 2, "node-b", at(83)),
            finished(
                4,
                2,
                Outcome::Ok { exit_code: 0 },
                Some(listing(digest(902), 0, &[], (None, None))),
                at(90),
            ),
            // Op 5: the farm failed it (INFRA). Op 6: timed out. Op 7: refused.
            queued_as("//app:infra", "Link", 100, digest(5), 5),
            granted(5, 1, "node-c", at(101)),
            ended(
                5,
                Some((1, LeaseEnd::Failed(Failure::Infra, "disk full".to_owned()))),
                Outcome::Failed {
                    code: 14,
                    message: "the farm failed the action".to_owned(),
                },
                at(110),
            ),
            queued_as("//app:hang", "Test", 120, digest(6), 6),
            ended(6, None, Outcome::TimedOut, at(130)),
            queued_as("//app:odd", "Test", 140, digest(7), 7),
            ended(
                7,
                None,
                Outcome::Refused("no node offers platform gpu".to_owned()),
                at(150),
            ),
        ])
        .await;

    let page = server.ok(&format!("/v1/invocations/{INV}/failures")).await;
    let list = failures(&page);
    let targets: Vec<_> = list
        .iter()
        .map(|f| f["target"]["value"].as_str().expect("a target"))
        .collect();
    assert_eq!(
        targets,
        ["//app:lib", "//app:infra", "//app:hang", "//app:odd"],
        "first-seen order, failures only: {page}"
    );
    assert!(page.get("next_page").is_none() && page.get("next_page_token").is_none());
    assert_eq!(page["invocation"]["id"], INV);
    assert_eq!(
        page["invocation"]["links"]["failures"],
        format!("/v1/invocations/{INV}/failures")
    );

    // The run that exited 1.
    let lib = &list[0];
    assert_eq!(lib["action"], text(&failing));
    assert_eq!(
        lib["href"],
        format!("/v1/invocations/{INV}/actions/{}", text(&failing))
    );
    assert_eq!(lib["mnemonic"]["value"], "CxxCompile");
    assert_eq!(lib["outcome"], json!({"ok": {"exit_code": 1}}));
    assert_eq!(lib["failure"], Value::Null, "it ran to a non-zero exit");
    let attempts = lib["attempts"].as_array().expect("attempts");
    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[0]["worker"], "node-a");
    assert_eq!(attempts[0]["end"]["requeue"], "silent");
    assert_eq!(attempts[1]["worker"], "node-b");
    assert_eq!(attempts[1]["end"], json!({"result": "ok"}));
    assert_eq!(lib["command_available"], true);
    assert_eq!(
        lib["command"],
        json!({
            "arguments": ["cc", "-c", "a.c", "-o", "out/a.o"],
            "environment_names": ["PATH"],
            "working_directory": "src",
            "platform": {"OSFamily": "linux"},
            "timeout_micros": 30_000_000,
            "output_paths": ["out/a.o"],
            "truncated": false,
        })
    );
    assert!(
        !page.to_string().contains("secret-path-value"),
        "an environment value"
    );
    let err = &lib["stderr"];
    assert_eq!(err["digest"], text(&stderr));
    assert_eq!(err["bytes"], stderr.size_bytes);
    assert_eq!(err["available"], true);
    assert_eq!(err["tail_bytes"], 16 * 1024);
    assert_eq!(err["truncated"], true);
    let tail = err["tail"].as_str().expect("a tail");
    assert_eq!(tail.len(), 16 * 1024);
    assert!(
        tail.ends_with("line 003999\nerror: a.c:3: boom\n"),
        "the end of the log"
    );
    assert!(!tail.contains("line 000000"), "not the start of the log");
    assert_eq!(
        err["href"],
        format!(
            "/v1/blobs/{}?instance=main&name=stderr&inline=text",
            text(&stderr)
        )
    );
    let out = &lib["stdout"];
    assert_eq!(
        (&out["tail"], &out["tail_bytes"], &out["truncated"]),
        (&json!("compiling a.c\n"), &json!(14), &json!(false))
    );

    // The farm's failure, with the daemon's reason; no Command was ever seen for it.
    let infra = &list[1];
    assert_eq!(infra["outcome"]["failed"]["code"], 14);
    assert_eq!(
        infra["failure"],
        json!({"class": "infra", "reason": "disk full"})
    );
    assert_eq!(infra["attempts"][0]["end"]["failure"], "infra");
    assert_eq!(
        (&infra["command"], &infra["command_available"]),
        (&Value::Null, &json!(false))
    );
    assert_eq!(
        (&infra["stdout"], &infra["stderr"]),
        (&Value::Null, &Value::Null)
    );
    assert_eq!(list[2]["outcome"], json!({"timed_out": {}}));
    assert_eq!(list[2]["attempts"], json!([]));
    assert_eq!(
        list[3]["outcome"],
        json!({"refused": {"reason": "no node offers platform gpu"}})
    );
    server.stop().await;
}

/// Catches: a page that skips or repeats a failure across pages, a failure that arrives
/// while the build is paged put before the pages already read, a `next_page` that does
/// not carry the page size, a page token of another route (or of this one on another)
/// accepted, and page sizes outside 1 to 100, a duplicate or an unknown parameter
/// answered anything but `400`.
#[tokio::test]
async fn failures_page_in_order_and_refuse_what_the_route_does_not_take() {
    let server = Server::with(OPEN);
    let fail = |n: u64| {
        [
            queued(INV, "main", at(n * 10), digest(n), n),
            ended(n, None, Outcome::TimedOut, at(n * 10 + 1)),
        ]
    };
    server.feed((1..=3).flat_map(fail)).await;
    let base = format!("/v1/invocations/{INV}/failures");
    let first = server.ok(&format!("{base}?page_size=2")).await;
    assert_eq!(failures(&first).len(), 2);
    let next = first["next_page"].as_str().expect("a next page").to_owned();
    assert!(
        next.starts_with(&format!("{base}?page_size=2&page_token=")),
        "{next}"
    );
    // A failure that lands now comes after the three, not between the pages.
    server.feed(fail(4)).await;
    let second = server.ok(&next).await;
    let actions: Vec<_> = failures(&first)
        .iter()
        .chain(failures(&second))
        .map(|f| f["action"].as_str().expect("an action").to_owned())
        .collect();
    let want: Vec<_> = (1..=4).map(|n| text(&digest(n))).collect();
    assert_eq!(actions, want, "each failure once, in the order seen");
    assert!(second.get("next_page").is_none(), "{second}");
    let token = first["next_page_token"].as_str().expect("a token");

    for query in [
        "?page_size=0".to_owned(),
        "?page_size=101".to_owned(),
        "?page_size=x".to_owned(),
        "?page_size=2&page_size=3".to_owned(),
        "?outcome=failed".to_owned(),
        "?sort=wall_ms".to_owned(),
        "?page_token=zz".to_owned(),
    ] {
        let answer = server.get(&format!("{base}{query}")).await;
        assert_eq!(answer.code, 400, "{query}: {}", answer.json);
    }
    let on_actions = server
        .get(&format!("/v1/invocations/{INV}/actions?page_token={token}"))
        .await;
    assert_eq!(on_actions.code, 400, "a failures token on /actions");
    let actions_page = server
        .ok(&format!("/v1/invocations/{INV}/actions?page_size=1"))
        .await;
    let actions_token = actions_page["next_page_token"].as_str().expect("a token");
    let on_failures = server
        .get(&format!("{base}?page_token={actions_token}"))
        .await;
    assert_eq!(on_failures.code, 400, "an /actions token on /failures");
    // The cap is 100: the default is 20.
    let all = server.ok(&format!("{base}?page_size=100")).await;
    assert_eq!(failures(&all).len(), 4);
    server.stop().await;
}

/// Catches: a failure listing for a record that is not held answered anything but 4.10's
/// `no_record`, and the route open when the policy names no invocation authorizer.
#[tokio::test]
async fn failures_of_an_unknown_record_are_no_record_and_the_route_is_off_without_a_policy() {
    let server = Server::with(OPEN);
    let gone = server.get("/v1/invocations/nope/failures").await;
    assert_eq!(gone.code, 404);
    assert_eq!(gone.json["error"], "no_record");
    server.stop().await;
    let off = Server::with(r#"{"authenticationPolicy": {"allow": {}}}"#);
    let answer = off.get("/v1/invocations/nope/failures").await;
    assert_eq!(answer.code, 404);
    assert_eq!(answer.json["off"], "invocations");
    off.stop().await;
}

/// Catches: a log tail or command read from the CAS when the policy names no
/// `blobs.getAuthorizer` (the page then shows what the blob route would refuse), and,
/// with the blob route on, a missing blob that is not told apart from a read that failed:
/// a log not held, one over the read bound, a Command blob gone, an Action blob never
/// uploaded.
#[tokio::test]
async fn what_the_cas_cannot_give_is_said_not_guessed() {
    let base = format!("/v1/invocations/{INV}/failures");

    // The blob route off: nothing is read, whatever the cache holds.
    let off = Server::with(PAGES_ONLY);
    let (held_out, held_err) = (off.store(b"out").await, off.store(b"err").await);
    exit_two(&off, (held_out, held_err)).await;
    let page = off.ok(&base).await;
    let row = &failures(&page)[0];
    for log in ["stdout", "stderr"] {
        assert_eq!(row[log]["tail_omitted"], "blobs_off", "{log}");
        assert_eq!(row[log]["tail"], Value::Null, "{log}");
        assert_eq!(row[log]["href"], Value::Null, "{log}");
    }
    assert_eq!(
        (&row["command"], &row["command_available"]),
        (&Value::Null, &json!(false))
    );
    off.stop().await;

    // The blob route on, and the cache holding none of it.
    let server = Server::with(OPEN);
    let huge = Digest::new(kbf_types::DigestFunction::Sha256, [7; 32], 65 * 1024 * 1024);
    exit_two(&server, (digest(63), huge)).await;
    let page = server.ok(&base).await;
    let row = &failures(&page)[0];
    assert_eq!(row["stdout"]["available"], false);
    assert_eq!(row["stdout"]["tail_omitted"], "not_held");
    assert_eq!(row["stderr"]["tail_omitted"], "too_large");
    assert_eq!(row["stderr"]["bytes"], huge.size_bytes);
    assert_eq!(
        (&row["command"], &row["command_available"]),
        (&Value::Null, &json!(false)),
        "neither the Action nor the Command is held"
    );
    server.stop().await;

    // The Action is held, its Command is not.
    let server = Server::with(OPEN);
    let action = action_of(&server, &digest(64)).await;
    server
        .feed([
            queued(INV, "main", at(0), action, 1),
            names_command(action, digest(64)),
            ended(1, None, Outcome::TimedOut, at(10)),
        ])
        .await;
    let page = server.ok(&base).await;
    assert_eq!(failures(&page)[0]["command_available"], false);
    server.stop().await;
}

/// Catches (an attacker names the target, the farm's reason text, the arguments, a
/// worker and writes the logs): any of their text reaching the body as raw `<`, `>` or
/// `&` (a body sniffed or embedded as HTML would run it); a body that stops being JSON
/// or changes the shape of its row when a string holds a quote, a backslash, a newline,
/// a NUL, a bidi override or a closing brace; text that does not come back byte for byte
/// (invalid UTF-8 excepted, which is replaced); and an answer that is not
/// `application/json` with `nosniff`.
#[tokio::test]
async fn attacker_text_is_inert_in_the_body_and_comes_back_unchanged() {
    const XSS: &str = "<script>alert(1)</script>\"><img src=x onerror=alert(2)>&amp;";
    const BREAKOUT: &str = "\",\"outcome\":{\"ok\":{\"exit_code\":0}},\"x\":\"}\\\r\n\0\u{202e}";
    let payloads = [
        XSS,
        BREAKOUT,
        "javascript:alert(1)",
        "data:text/html,<b>x</b>",
    ];
    let server = Server::with(OPEN);
    let hostile = Command {
        arguments: payloads.map(str::to_owned).into(),
        environment_variables: vec![command::EnvironmentVariable {
            name: XSS.to_owned(),
            value: "v".to_owned(),
        }],
        working_directory: BREAKOUT.to_owned(),
        output_paths: vec![XSS.to_owned()],
        ..Command::default()
    };
    let command = server.store(&hostile.encode_to_vec()).await;
    let action = action_of(&server, &command).await;
    let mut log = payloads.join("\n").into_bytes();
    log.extend_from_slice(b"\n\xff\xfe invalid utf-8 </pre>\n");
    let stderr = server.store(&log).await;
    server
        .feed([
            queued_as(XSS, BREAKOUT, 0, action, 1),
            names_command(action, command),
            granted(1, 1, XSS, at(1)),
            ended(
                1,
                Some((1, LeaseEnd::Failed(Failure::Infra, BREAKOUT.to_owned()))),
                Outcome::Failed {
                    code: 14,
                    message: XSS.to_owned(),
                },
                at(2),
            ),
            queued_as("//t:ok-exit", "M", 10, digest(9), 2),
            finished(
                2,
                1,
                Outcome::Ok { exit_code: 3 },
                Some(listing(digest(70), 3, &[], (None, Some(stderr)))),
                at(11),
            ),
        ])
        .await;
    let answer = server.get(&format!("/v1/invocations/{INV}/failures")).await;
    assert_eq!(answer.code, 200, "{}", answer.head);
    let head = answer.head.to_ascii_lowercase();
    assert!(
        head.contains("content-type: application/json\r\n"),
        "{head}"
    );
    assert!(head.contains("x-content-type-options: nosniff"), "{head}");
    for raw in [b'<', b'>', b'&'] {
        assert!(
            !answer.body.contains(&raw),
            "a raw {:?} in the body",
            char::from(raw)
        );
    }
    let page = answer.json;
    let list = failures(&page);
    assert_eq!(list.len(), 2, "no payload added or hid a row: {page}");
    let keys =
        |row: &Value| -> Vec<String> { row.as_object().expect("a row").keys().cloned().collect() };
    assert_eq!(
        keys(&list[0]),
        keys(&list[1]),
        "the same fields in both rows"
    );

    let (infra, exit) = (&list[0], &list[1]);
    assert_eq!(infra["target"]["value"], XSS);
    assert_eq!(infra["mnemonic"]["value"], BREAKOUT);
    assert_eq!(infra["outcome"]["failed"]["message"], XSS);
    assert_eq!(infra["failure"]["reason"], BREAKOUT);
    assert_eq!(infra["attempts"][0]["worker"], XSS);
    assert_eq!(infra["attempts"][0]["end"]["reason"], BREAKOUT);
    assert_eq!(
        infra["command"]["arguments"],
        json!(payloads),
        "arguments come back as sent"
    );
    assert_eq!(infra["command"]["environment_names"], json!([XSS]));
    assert_eq!(infra["command"]["working_directory"], BREAKOUT);
    assert_eq!(infra["command"]["output_paths"], json!([XSS]));
    // The row of the exit code is untouched by the payloads in the first.
    assert_eq!(exit["outcome"], json!({"ok": {"exit_code": 3}}));
    let tail = exit["stderr"]["tail"].as_str().expect("a tail");
    let want = format!(
        "{}\n\u{fffd}\u{fffd} invalid utf-8 </pre>\n",
        payloads.join("\n")
    );
    assert_eq!(tail, want, "the log, invalid bytes replaced");
    server.stop().await;
}

/// Catches: the failure of an action taken from the first attempt rather than the last
/// (a lease lost, then one that ran), a class that is not the last attempt's, a reason
/// longer than the record's 1 KiB cap or cut inside a character, a cached result that
/// exited 1 left out (the client saw it fail: it has no attempts), and one that exited 0
/// listed.
#[tokio::test]
async fn the_failure_is_the_last_attempts_and_a_cached_failure_has_none() {
    let server = Server::with(OPEN);
    // 'é' is two bytes: 1024 bytes is 512 of them, so an odd prefix puts the cut inside one.
    let reason = format!("x{}", "é".repeat(600));
    server
        .feed([
            queued(INV, "main", at(0), digest(1), 1),
            granted(1, 1, "n", at(1)),
            requeued(1, 1, RequeueReason::Silent, at(2)),
            granted(1, 2, "n", at(3)),
            ended(
                1,
                Some((2, LeaseEnd::Failed(Failure::Lost(LostWhy::Fenced), reason))),
                Outcome::Failed {
                    code: 14,
                    message: "lost".to_owned(),
                },
                at(4),
            ),
            hit(
                INV,
                at(5),
                digest(2),
                listing(digest(3), 1, &[], (None, None)),
            ),
            hit(
                INV,
                at(6),
                digest(4),
                listing(digest(5), 0, &[], (None, None)),
            ),
        ])
        .await;
    let page = server.ok(&format!("/v1/invocations/{INV}/failures")).await;
    let [row, cached] = failures(&page) else {
        panic!(
            "the lost action and the cached exit 1 are failures, the cached exit 0 is not: {page}"
        );
    };
    assert_eq!(cached["action"], text(&digest(2)));
    assert_eq!(cached["outcome"], json!({"ok": {"exit_code": 1}}));
    assert_eq!(
        (&cached["attempts"], &cached["failure"]),
        (&json!([]), &Value::Null)
    );
    assert_eq!(row["failure"]["class"], "lost");
    let shown = row["failure"]["reason"].as_str().expect("a reason");
    assert_eq!(shown.len(), 1023, "1024 bytes, less the half character");
    assert!(shown[1..].chars().all(|c| c == 'é'));
    assert_eq!(row["attempts"][0]["end"]["requeue"], "silent");
    server.stop().await;
}

/// One action, `digest(61)`, that exited 2 with the logs `logs` (stdout, stderr), and
/// whose Command digest, `digest(60)`, the recorder has read.
async fn exit_two(server: &Server, logs: (Digest, Digest)) {
    let known = digest(61);
    server
        .feed([
            queued(INV, "main", at(0), known, 1),
            names_command(known, digest(60)),
            finished(
                1,
                1,
                Outcome::Ok { exit_code: 2 },
                Some(listing(digest(62), 2, &[], (Some(logs.0), Some(logs.1)))),
                at(10),
            ),
        ])
        .await;
}
