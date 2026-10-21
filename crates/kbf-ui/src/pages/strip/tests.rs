//! The strip's count line, grid lookup and URL, over documents built by hand.

use serde_json::json;

use super::{GridDoc, Run, grid_url, history, tally};

fn runs<'a>(statuses: &[&'a str]) -> Vec<Run<'a>> {
    statuses
        .iter()
        .map(|status| Run {
            build: "b",
            first_seen: "t",
            status,
        })
        .collect()
}

/// Catches: a count line that counts the wrong cells: the failures out of every build
/// instead of the builds that ran the target (a quiet target would read as clean), a
/// word the page does not know dropped from the line, or no builds read as "0 failed".
#[test]
fn the_count_line_names_every_outcome() {
    assert_eq!(tally(&[]), "No build is held in history.");
    assert_eq!(
        tally(&runs(&["absent", "absent"])),
        "None of the last 2 builds ran it."
    );
    assert_eq!(
        tally(&runs(&["pass", "fail", "fail", "pass", "pass"])),
        "2 failed of 5 builds that ran it (3 passed) in the last 5 builds."
    );
    assert_eq!(
        tally(&runs(&[
            "fail",
            "absent",
            "infra",
            "oom_killed",
            "weird",
            "weird"
        ])),
        "1 failed of 5 builds that ran it (0 passed, 1 infra, 1 oom_killed, 2 weird) \
         in the last 6 builds; 1 did not run it."
    );
}

/// Catches: the lookup taking another instance's or configuration's row, a row with a
/// cell for each build read by position the wrong way round, and a target no build ran
/// given a status.
#[test]
fn history_picks_the_row_by_instance_key_and_configuration() {
    let doc: GridDoc = serde_json::from_value(json!({
        "columns": [
            {"id": "new", "first_seen": "t2", "complete": true},
            {"id": "old", "first_seen": "t1", "complete": false},
        ],
        "rows": [
            {"instance": "a", "key": "k", "cfg": "c", "cells": [{"status": "fail"}, {"status": "fail"}]},
            {"instance": "b", "key": "k", "cfg": "c", "cells": [{"status": "fail"}, {"status": "fail"}]},
            {"instance": "a", "key": "k", "cfg": "d", "cells": [{"status": "fail"}, {"status": "fail"}]},
            {"instance": "a", "key": "j", "cfg": "c", "cells": [{"status": "fail"}, {"status": "fail"}]},
            {"instance": "z", "key": "z", "cfg": "z", "cells": [{"status": "pass"}, {"status": "absent"}]},
        ],
    }))
    .expect("a grid");
    let found = history(&doc, "z", "z", "z");
    let seen: Vec<(&str, &str)> = found.runs.iter().map(|r| (r.build, r.status)).collect();
    assert_eq!(seen, [("old", "absent"), ("new", "pass")]);
    assert_eq!(found.partial_builds, 1);
    let none = history(&doc, "z", "z", "other");
    assert!(none.runs.iter().all(|r| r.status == "absent"));
}

/// Catches: a grid URL that does not encode the shape list or the instance (an `&` in an
/// instance would start a parameter of its own), or asks for another number of builds.
#[test]
fn the_grid_url_encodes_its_parts() {
    assert_eq!(
        grid_url("aaaaaaaaaaaaaaaa,bbbbbbbbbbbbbbbb", 16, None),
        "/v1/grid?shapes=aaaaaaaaaaaaaaaa%2Cbbbbbbbbbbbbbbbb&last=16"
    );
    assert_eq!(
        grid_url("k", 50, Some("a&last=1")),
        "/v1/grid?shapes=k&last=50&instance=a%26last%3D1"
    );
}
