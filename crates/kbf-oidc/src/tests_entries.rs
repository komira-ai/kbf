//! The allow-list with several entries: each token is admitted by the entry it matches and
//! gets that entry's services, and a file that could be read two ways is refused.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};

use super::testkit::{
    AUDIENCE, STATIC_ISSUER, StaticKeys, WORKFLOW, clock, good_claims, jwks_of, keys, mint,
};
use super::{AllowList, ConfigError, OidcConfig, Refusal, Verifier};

const OTHER_WORKFLOW: &str = "komira-ai/jobscheduler/.github/workflows/release.yml@refs/heads/main";

fn entry(header: &str, id: &str, workflow: &str, extra: &str) -> String {
    format!(
        "{header}\nissuer = \"{STATIC_ISSUER}\"\naudience = \"{AUDIENCE}\"\nrepository_id = \"{id}\"\n\
         job_workflow_ref = [\"{workflow}\"]\nrefs = [\"refs/heads/main\"]\n{extra}\n"
    )
}

fn two_entries() -> String {
    format!(
        "{}{}",
        entry("[[remote_operator]]", "4242", WORKFLOW, ""),
        entry(
            "[[remote_operator]]",
            "777",
            OTHER_WORKFLOW,
            "services = [\"jobscheduler\", \"jobscheduler-worker\"]"
        ),
    )
}

fn verifier(text: &str) -> Verifier {
    let (clock, _) = clock();
    Verifier::new(
        AllowList::parse(text).expect("an allow-list"),
        Arc::new(StaticKeys(jwks_of(&[&keys()[0]]))),
        Duration::from_secs(30),
        clock,
    )
}

/// Claims of a token from the first entry's repository, changed to `mutate`.
fn token(mutate: impl FnOnce(&mut Value)) -> String {
    let mut claims = good_claims(STATIC_ISSUER);
    mutate(&mut claims);
    mint(&keys()[0], &claims)
}

fn second_entry_claims(c: &mut Value) {
    c["repository_id"] = json!("777");
    c["job_workflow_ref"] = json!(OTHER_WORKFLOW);
    c["ref"] = json!("refs/heads/main");
}

/// The single `[remote_operator]` table means what it meant: it parses to one entry whose
/// services are `kbf`, the token of the old form verifies and says entry 0 and `kbf`, and
/// `OidcConfig::parse` and `AllowList::parse` read it alike.
#[tokio::test]
async fn the_single_table_form_is_one_entry_for_kbf() {
    let text = entry("[remote_operator]", "4242", WORKFLOW, "");
    let list = AllowList::parse(&text).expect("list");
    assert_eq!(list.entries(), [OidcConfig::parse(&text).expect("one")]);
    assert_eq!(list.entries()[0].services, ["kbf"]);
    let who = verifier(&text)
        .verify(&token(|_| {}))
        .await
        .expect("admitted");
    assert_eq!((who.entry, who.services), (0, vec!["kbf".to_owned()]));
}

/// A `services` list in the single table is carried to the verified token, and an empty
/// list, or one with a blank name, is refused.
#[tokio::test]
async fn an_entry_may_name_its_services() {
    let text = entry(
        "[remote_operator]",
        "4242",
        WORKFLOW,
        "services = [\"a\", \"b\"]",
    );
    let who = verifier(&text).verify(&token(|_| {})).await.expect("ok");
    assert_eq!(who.services, ["a", "b"]);
    for bad in ["services = []", "services = [\" a\"]", "services = [\"\"]"] {
        let text = entry("[remote_operator]", "4242", WORKFLOW, bad);
        assert!(AllowList::parse(&text).is_err(), "{bad}");
    }
}

/// Two entries: a token of each is admitted by its own entry and gets its own services
/// (the second's default does not leak, the first's `kbf` default does not widen).
#[tokio::test]
async fn each_token_gets_the_services_of_its_own_entry() {
    let verifier = verifier(&two_entries());
    let a = verifier.verify(&token(|_| {})).await.expect("a");
    assert_eq!((a.entry, a.services), (0, vec!["kbf".to_owned()]));
    let b = verifier
        .verify(&token(second_entry_claims))
        .await
        .expect("b");
    assert_eq!(b.entry, 1);
    assert_eq!(b.services, ["jobscheduler", "jobscheduler-worker"]);
}

