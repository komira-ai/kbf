//! The host agent in the deb: its files come from the host agent's directory, the configuration
//! gives nfpm its links as links and keeps all of it to the deb, the links are no sources of the
//! build, and a host agent directory that lacks a file is refused naming it.

use super::tests::{Tree, refused};
use super::*;

/// Asserts that the host agent's files in the deb are the bytes of the host agent's directory:
/// each unit and the account file is taken from there, not from the units directory.
#[test]
fn the_host_agent_files_are_the_host_agent_directorys() {
    let t = Tree::new("agent-bytes");
    let plan = kbf_plan(&t.inputs()).expect("plan");
    for name in AGENT_UNITS.into_iter().chain([AGENT_SYSUSERS]) {
        let src = t.dir.join("host-agent").join(name);
        let e = plan
            .iter()
            .find(|e| e.src == src)
            .unwrap_or_else(|| panic!("{name} is packaged"));
        assert_eq!(e.sha256, Some(Sha256::of(name.as_bytes())), "{name}");
    }
}

/// Asserts that the configuration gives nfpm each link as a link (its target as `src`), and marks
/// every entry of the host agent, and no other, as the deb's alone.
#[test]
fn the_config_links_the_agent_units_and_keeps_them_to_the_deb() {
    let t = Tree::new("config-agent");
    let entries = kbf_plan(&t.inputs()).expect("plan");
    let text = config(&entries, Platform::LinuxX86_64, "1.2.3+ci");
    for unit in AGENT_UNITS {
        let want = format!(
            "  - dst: \"{WANTS_DIR}/{unit}\"\n    src: \"../{unit}\"\n    type: \"symlink\"\n    \
             packager: \"deb\"\n    file_info: {{mode: 0777, owner: root, group: root}}\n"
        );
        assert!(text.contains(&want), "{want} in {text}");
        let unit_entry = format!("  - dst: \"{UNIT_DIR}/{unit}\"\n    src: ");
        let at = text.find(&unit_entry).expect("the unit is an entry");
        let rest = &text[at + unit_entry.len()..];
        let next = rest.find("  - dst:").unwrap_or(rest.len());
        assert!(rest[..next].contains("    packager: \"deb\"\n"), "{text}");
    }
    assert_eq!(text.matches("packager: \"deb\"").count(), 5, "{text}");
    assert_eq!(text.matches("type: \"symlink\"").count(), 2, "{text}");
}

/// Asserts that the sources whose times `pkg-build` changes are the files the package copies, and
/// not the targets of its links, which are paths inside the package.
#[test]
fn the_sources_of_a_config_are_its_files_and_not_its_links() {
    let t = Tree::new("sources-links");
    let entries = kbf_plan(&t.inputs()).expect("plan");
    let got = sources(&config(&entries, Platform::LinuxX86_64, "1.0.0")).expect("sources");
    let want: Vec<PathBuf> = entries
        .iter()
        .filter(|e| e.sha256.is_some())
        .map(|e| e.src.clone())
        .collect();
    assert_eq!(got, want);
    assert!(!got.iter().any(|p| p.starts_with("..")), "{got:?}");
}

/// Asserts that the plan refuses a host agent directory without the helper's unit, without the
/// agent's, without the agent's account file, or that is not there, each with its reason.
#[test]
fn the_plan_refuses_a_host_agent_directory_that_lacks_a_file() {
    type Change = fn(&Tree) -> Inputs<'_>;
    let cases: [(&str, Change, &str); 4] = [
        (
            "a missing helper unit",
            |t| {
                std::fs::remove_file(t.dir.join("host-agent/kbf-host-local.service")).expect("rm");
                t.inputs()
            },
            "host agent file kbf-host-local.service is missing",
        ),
        (
            "a missing agent unit",
            |t| {
                std::fs::remove_file(t.dir.join("host-agent/kbf-updater-agent.service"))
                    .expect("rm");
                t.inputs()
            },
            "host agent file kbf-updater-agent.service is missing",
        ),
        (
            "a missing agent account",
            |t| {
                std::fs::remove_file(t.dir.join("host-agent/kbf-updater.conf")).expect("rm");
                t.inputs()
            },
            "host agent file kbf-updater.conf is missing",
        ),
        (
            "a host agent directory that is not there",
            |t| {
                let mut i = t.inputs();
                i.host_agent = Path::new("/nonexistent/host-agent");
                i
            },
            "/nonexistent/host-agent cannot be read as a directory",
        ),
    ];
    for (n, (what, change, says)) in cases.iter().enumerate() {
        let t = Tree::new(&format!("refuse-agent-{n}"));
        let text = refused(kbf_plan(&change(&t)));
        assert!(text.contains(says), "{what}: {text}");
    }
}
