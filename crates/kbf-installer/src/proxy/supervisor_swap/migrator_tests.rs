//! The default migrator: which releases it lets through and the word of each refusal.

use std::sync::Arc;

use super::rig::Rig;
use super::{ExpandOnlyDefault, RunningSchema};
use crate::proxy::model::PortError;
use crate::proxy::ports::Migrator;
use crate::testkit::{B, C, c};

/// The rig stages release B with state schema 3.
fn migrator(embedded: u32, running: RunningSchema) -> (ExpandOnlyDefault, Arc<Rig>) {
    let rig = Rig::new(None);
    (ExpandOnlyDefault::new(rig.clone(), embedded, running), rig)
}

fn running(n: u32) -> RunningSchema {
    Box::new(move || Ok(n))
}

fn refused(r: Result<(), PortError>) -> String {
    r.expect_err("the migrator refuses").0
}

#[test]
fn a_release_on_the_schema_the_host_runs_passes() {
    let (m, _) = migrator(5, running(3));
    assert_eq!(m.migrate_forward(&c(B)), Ok(()));
}

#[test]
fn only_the_staged_release_passes() {
    let (m, rig) = migrator(5, running(3));
    let why = refused(m.migrate_forward(&c(C)));
    assert!(why.starts_with("not_staged:"), "{why}");
    rig.knobs(|k| k.staged = None);
    let why = refused(m.migrate_forward(&c(B)));
    assert!(why.starts_with("not_staged:"), "{why}");
}

#[test]
fn a_release_that_needs_a_newer_schema_than_this_installer_knows_is_installer_too_old() {
    // The running schema is never asked: nothing this build does could reach schema 3.
    let (m, _) = migrator(2, Box::new(|| panic!("the database is not asked")));
    let why = refused(m.migrate_forward(&c(B)));
    assert!(why.starts_with("installer_too_old:"), "{why}");
    assert!(why.contains('3') && why.contains('2'), "{why}");
}

#[test]
fn a_schema_other_than_the_one_the_host_runs_is_not_additive_whichever_way_it_differs() {
    for host in [2, 4] {
        let (m, _) = migrator(5, running(host));
        let why = refused(m.migrate_forward(&c(B)));
        assert!(why.starts_with("not_additive:"), "host {host}: {why}");
    }
}

#[test]
fn an_unknown_host_schema_does_not_let_a_migration_through() {
    let (m, _) = migrator(5, Box::new(|| Err("the database does not answer".into())));
    let why = refused(m.migrate_forward(&c(B)));
    assert!(why.starts_with("not_additive:"), "{why}");
    assert!(why.contains("does not answer"), "{why}");
}
