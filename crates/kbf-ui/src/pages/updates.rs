//! The Updates page: which release the farm wants, which commit each host runs and
//! where its update step is. Read-only: it has no form, no button and no credential;
//! the command that sets the desired release is shown as text for an operator to run
//! (`docs/design/update-ui.md` 4).

use std::collections::HashMap;

use super::{definitions, failed, pill};
use crate::api::{Desired, HostsDoc, Node, NodesDoc, Pause, Release, UpdateDoc};
use crate::html::{Html, attr};
use crate::layout;
use crate::read::{Page, ReadError, Reader};
use crate::{Answer, Source};

/// The page's path.
pub(crate) const UPDATES_PATH: &str = "/ui/updates";

/// The per-host detail, asked for as many hosts as one page may hold.
const HOSTS_URL: &str = "/v1/update/hosts?page_size=1000";

/// Updates: the open summary, and the detail where its route answers.
pub(crate) const UPDATES: Page = Page {
    name: "Updates",
    reads: &["/v1/update", "/v1/nodes", "/v1/update/hosts"],
};

/// The operator-API command that sets the desired release: placeholders for the
/// port, the token file and the commit, never a token. `{generation}` is the one the
/// page read, so a second operator's change makes the call `409`.
const COMMAND: &str = "curl -X POST http://127.0.0.1:<api-port>/v1/update/desired \\\n  \
     -H \"Authorization: Bearer $(cat <api-token-file>)\" \\\n  \
     -H \"Content-Type: application/json\" \\\n  \
     -d '{\"commit\":\"<40 hex digits of a complete release>\",\"expect_generation\":GENERATION}'";

/// The Updates page.
pub(crate) async fn updates<S: Source>(source: &S) -> Answer {
    let mut r = Reader::new(source, &UPDATES);
    let loaded = load(&mut r).await;
    let (update, nodes, detail) = match loaded {
        Ok(docs) => docs,
        Err(e) => return failed(&UPDATES, UPDATES_PATH, r.asked(), &e),
    };
    let body = layout::page(UPDATES.name, UPDATES_PATH, r.asked(), |h| {
        h.el("h1", &[], |h| {
            h.text("Updates");
        });
        farm_block(h, &update, &nodes.nodes);
        releases(h, update.desired.as_ref(), &update.releases.releases);
        frontend(h, &nodes.server.commit);
        hosts(h, &nodes.nodes, &detail);
        command(h, update.desired.as_ref());
    });
    Answer::html(200, body)
}

/// What the detail route gave: its hosts, or why there are none.
enum Detail {
    Hosts {
        by_node: HashMap<String, crate::api::Host>,
        more: bool,
    },
    Absent(ReadError),
}

async fn load<S: Source>(
    r: &mut Reader<'_, S>,
) -> Result<(UpdateDoc, NodesDoc, Detail), ReadError> {
    let update: UpdateDoc = r.json("/v1/update").await?;
    let nodes: NodesDoc = r.json("/v1/nodes").await?;
    // A refused or broken detail route removes the detail columns; it never fails
    // the page, and nothing is filled in for it.
    let detail = match r.json::<HostsDoc>(HOSTS_URL).await {
        Ok(doc) => Detail::Hosts {
            more: doc.next_page_token.is_some(),
            by_node: doc
                .hosts
                .into_iter()
                .map(|h| (h.node_id.clone(), h))
                .collect(),
        },
        Err(e) => Detail::Absent(e),
    };
    Ok((update, nodes, detail))
}