/// A token of entry B cannot obtain entry A's services: B's repository with A's workflow,
/// A's repository with B's workflow, and B's repository and workflow at a ref only A
/// lists are all refused, and a token that matches neither is refused with the first
/// entry's reason.
#[tokio::test]
async fn a_token_of_one_entry_cannot_borrow_the_other() {
    let text = format!(
        "{}{}",
        entry("[[remote_operator]]", "4242", WORKFLOW, ""),
        entry("[[remote_operator]]", "777", OTHER_WORKFLOW, "")
            .replace("refs = [\"refs/heads/main\"]", "refs = [\"refs/heads/b\"]"),
    );
    let verifier = verifier(&text);
    let cross = |id: &'static str, wf: &'static str| {
        token(move |c| {
            c["repository_id"] = json!(id);
            c["job_workflow_ref"] = json!(wf);
        })
    };
    assert_eq!(
        verifier.verify(&cross("777", WORKFLOW)).await,
        Err(Refusal::Repository)
    );
    assert_eq!(
        verifier.verify(&cross("4242", OTHER_WORKFLOW)).await,
        Err(Refusal::Workflow)
    );
    // B's repository and workflow on A's ref: B's entry refuses the ref, A's the repository.
    assert!(
        verifier
            .verify(&cross("777", OTHER_WORKFLOW))
            .await
            .is_err()
    );
    let on_b = token(|c| {
        second_entry_claims(c);
        c["ref"] = json!("refs/heads/b");
    });
    let who = verifier.verify(&on_b).await.expect("b on its own ref");
    assert_eq!((who.entry, who.services), (1, vec!["kbf".to_owned()]));
    let neither = token(|c| c["repository_id"] = json!("5"));
    assert_eq!(verifier.verify(&neither).await, Err(Refusal::Repository));
}

/// A token of one entry cannot borrow the other's environment, ref or event: entry A allows
/// only environment `gamma`, ref `refs/heads/main` and `push`; entry B only `prod`,
/// `refs/heads/b` and `workflow_dispatch`. A token of B's repository and workflow with any
/// one of A's three values (the rest B's own) is refused, and so is A's with one of B's;
/// with all of its own entry's values each is admitted by that entry.
#[tokio::test]
async fn no_entry_lends_its_environment_ref_or_event() {
    let fields =
        |env: &str, event: &str| format!("environments = [\"{env}\"]\nevent_names = [\"{event}\"]");
    let a = entry(
        "[[remote_operator]]",
        "4242",
        WORKFLOW,
        &fields("gamma", "push"),
    );
    let b = entry(
        "[[remote_operator]]",
        "777",
        OTHER_WORKFLOW,
        &fields("prod", "workflow_dispatch"),
    )
    .replace("refs = [\"refs/heads/main\"]", "refs = [\"refs/heads/b\"]");
    let verifier = verifier(&format!("{a}{b}"));
    let own_a = [
        ("environment", "gamma"),
        ("ref", "refs/heads/main"),
        ("event_name", "push"),
    ];
    let own_b = [
        ("environment", "prod"),
        ("ref", "refs/heads/b"),
        ("event_name", "workflow_dispatch"),
    ];
    let claims = |id: &'static str, wf: &'static str, values: [(&'static str, &'static str); 3]| {
        token(move |c| {
            c["repository_id"] = json!(id);
            c["job_workflow_ref"] = json!(wf);
            for (field, value) in values {
                c[field] = json!(value);
            }
        })
    };
    let who = verifier
        .verify(&claims("4242", WORKFLOW, own_a))
        .await
        .expect("a with its own values");
    assert_eq!(who.entry, 0);
    let who = verifier
        .verify(&claims("777", OTHER_WORKFLOW, own_b))
        .await
        .expect("b with its own values");
    assert_eq!(who.entry, 1);
    for i in 0..3 {
        let mut b_with_a = own_b;
        b_with_a[i] = own_a[i];
        assert!(
            verifier
                .verify(&claims("777", OTHER_WORKFLOW, b_with_a))
                .await
                .is_err(),
            "b with a's {}",
            own_a[i].0
        );
        let mut a_with_b = own_a;
        a_with_b[i] = own_b[i];
        assert!(
            verifier
                .verify(&claims("4242", WORKFLOW, a_with_b))
                .await
                .is_err(),
            "a with b's {}",
            own_b[i].0
        );
    }
}

