//! `restore --database NAME`: the scratch database is derived from `installer.toml`.

use super::*;

/// Catches a restore test that connects as another role or on another socket than the
/// configuration's (an assumed `kbf`): a bare `--database` name is a database on the server
/// and socket `installer.toml` names, for the role it names; a bad name is refused.
///
/// Mutants seen red: the role dropped from the derived URL; a bare name taken as a URL.
#[test]
fn a_bare_database_name_is_on_the_configured_server_for_the_configured_role() {
    let url = "postgres:///kbf?host=/run/postgresql&user=kbf-server";
    let root = root_with_config("app-restore-bare", &format!("database = \"{url}\"\n"));
    let r = root.to_str().expect("text");
    fs::create_dir_all(root.join("b")).expect("b");
    fs::write(root.join("b/kbf-x.dump"), "PGDMP").expect("dump");
    let args = |name: &str| {
        restore_args(&[
            "--root",
            r,
            "restore",
            "--file",
            "/b/kbf-x.dump",
            "--database",
            name,
        ])
    };
    let tool = Fakes(Default::default());
    run_restore(
        &tool,
        root.clone(),
        true,
        &args("kbf_restore_test"),
        &mut Vec::new(),
    )
    .expect("restores");
    let want = "--dbname=postgres:///kbf_restore_test?host=/run/postgresql&user=kbf-server ";
    assert!(tool.0.borrow()[0].contains(want), "{:?}", tool.0);
    let tool = Fakes(Default::default());
    let refused = run_restore(
        &tool,
        root.clone(),
        true,
        &args("bad-name"),
        &mut Vec::new(),
    );
    assert!(refused.is_err() && tool.0.borrow().is_empty());
}