fn farm_block(h: &mut Html, update: &UpdateDoc, nodes: &[Node]) {
    h.el("section", &[attr("class", "card")], |h| {
        h.el("h2", &[], |h| {
            h.text("Farm");
        });
        let (words, class) = match update.pause() {
            Pause::Paused => ("paused", "fail"),
            Pause::Running => ("running", "ok"),
            Pause::Unknown => ("pause state unknown", "pending"),
        };
        h.el("p", &[], |h| pill(h, words, class));
        let desired = update.desired.as_ref().map_or_else(
            || "none offered yet".to_owned(),
            |d| format!("{} (generation {})", d.commit, d.generation),
        );
        let set_at = update
            .desired
            .as_ref()
            .map_or_else(|| "-".to_owned(), |d| rfc3339(d.set_at_ms));
        let flag = |key| match update.policy_flag(key) {
            Some(true) => "on",
            Some(false) => "off",
            None => "not set",
        };
        let judged = nodes
            .iter()
            .filter(|n| n.update.up_to_date.is_some())
            .count();
        let current = nodes
            .iter()
            .filter(|n| n.update.up_to_date == Some(true))
            .count();
        let fields: [(&str, String); 5] = [
            ("Desired release", desired),
            ("Set at", set_at),
            ("Policy auto_nodes", flag("auto_nodes").to_owned()),
            ("Policy auto_frontend", flag("auto_frontend").to_owned()),
            (
                "Hosts up to date",
                format!("{current} of {judged} judged, {} hosts in all", nodes.len()),
            ),
        ];
        definitions(h, &fields);
    });
}

/// The release store's commits, each platform complete or not with the component it
/// needs. The store lists commits in commit order, not time, so the page does not
/// say which is newest.
fn releases(h: &mut Html, desired: Option<&Desired>, releases: &[Release]) {
    h.el("h2", &[], |h| {
        h.text("Releases");
    });
    if releases.is_empty() {
        h.el("p", &[attr("class", "hint")], |h| {
            h.text("The release store holds no release.");
        });
        return;
    }
    h.el("table", &[attr("class", "rows")], |h| {
        h.el("thead", &[], |h| {
            h.el("tr", &[], |h| {
                for name in ["Commit", "Desired", "Platforms"] {
                    h.el("th", &[attr("scope", "col")], |h| {
                        h.text(name);
                    });
                }
            });
        });
        h.el("tbody", &[], |h| {
            for release in releases {
                let is_desired = desired.is_some_and(|d| d.commit == release.commit);
                let platforms: Vec<String> = release
                    .platforms
                    .iter()
                    .map(|p| {
                        let component = release_component(&p.platform);
                        let state = if p.complete {
                            "complete"
                        } else {
                            "incomplete, needs"
                        };
                        format!("{}: {state} ({component})", p.platform)
                    })
                    .collect();
                h.el("tr", &[], |h| {
                    h.el("td", &[], |h| {
                        h.el("code", &[], |h| {
                            h.text(&release.commit);
                        });
                    });
                    h.el("td", &[], |h| {
                        h.text(if is_desired { "yes" } else { "" });
                    });
                    h.el("td", &[], |h| {
                        h.text(&platforms.join("; "));
                    });
                });
            }
        });
    });
}

/// The component a release platform needs to be complete: linux `kbf`, darwin
/// `kbf-daemon`.
fn release_component(platform: &str) -> &'static str {
    if platform.starts_with("darwin") {
        "kbf-daemon"
    } else if platform.starts_with("linux") {
        "kbf"
    } else {
        "unknown platform"
    }
}

/// A node's platform and the component it runs, from its OS name; `None` before it
/// reported software.
fn node_platform(node: &Node) -> Option<(&'static str, &'static str)> {
    let os = node.software.as_ref()?.os_name.to_ascii_lowercase();
    Some(if os == "macos" || os == "darwin" {
        ("darwin", "kbf-daemon")
    } else {
        ("linux", "kbf")
    })
}

/// The frontend is not a host of the node roll: it updates only through the frontend
/// switch behind Envoy.
fn frontend(h: &mut Html, commit: &str) {
    h.el("h2", &[], |h| {
        h.text("Frontend");
    });
    let fields: [(&str, String); 2] = [
        ("Commit", commit.to_owned()),
        (
            "Updates",
            "only through the frontend switch behind Envoy, never by the node roll; \
             it is not one of the hosts below"
                .to_owned(),
        ),
    ];
    definitions(h, &fields);
}

