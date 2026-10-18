use super::*;

const B: &str = "0123456789abcdef0123456789abcdef01234567";

/// A status document as `kbf-installer` writes it, with `frontend` as given (the same section
/// literals are pinned in the installer's own tests, `frontend/status_tests.rs`: change one,
/// change the other).
fn status(frontend: &str) -> Vec<u8> {
    format!(
        "{{\"schema\":2,\"installed_commit\":\"{B}\",\"previous_commit\":null,\
         \"staged_commit\":null,\"state\":\"idle\",\"refusal\":\"\",\"acked_order_id\":0,\
         \"staged\":null,\"envoy\":{{\"current\":null,\"previous\":null}}{frontend}}}"
    )
    .into_bytes()
}

fn frontend(report: &[u8]) -> Option<FrontendReport> {
    parse(report).expect("the node fields are good").frontend
}

/// The section, and each word of its state, is read as written; a missing section (an older
/// installer), a section this client cannot read and one with a commit that is not hex all
/// leave the node fields readable and the section `None`, so a reader falls back to its own
/// deadline rather than a guess.
///
/// Mutants planted: a bad section made the whole file unreadable; the commit not checked; a
/// state word mapped to another.
#[test]
fn the_frontend_section_is_read_leniently() {
    for (word, state) in [
        ("pending", FrontendState::Pending),
        ("needs_operator", FrontendState::NeedsOperator),
        ("confirmed", FrontendState::Confirmed),
        ("reverted", FrontendState::Reverted),
        ("refused", FrontendState::Refused),
    ] {
        let doc = status(&format!(
            ",\"frontend\":{{\"order_id\":5,\"epoch\":3,\"commit\":\"{B}\",\"state\":\"{word}\",\
             \"refusal\":\"why\",\"later\":1}}"
        ));
        assert_eq!(
            frontend(&doc),
            Some(FrontendReport {
                order_id: 5,
                epoch: 3,
                commit: B.to_owned(),
                state,
                refusal: "why".to_owned(),
            })
        );
        assert_eq!(
            state.is_terminal(),
            matches!(
                state,
                FrontendState::Confirmed | FrontendState::Reverted | FrontendState::Refused
            )
        );
    }
    let no_refusal = status(&format!(
        ",\"frontend\":{{\"order_id\":5,\"epoch\":3,\"commit\":\"{B}\",\"state\":\"confirmed\"}}"
    ));
    assert_eq!(frontend(&no_refusal).expect("read").refusal, "");
    assert_eq!(frontend(&status("")), None, "an older installer");
    for bad in [
        ",\"frontend\":null",
        ",\"frontend\":7",
        ",\"frontend\":{\"order_id\":5}",
        &format!(
            ",\"frontend\":{{\"order_id\":5,\"epoch\":3,\"commit\":\"{B}\",\"state\":\"gone\"}}"
        ),
        ",\"frontend\":{\"order_id\":5,\"epoch\":3,\"commit\":\"zz\",\"state\":\"confirmed\"}",
    ] {
        assert_eq!(frontend(&status(bad)), None, "{bad}");
    }
}
