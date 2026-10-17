//! What the Updates page shows for each answer the read API can give.

use futures::executor::block_on;
use serde_json::{Value, json};

use super::{UPDATES, rfc3339};
use crate::read::Reader;
use crate::tests::{
    COMMIT_A, COMMIT_B, Fake, HOSTS_URL, body, hosts_doc, node, nodes_doc, render, update_doc,
    updates_farm,
};

fn page(fake: &Fake) -> (u16, String) {
    let a = render(fake, "/ui/updates");
    (a.status(), body(&a))
}

fn farm_with(update: &Value) -> Fake {
    Fake::default()
        .json("/v1/update", update)
        .json("/v1/nodes", &nodes_doc())
        .json(HOSTS_URL, &hosts_doc())
}

/// Catches: the page missing a part: the farm block (desired and generation, set time,
/// policy flags, "N of M"), the releases, the frontend row, the host table, the
/// command. Mutant: the "up to date" count taken over all hosts, not the judged ones.
#[test]
fn the_page_shows_the_farm_the_releases_the_frontend_and_the_hosts() {
    let (status, html) = page(&updates_farm());
    assert_eq!(status, 200, "{html}");
    for text in [
        "<title>Updates - kbf</title>",
        &format!("<dt>Desired release</dt><dd>{COMMIT_B} (generation 7)</dd>"),
        "<dt>Set at</dt><dd>2026-10-07T10:46:40Z</dd>",
        "<dt>Policy auto_nodes</dt><dd>not set</dd>",
        "<dt>Hosts up to date</dt><dd>1 of 2 judged, 3 hosts in all</dd>",
        "running</span>",
        "<h2>Frontend</h2>",
        &format!("<dt>Commit</dt><dd>{COMMIT_B}</dd>"),
        "only through the frontend switch behind Envoy",
        "aria-current=\"page\">Updates</a>",
    ] {
        assert!(html.contains(text), "{text}: {html}");
    }
    // The frontend is a block of its own: not a row of the host table.
    let table = &html[html.find("<h2>Hosts</h2>").expect("hosts")..];
    assert!(!table.contains("frontend"), "{table}");
}

/// Catches: a pause read wrongly. The rule: `policy.paused` only when that key is
/// there, else `desired.paused`; never "running" when it cannot be told. Mutants: read
/// only `policy.paused` (a policy row without the key shows running or unknown); read
/// only `desired.paused` (a policy that says paused shows running); treat a key that is
/// not a boolean as absent.
#[test]
fn a_pause_is_read_by_the_rule_and_never_defaults_to_running() {
    let paused_desired = |paused: bool| json!({"generation": 1, "commit": COMMIT_A, "set_at_ms": 0, "paused": paused});
    let releases = update_doc()["releases"].clone();
    let doc = |desired: &Value, policy: &Value| json!({"desired": desired, "policy": policy, "releases": releases});
    let cases = [
        // A policy row without the key: the desired release's pause counts.
        (
            doc(&paused_desired(true), &json!({"auto_nodes": true})),
            "paused",
        ),
        (
            doc(&paused_desired(false), &json!({"auto_nodes": true})),
            "running",
        ),
        // No policy row.
        (doc(&paused_desired(true), &Value::Null), "paused"),
        // The policy's own key wins, both ways.
        (
            doc(&paused_desired(true), &json!({"paused": false})),
            "running",
        ),
        (
            doc(&paused_desired(false), &json!({"paused": true})),
            "paused",
        ),
        // Nothing says, or the key is not a boolean: unknown, not running.
        (
            doc(&Value::Null, &json!({"auto_nodes": true})),
            "pause state unknown",
        ),
        (doc(&Value::Null, &Value::Null), "pause state unknown"),
        (
            doc(&paused_desired(false), &json!({"paused": null})),
            "pause state unknown",
        ),
        (
            doc(&paused_desired(false), &json!({"paused": "yes"})),
            "pause state unknown",
        ),
    ];
    for (update, word) in cases {
        let (status, html) = page(&farm_with(&update));
        assert_eq!(status, 200, "{update}: {html}");
        let start = html.find("<p><span class=\"pill ").expect("a pill");
        let shown = &html[start..html[start..].find("</span>").map(|e| start + e).unwrap()];
        assert!(shown.ends_with(&format!(">{word}")), "{update}: {shown}");
    }
}

