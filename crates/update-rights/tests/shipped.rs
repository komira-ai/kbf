//! The declarations the repository carries: each must load, with every key in the table the
//! loader reads it from.

use std::fs;
use std::path::{Path, PathBuf};

use update_rights::Declaration;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Every `*.toml` file that sits in a directory named `updater.d`, anywhere in the repository's
/// source directories.
fn shipped() -> Vec<PathBuf> {
    fn walk(dir: &Path, found: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            if path.is_dir() {
                if name != "target" && name != "node_modules" {
                    walk(&path, found);
                }
            } else if path.extension().is_some_and(|e| e == "toml")
                && path
                    .parent()
                    .and_then(Path::file_name)
                    .is_some_and(|d| d == "updater.d")
            {
                found.push(path);
            }
        }
    }
    let mut found = Vec::new();
    for top in [
        "crates",
        "packaging",
        "release",
        "docs",
        "infra",
        "examples",
        "tools",
    ] {
        walk(&root().join(top), &mut found);
    }
    found.sort();
    found
}

/// Catches a declaration shipped in the repository that its own loader would refuse: an
/// unknown key, a key in the wrong table, a path outside the grammar. The two fixtures of this
/// crate are shipped in this sense, so the scan cannot pass for want of a file.
#[test]
fn every_shipped_declaration_parses() {
    let files = shipped();
    let names: Vec<String> = files
        .iter()
        .map(|f| f.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert!(names.contains(&"tierstore.toml".to_owned()), "{names:?}");
    assert!(names.contains(&"js-agent.toml".to_owned()), "{names:?}");
    assert!(names.contains(&"kbf.toml".to_owned()), "{names:?}");
    for file in files {
        let text = fs::read_to_string(&file).unwrap();
        if let Err(refusal) = Declaration::parse(&text) {
            panic!("{}: {refusal}", file.display());
        }
    }
}

/// Catches the key-placement scan being unable to fail: the same fixtures with a key moved to
/// the wrong table are refused.
#[test]
fn a_shipped_declaration_with_a_key_in_the_wrong_table_is_refused() {
    let text = fs::read_to_string(
        root().join("crates/update-rights/tests/fixtures/updater.d/tierstore.toml"),
    )
    .unwrap();
    assert!(Declaration::parse(&text).is_ok());
    // A top-level key moved under [hook], a [hook] key moved to the top.
    let moved_down = text
        .replace("\n[hook]\n", "\n[hook]\nverify_deadline_secs = 1600\n")
        .replace("verify_deadline_secs = 1600\npromotion", "promotion");
    assert!(Declaration::parse(&moved_down).is_err());
    let moved_up = text.replace(
        "registration_ttl_secs = 300\n",
        "registration_ttl_secs = 300\nunits = []\n",
    );
    assert!(Declaration::parse(&moved_up).is_err());
}

/// Catches the design's own example drifting away from what the loader reads: the TOML block of
/// section 2.4, with its two placeholders filled in, is a declaration.
#[test]
fn the_example_in_the_design_is_a_declaration() {
    let doc = fs::read_to_string(root().join("docs/design/host-updates.md")).unwrap();
    let from = doc.find("### 2.4 The declaration").expect("section 2.4");
    let block = &doc[from..];
    let start = block.find("```toml\n").expect("the example") + "```toml\n".len();
    let end = start
        + block[start..]
            .find("\n```")
            .expect("the end of the example");
    let example = block[start..end].replace("\"...\"", "\"0.1.0\"");
    let declaration = Declaration::parse(&example).unwrap_or_else(|e| panic!("{e}\n{example}"));
    assert_eq!(declaration.name, "tierstore");
    assert_eq!(declaration.hook.units.len(), 2);
}

/// Catches a shipped declaration of a hook that runs as root naming a write path the root rule
/// would refuse when it is loaded: each write path is at or below a directory named for the
/// service.
#[test]
fn the_write_paths_of_shipped_root_hooks_are_named_for_their_service() {
    let mut checked = 0;
    for file in shipped() {
        let declaration = Declaration::parse(&fs::read_to_string(&file).unwrap()).unwrap();
        if declaration.hook.user != "root" {
            continue;
        }
        for path in &declaration.hook.write_paths {
            let verdict = update_rights::reserved::check_service_write(path, &declaration.name);
            assert_eq!(verdict, Ok(()), "{}: {path}", file.display());
            checked += 1;
        }
    }
    assert!(
        checked >= 2,
        "the js-agent fixture writes in two places: {checked}"
    );
}

/// The keys a declaration has at its top level, from the type the loader reads: every key of a
/// declaration that sets them all, but the `[hook]` table.
fn top_level_keys() -> Vec<String> {
    let text = fs::read_to_string(
        root().join("crates/update-rights/tests/fixtures/updater.d/tierstore.toml"),
    )
    .unwrap();
    let declaration = Declaration::parse(&text).unwrap();
    assert!(declaration.boot.is_some() && declaration.min_version.is_some());
    let table = toml::Table::try_from(&declaration).unwrap();
    let keys: Vec<String> = table.keys().filter(|k| *k != "hook").cloned().collect();
    assert!(
        keys.contains(&"verify_deadline_secs".to_owned()),
        "{keys:?}"
    );
    keys
}

/// The lines of `text` that set a top-level key after a table header, where TOML reads them as
/// keys of that table.
fn misplaced(text: &str, top: &[String]) -> Vec<String> {
    let mut in_table = false;
    let mut found = Vec::new();
    for line in text.lines().map(str::trim) {
        in_table |= line.starts_with('[');
        let key = line.split('=').next().unwrap_or_default().trim();
        if in_table & top.iter().any(|t| t == key) {
            found.push(line.to_owned());
        }
    }
    found
}

/// Catches a top-level key written after a table header, which TOML reads as a key of that
/// table: under `[hook]` the loader refuses it, but under the opaque `[hook.params]` it would load
/// as a parameter and the declaration would silently lose the key. Every shipped declaration,
/// the kbf one among them, has its top-level keys before its first table header.
#[test]
fn every_shipped_declaration_has_its_top_level_keys_before_any_table() {
    let top = top_level_keys();
    let files = shipped();
    assert!(
        files
            .iter()
            .any(|f| f.ends_with("host-agent/linux/updater.d/kbf.toml"))
    );
    for file in files {
        let text = fs::read_to_string(&file).unwrap();
        let found = misplaced(&text, &top);
        assert!(found.is_empty(), "{}: {found:?}", file.display());
    }
}

/// Catches the placement scan being unable to fail: the tierstore fixture with its boot entry
/// moved under `[hook.params]` still loads (the table is opaque), and the scan finds the line.
#[test]
fn a_top_level_key_under_hook_params_loads_and_is_found_by_the_scan() {
    let text = fs::read_to_string(
        root().join("crates/update-rights/tests/fixtures/updater.d/tierstore.toml"),
    )
    .unwrap();
    let moved = text.replacen("boot = \"restore\"\n", "", 1).replacen(
        "[hook.params]\n",
        "[hook.params]\nboot = \"restore\"\n",
        1,
    );
    assert_ne!(moved, text);
    let loaded = Declaration::parse(&moved).expect("the opaque table takes it");
    assert_eq!(loaded.boot, None, "the boot entry is lost");
    assert_eq!(misplaced(&moved, &top_level_keys()), ["boot = \"restore\""]);
}