/// A token refused by every entry does not spend its `jti`; the admitted one does.
#[tokio::test]
async fn only_the_admitting_entry_spends_the_jti() {
    let verifier = verifier(&two_entries());
    let good = token(second_entry_claims);
    assert!(verifier.verify(&good).await.is_ok());
    assert_eq!(verifier.verify(&good).await, Err(Refusal::Replayed));
}

/// A mix of `[remote_operator]` and `[[remote_operator]]` is refused, and so is an empty
/// list, a scalar, an array of scalars, and several entries read as one.
#[test]
fn forms_that_can_be_read_two_ways_are_refused() {
    let one = entry("[remote_operator]", "4242", WORKFLOW, "");
    let many = entry("[[remote_operator]]", "777", OTHER_WORKFLOW, "");
    assert!(matches!(
        AllowList::parse(&format!("{one}{many}")),
        Err(ConfigError::Parse(_))
    ));
    assert!(matches!(
        AllowList::parse(&format!("{many}{one}")),
        Err(ConfigError::Parse(_))
    ));
    for text in [
        "remote_operator = []",
        "remote_operator = 3",
        "remote_operator = [1]",
    ] {
        assert!(AllowList::parse(text).is_err(), "{text}");
    }
    assert!(AllowList::parse("").is_err(), "absent");
    assert!(
        OidcConfig::parse(&two_entries()).is_err(),
        "several, read as one"
    );
    assert!(
        OidcConfig::parse(&many).is_ok(),
        "a list of one is one entry"
    );
}

/// Two entries for the same repository and workflow are refused at load, also when the
/// workflow is one of several; the same workflow of different repositories, and the same
/// repository with different workflows, are not duplicates.
#[test]
fn two_entries_for_one_repository_and_workflow_are_refused() {
    let a = entry("[[remote_operator]]", "4242", WORKFLOW, "");
    let again = entry(
        "[[remote_operator]]",
        "4242",
        WORKFLOW,
        "services = [\"x\"]",
    );
    let why = AllowList::parse(&format!("{a}{again}")).expect_err("duplicate");
    assert!(why.to_string().contains("4242"), "{why}");
    let wider = entry("[[remote_operator]]", "4242", OTHER_WORKFLOW, "").replace(
        &format!("[\"{OTHER_WORKFLOW}\"]"),
        &format!("[\"{OTHER_WORKFLOW}\", \"{WORKFLOW}\"]"),
    );
    assert!(AllowList::parse(&format!("{a}{wider}")).is_err());
    let other_workflow = entry("[[remote_operator]]", "4242", OTHER_WORKFLOW, "");
    assert!(AllowList::parse(&format!("{a}{other_workflow}")).is_ok());
    let other_repo = entry("[[remote_operator]]", "9", WORKFLOW, "");
    assert!(AllowList::parse(&format!("{a}{other_repo}")).is_ok());
}

/// Entries of different issuers are refused (the keys are one issuer's), and an entry
/// that is itself invalid refuses the file.
#[test]
fn entries_share_an_issuer_and_each_must_be_valid() {
    let a = entry("[[remote_operator]]", "4242", WORKFLOW, "");
    let b = entry("[[remote_operator]]", "777", OTHER_WORKFLOW, "")
        .replace(STATIC_ISSUER, "https://other.example");
    assert!(AllowList::parse(&format!("{a}{b}")).is_err());
    let bad = entry("[[remote_operator]]", "x", OTHER_WORKFLOW, "");
    assert!(AllowList::parse(&format!("{a}{bad}")).is_err());
    let unknown = entry("[[remote_operator]]", "777", OTHER_WORKFLOW, "surprise = 1");
    assert!(matches!(
        AllowList::parse(&format!("{a}{unknown}")),
        Err(ConfigError::Parse(_))
    ));
}

/// A file that is not there is a read error for both readers.
#[test]
fn a_missing_file_is_a_read_error() {
    let path = std::path::Path::new("/nonexistent/ci.toml");
    assert!(matches!(
        AllowList::from_file(path),
        Err(ConfigError::Read { .. })
    ));
}