fn hosts(h: &mut Html, nodes: &[Node], detail: &Detail) {
    h.el("h2", &[], |h| {
        h.text("Hosts");
    });
    match detail {
        Detail::Hosts { more: true, .. } => {
            h.el("p", &[attr("class", "hint")], |h| {
                h.text("The detail of the first 1000 hosts is shown.");
            });
        }
        Detail::Hosts { .. } => {}
        Detail::Absent(e) => {
            h.el("p", &[attr("class", "hint")], |h| {
                h.text("Commits and last errors are not shown: ");
                h.text(&absent_reason(e));
            });
        }
    }
    if nodes.is_empty() {
        h.el("p", &[attr("class", "hint")], |h| {
            h.text("No host has registered.");
        });
        return;
    }
    let by_node = match detail {
        Detail::Hosts { by_node, .. } => Some(by_node),
        Detail::Absent(_) => None,
    };
    h.el("table", &[attr("class", "rows")], |h| {
        h.el("thead", &[], |h| {
            h.el("tr", &[], |h| {
                let detail_columns = ["Running", "Desired"];
                let head = ["Node", "Platform", "Role"]
                    .into_iter()
                    .chain(by_node.iter().flat_map(|_| detail_columns))
                    .chain(["Up to date", "Step", "Availability"])
                    .chain(by_node.iter().map(|_| "Last error"));
                for name in head {
                    h.el("th", &[attr("scope", "col")], |h| {
                        h.text(name);
                    });
                }
            });
        });
        h.el("tbody", &[], |h| {
            for node in nodes {
                host_row(h, node, by_node);
            }
        });
    });
}

/// Why the detail columns are absent, in words: a refused route is the common case.
fn absent_reason(e: &ReadError) -> String {
    match e {
        ReadError::Status { status, .. } => {
            format!("the detail route answered {status}; it needs read access.")
        }
        other => format!("{other}."),
    }
}

fn host_row(h: &mut Html, node: &Node, by_node: Option<&HashMap<String, crate::api::Host>>) {
    let platform = node_platform(node).map_or_else(
        || "unknown".to_owned(),
        |(os, component)| format!("{os} ({component})"),
    );
    let up_to_date = match node.update.up_to_date {
        Some(true) => "yes",
        Some(false) => "no",
        None => "not judged",
    };
    let availability = node.update.availability.as_str();
    let class = match availability {
        "ok" => "ok",
        "unavailable" => "fail",
        "unsupported" => "pending",
        _ => "other",
    };
    let detail = by_node.map(|m| m.get(&node.node_id));
    let cell = |h: &mut Html, text: &str| {
        h.el("td", &[], |h| {
            h.text(text);
        });
    };
    h.el("tr", &[], |h| {
        cell(h, &node.node_id);
        cell(h, &platform);
        cell(h, node.role.as_deref().unwrap_or("-"));
        if let Some(host) = detail {
            let commit = |c: Option<&Option<String>>| {
                c.and_then(Option::as_deref)
                    .filter(|c| !c.is_empty())
                    .unwrap_or("-")
                    .to_owned()
            };
            cell(h, &commit(host.map(|x| &x.running_commit)));
            cell(h, &commit(host.map(|x| &x.desired_commit)));
        }
        cell(h, up_to_date);
        cell(h, node.update.state.as_deref().unwrap_or("-"));
        h.el("td", &[], |h| pill(h, availability, class));
        if let Some(host) = detail {
            let refusal = host
                .and_then(|x| x.refusal.as_deref())
                .filter(|r| !r.is_empty())
                .unwrap_or("-");
            cell(h, refusal);
        }
    });
}

/// How to set the desired release: text for an operator, with placeholders, no token.
fn command(h: &mut Html, desired: Option<&Desired>) {
    h.el("h2", &[], |h| {
        h.text("Set the desired release");
    });
    h.el("p", &[], |h| {
        h.text(
            "This page changes nothing. On the server's host, run this with the API's port \
             and token file filled in, and the commit of a complete release above:",
        );
    });
    let generation = desired.map_or(0, |d| d.generation);
    let text = COMMAND.replace("GENERATION", &generation.to_string());
    h.el("pre", &[], |h| {
        h.el("code", &[], |h| {
            h.text(&text);
        });
    });
    h.el("p", &[attr("class", "hint")], |h| {
        h.text(
            "The call is refused with 409 if the generation has moved since this page was \
             read, so a second operator's change is not overwritten.",
        );
    });
}

/// `ms` since the Unix epoch as an RFC 3339 UTC time.
fn rfc3339(ms: u64) -> String {
    let secs = ms / 1000;
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // Civil date from days since 1970-01-01 (proleptic Gregorian; Hinnant's algorithm).
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z % 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests;