/// Catches: the policy flags shown wrongly or not at all. Mutant: the two flags swapped.
#[test]
fn the_policy_flags_are_shown_by_name() {
    let mut update = update_doc();
    update["policy"] = json!({"auto_nodes": true, "auto_frontend": false});
    let (_, html) = page(&farm_with(&update));
    assert!(
        html.contains("<dt>Policy auto_nodes</dt><dd>on</dd>"),
        "{html}"
    );
    assert!(
        html.contains("<dt>Policy auto_frontend</dt><dd>off</dd>"),
        "{html}"
    );
}

/// Catches: a platform complete or not without the component it needs named (linux
/// `kbf`, darwin `kbf-daemon`); the desired release not marked; an empty store.
/// Mutants: the two components swapped; every platform "complete".
#[test]
fn releases_name_the_component_each_platform_needs() {
    let (_, html) = page(&updates_farm());
    for text in [
        "linux-x86_64: complete (kbf)",
        "darwin-arm64: incomplete, needs (kbf-daemon)",
        "linux-arm64: complete (kbf)",
        "darwin-arm64: complete (kbf-daemon)",
        &format!("<td><code>{COMMIT_B}</code></td><td>yes</td>"),
        &format!("<td><code>{COMMIT_A}</code></td><td></td>"),
    ] {
        assert!(html.contains(text), "{text}: {html}");
    }
    let mut update = update_doc();
    update["releases"] = json!({"releases": [
        {"commit": COMMIT_A, "platforms": [{"platform": "plan9-mips", "complete": true}]}]});
    let (_, html) = page(&farm_with(&update));
    assert!(
        html.contains("plan9-mips: complete (unknown platform)"),
        "{html}"
    );
    update["releases"] = json!({"releases": []});
    update["desired"] = Value::Null;
    let (_, html) = page(&farm_with(&update));
    assert!(
        html.contains("The release store holds no release."),
        "{html}"
    );
    assert!(html.contains("<dd>none offered yet</dd>"), "{html}");
    assert!(html.contains("<dt>Set at</dt><dd>-</dd>"), "{html}");
}

/// Catches: a host row missing a column, or a value under another's heading: node,
/// platform with its component, role, running and desired commit, up to date, step
/// state, availability, last error. Mutants: running and desired swapped; the Mac
/// shown as linux; a refusal dropped; a host with no detail row given another's.
#[test]
fn the_host_table_shows_each_host_with_its_detail() {
    let (_, html) = page(&updates_farm());
    for head in [
        "Node",
        "Platform",
        "Role",
        "Running",
        "Desired",
        "Up to date",
        "Step",
        "Availability",
        "Last error",
    ] {
        assert!(
            html.contains(&format!("<th scope=\"col\">{head}</th>")),
            "{head}"
        );
    }
    let row = |id: &str| {
        let start = html.find(&format!("<tr><td>{id}</td>")).expect("a row");
        html[start..start + html[start..].find("</tr>").expect("end")].to_owned()
    };
    assert_eq!(
        row("linux-1"),
        format!(
            "<tr><td>linux-1</td><td>linux (kbf)</td><td>-</td><td>bbbbbbbbbbbb</td>\
             <td>{COMMIT_B}</td><td>yes</td><td>confirmed</td>\
             <td><span class=\"pill ok\">ok</span></td><td>-</td>"
        )
    );
    assert_eq!(
        row("mac-1"),
        format!(
            "<tr><td>mac-1</td><td>darwin (kbf-daemon)</td><td>-</td><td>aaaaaaaaaaaa</td>\
             <td>{COMMIT_B}</td><td>no</td><td>refused</td>\
             <td><span class=\"pill ok\">ok</span></td><td>no helper installed</td>"
        )
    );
    // Never reported: no state, not judged, unsupported (never idle); no detail row.
    assert_eq!(
        row("old-1"),
        "<tr><td>old-1</td><td>unknown</td><td>-</td><td>-</td><td>-</td>\
         <td>not judged</td><td>-</td>\
         <td><span class=\"pill pending\">unsupported</span></td><td>-</td>"
    );
}

/// Catches: an availability or role not shown as the server says it: `unavailable` is a
/// failure colour, an availability this page does not know is shown as it is.
#[test]
fn availability_and_role_are_shown_as_the_server_sends_them() {
    let mut nodes = nodes_doc();
    let mut gone = node(
        "gone-1",
        Some("Linux"),
        &json!({"state": null, "up_to_date": null, "availability": "unavailable"}),
    );
    gone["role"] = json!("storage");
    let odd = node(
        "odd-1",
        Some("Linux"),
        &json!({"state": "future_state", "up_to_date": null, "availability": "sideways"}),
    );
    nodes["nodes"] = json!([gone, odd]);
    let fake = Fake::default()
        .json("/v1/update", &update_doc())
        .json("/v1/nodes", &nodes)
        .json(HOSTS_URL, &hosts_doc());
    let (_, html) = page(&fake);
    assert!(html.contains("<td>storage</td>"), "{html}");
    assert!(
        html.contains("<span class=\"pill fail\">unavailable</span>"),
        "{html}"
    );
    assert!(
        html.contains("<span class=\"pill other\">sideways</span>"),
        "{html}"
    );
    assert!(html.contains("<td>future_state</td>"), "{html}");
}

/// Catches: the detail columns shown when the detail route refused, or the page failing
/// with it; a guess in their place. The summary stays. Mutants: detail columns always
/// shown; the page failing on a refused detail; a refused route's message dropped.
#[test]
fn a_refused_detail_route_removes_the_detail_columns_and_keeps_the_summary() {
    let refused = |fake: Fake| fake.with(HOSTS_URL, 401, r#"{"error": "needs a read grant"}"#);
    let unreadable = |fake: Fake| fake.with(HOSTS_URL, 200, "{}");
    let down = |fake: Fake| fake.down(HOSTS_URL);
    let open_only = || {
        Fake::default()
            .json("/v1/update", &update_doc())
            .json("/v1/nodes", &nodes_doc())
    };
    let cases: [(Fake, &str); 3] = [
        (
            refused(open_only()),
            "the detail route answered 401; it needs read access.",
        ),
        (unreadable(open_only()), "a document this page cannot read"),
        (down(open_only()), "connection refused"),
    ];
    for (fake, why) in cases {
        let (status, html) = page(&fake);
        assert_eq!(status, 200, "{html}");
        assert!(
            html.contains("Commits and last errors are not shown: "),
            "{html}"
        );
        assert!(html.contains(why), "{why}: {html}");
        // The releases table has a Desired column of its own: look at the hosts.
        let hosts = &html[html.find("<h2>Hosts</h2>").expect("hosts")..];
        for column in ["Running", "Desired", "Last error"] {
            assert!(
                !hosts.contains(&format!("<th scope=\"col\">{column}</th>")),
                "{html}"
            );
        }
        assert!(!html.contains("no helper installed"), "{html}");
        // The open summary of every host.
        assert!(
            html.contains(
                "<tr><td>mac-1</td><td>darwin (kbf-daemon)</td><td>-</td><td>no</td>\
                 <td>refused</td><td><span class=\"pill ok\">ok</span></td></tr>"
            ),
            "{html}"
        );
        assert!(html.contains(COMMIT_B), "the farm block stays: {html}");
    }
}

/// Catches: a detail page that is cut without saying so. Mutant: the note dropped.
#[test]
fn a_detail_with_a_next_page_says_it_is_the_first_1000() {
    let mut hosts = hosts_doc();
    hosts["next_page_token"] = json!("mac-1");
    let fake = updates_farm().json(HOSTS_URL, &hosts);
    let (_, html) = page(&fake);
    assert!(
        html.contains("The detail of the first 1000 hosts is shown."),
        "{html}"
    );
    let (_, html) = page(&updates_farm());
    assert!(!html.contains("first 1000"), "{html}");
}

/// Catches: no host registered not said.
#[test]
fn no_registered_host_is_said() {
    let mut nodes = nodes_doc();
    nodes["nodes"] = json!([]);
    let fake = updates_farm().json("/v1/nodes", &nodes);
    let (_, html) = page(&fake);
    assert!(html.contains("No host has registered."), "{html}");
    assert!(
        html.contains("<dd>0 of 0 judged, 0 hosts in all</dd>"),
        "{html}"
    );
}

/// Catches: a string from outside written as markup: a refusal, a node id, a role, a
/// commit, a release name, the frontend's commit, a platform. Mutant: any of them
/// written raw.
#[test]
fn strings_from_outside_render_as_text() {
    let evil = "<script>alert(1)</script>";
    let mut update = update_doc();
    update["desired"]["commit"] = json!(format!("<b>{evil}"));
    update["releases"]["releases"][0]["commit"] = json!(format!("<i>{evil}"));
    update["releases"]["releases"][0]["platforms"][0]["platform"] = json!(format!("<u>{evil}"));
    let mut nodes = nodes_doc();
    nodes["server"]["commit"] = json!(format!("<em>{evil}"));
    nodes["nodes"][0]["node_id"] = json!(format!("<h1>{evil}"));
    nodes["nodes"][0]["role"] = json!(format!("<p>{evil}"));
    nodes["nodes"][0]["update"]["state"] = json!(format!("<s>{evil}"));
    let mut hosts = hosts_doc();
    hosts["hosts"][0]["node_id"] = json!(format!("<h1>{evil}"));
    hosts["hosts"][0]["refusal"] = json!(format!("<img src=x onerror=alert(1)>{evil}"));
    hosts["hosts"][0]["running_commit"] = json!(format!("<a href=x>{evil}"));
    let fake = Fake::default()
        .json("/v1/update", &update)
        .json("/v1/nodes", &nodes)
        .json(HOSTS_URL, &hosts);
    let (status, html) = page(&fake);
    assert_eq!(status, 200, "{html}");
    assert!(!html.contains("<script"), "{html}");
    for tag in [
        "<b>",
        "<i>",
        "<u>",
        "<em>",
        "<h1><",
        "<s>",
        "<img",
        "<a href=x",
    ] {
        assert!(!html.contains(tag), "{tag}: {html}");
    }
    assert_eq!(
        html.matches("&lt;script&gt;alert(1)&lt;/script&gt;")
            .count(),
        9,
        "{html}"
    );
}

/// Catches: an error string from the API written as markup: the page of a failed read
/// shows it as text, with the API's status. Mutant: the error written raw.
#[test]
fn an_error_with_markup_renders_as_text() {
    for route in ["/v1/update", "/v1/nodes"] {
        let evil = r#"{"error": "<script>alert(1)</script> & <b>"}"#;
        let fake = updates_farm().with(route, 503, evil);
        let answer = render(&fake, "/ui/updates");
        let html = body(&answer);
        assert_eq!(answer.status(), 503, "{route}");
        assert!(!html.contains("<script"), "{html}");
        assert!(!html.contains("<b>"), "{html}");
        assert!(
            html.contains("&lt;script&gt;alert(1)&lt;/script&gt; &amp; &lt;b&gt;"),
            "{html}"
        );
    }
    // A source that cannot be asked, and a document that is not one.
    let answer = render(&updates_farm().down("/v1/update"), "/ui/updates");
    assert_eq!(answer.status(), 502);
    let answer = render(&updates_farm().with("/v1/nodes", 200, "[]"), "/ui/updates");
    assert_eq!(answer.status(), 502);
    assert!(body(&answer).contains("a document this page cannot read"));
}

/// Catches: a token, or anything token-like, in the page: the page shows the command
/// with placeholders and nothing from a document it read beyond the keys it names; it
/// has no form, no script and sets no cookie. Mutants: the whole document dumped;
/// a `Bearer` followed by a value; a form or a cookie added; the generation not the
/// one read.
#[test]
fn the_command_is_text_with_placeholders_and_no_token_is_ever_rendered() {
    let secret = "tok-0123456789abcdef-SECRET";
    let plant = |mut v: Value| {
        v["api_token"] = json!(secret);
        v["token"] = json!(secret);
        v["authorization"] = json!(format!("Bearer {secret}"));
        v
    };
    let mut nodes = plant(nodes_doc());
    nodes["nodes"][0] = plant(nodes["nodes"][0].clone());
    let mut update = plant(update_doc());
    update["desired"] = plant(update["desired"].clone());
    let mut hosts = plant(hosts_doc());
    hosts["hosts"][0] = plant(hosts["hosts"][0].clone());
    let fake = Fake::default()
        .json("/v1/update", &update)
        .json("/v1/nodes", &nodes)
        .json(HOSTS_URL, &hosts);
    let answer = render(&fake, "/ui/updates");
    let html = body(&answer);
    assert_eq!(answer.status(), 200, "{html}");
    assert!(!html.contains(secret), "{html}");
    assert!(!html.contains("SECRET"), "{html}");
    // Every `Bearer` is the placeholder; there is one.
    assert_eq!(html.matches("Bearer").count(), 1, "{html}");
    assert!(
        html.contains("Bearer $(cat &lt;api-token-file&gt;)"),
        "{html}"
    );
    for text in [
        "curl -X POST http://127.0.0.1:&lt;api-port&gt;/v1/update/desired",
        "&quot;commit&quot;:&quot;&lt;40 hex digits of a complete release&gt;&quot;",
        "&quot;expect_generation&quot;:7}",
    ] {
        assert!(html.contains(text), "{text}: {html}");
    }
    // Read-only: no form, no script, no cookie, no input.
    for forbidden in ["<form", "<script", "<input", "<button", "cookie"] {
        assert!(
            !html.to_ascii_lowercase().contains(forbidden),
            "{forbidden}"
        );
    }
    assert!(
        answer
            .headers()
            .iter()
            .all(|(k, _)| !k.eq_ignore_ascii_case("set-cookie"))
    );
    // Before anything is offered the generation is 0.
    let mut update = update_doc();
    update["desired"] = Value::Null;
    let (_, html) = page(&farm_with(&update));
    assert!(html.contains("&quot;expect_generation&quot;:0}"), "{html}");
}

/// Catches: a read of a route the page does not declare, or a declaration of a route
/// that writes (the contract has no method: a page reads by GET, so its routes are
/// read routes). Mutants: `/v1/update/desired`, `/v1/update/pause` or `/v1/releases`
/// declared; the page asking for another URL (it is refused before it is sent, and the
/// exact reads are fixed here).
#[test]
fn the_page_reads_exactly_its_three_routes_and_no_other() {
    for url in [
        "/v1/update/desired",
        "/v1/update/pause",
        "/v1/update/resume",
        "/v1/update/plan",
        "/v1/releases",
        "/v1/members",
        "/v1/update/hosts/x",
        "/v1/update/..",
    ] {
        assert!(!UPDATES.declares(url), "{url}");
        let fake = updates_farm();
        let mut r = Reader::new(&fake, &UPDATES);
        let e = block_on(r.json::<Value>(url)).expect_err("refused");
        assert!(e.is_page_bug(), "{url}");
        assert!(fake.seen().is_empty(), "{url} was sent");
    }
    assert_eq!(
        UPDATES.reads,
        ["/v1/update", "/v1/nodes", "/v1/update/hosts"]
    );
    let fake = updates_farm();
    let _ = page(&fake);
    assert_eq!(fake.seen(), ["/v1/update", "/v1/nodes", HOSTS_URL]);
}

/// Catches: a time written wrongly: the epoch, a leap day, a time of day.
#[test]
fn a_time_is_written_as_rfc_3339() {
    assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
    assert_eq!(rfc3339(951_782_400_000), "2000-02-29T00:00:00Z");
    assert_eq!(rfc3339(1_709_210_096_999), "2024-02-29T12:34:56Z");
    assert_eq!(rfc3339(1_791_370_000_000), "2026-10-07T10:46:40Z");
}

/// Catches: routing that answers the page under another path.
#[test]
fn only_the_updates_path_is_the_page() {
    let fake = updates_farm();
    for path in [
        "/ui/updates/",
        "/ui/updates/x",
        "/ui/update",
        "/ui/updatesx",
    ] {
        assert_eq!(render(&fake, path).status(), 404, "{path}");
    }
    assert!(fake.seen().is_empty());
}
