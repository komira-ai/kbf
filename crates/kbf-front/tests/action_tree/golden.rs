//! The manifests, byte for byte, and as tierstore's reader reads them.

use tierstore::EntryKind;

use super::*;

/// The standard input root: a file and an executable whose bytes are spans of a pack, an empty
/// file, a large object held whole in `cas/l/`, a symlink and a nested directory, listed out
/// of order.
struct Standard {
    root: Digest,
    a: Digest,
    run: Digest,
    big: Digest,
    x: Digest,
}

impl Standard {
    async fn store(rig: &Rig) -> Self {
        let farm = &rig.farm;
        let a = farm.packed("int main;", "cas/f/w/p0", 17).await;
        let run = farm.packed("echo ran\n", "cas/f/w/p0", 40).await;
        let big = farm.whole("a large object").await;
        let x = farm.packed("#define X", "cas/f/w/p1", 0).await;
        let empty = kbf_segments::sha256(&[]);
        let sub = rig
            .store(&directory(vec![file("x.h", &x, false)], vec![], vec![]))
            .await;
        let root = rig
            .store(&directory(
                vec![
                    file("run.sh", &run, true),
                    file("a.c", &a, false),
                    file("empty", &empty, false),
                    file("big.bin", &big, false),
                ],
                vec![subdir("sub", &sub)],
                vec![link("link", "a.c")],
            ))
            .await;
        Self {
            root,
            a,
            run,
            big,
            x,
        }
    }
}

/// Catches: a manifest that is not byte for byte the documented form (member order, `exec`
/// always written, `source` only for a span, none for a whole object or an empty file, a
/// directory named by its manifest's digest and length, children sorted), a `.kbf` that does
/// not name the Action and the Command, and a root that does not hold the base's children as
/// they are beside `work` and `.kbf`.
#[tokio::test]
async fn manifests_are_written_byte_for_byte() {
    let rig = Rig::new().await;
    let tree = Standard::store(&rig).await;
    let written = rig.write(&tree.root).await.expect("a tree");
    let sub = format!(
        r#"{{"v":1,"children":[{{"name":"x.h","kind":"file","sha256":"{}","size":9,"exec":false,"source":{{"key":"cas/f/w/p1","offset":0}}}}]}}"#,
        tree.x.hash_hex()
    );
    let sub_hex = TreeDigest::of(sub.as_bytes()).to_string();
    assert_eq!(rig.farm.manifest(&sub_hex), sub);
    let work = format!(
        concat!(
            r#"{{"v":1,"children":["#,
            r#"{{"name":"a.c","kind":"file","sha256":"{a}","size":9,"exec":false,"source":{{"key":"cas/f/w/p0","offset":17}}}},"#,
            r#"{{"name":"big.bin","kind":"file","sha256":"{big}","size":14,"exec":false}},"#,
            r#"{{"name":"empty","kind":"file","sha256":"e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855","size":0,"exec":false}},"#,
            r#"{{"name":"link","kind":"symlink","target":"a.c"}},"#,
            r#"{{"name":"run.sh","kind":"file","sha256":"{run}","size":9,"exec":true,"source":{{"key":"cas/f/w/p0","offset":40}}}},"#,
            r#"{{"name":"sub","kind":"dir","sha256":"{sub}","size":{sub_len}}}"#,
            r#"]}}"#
        ),
        a = tree.a.hash_hex(),
        big = tree.big.hash_hex(),
        run = tree.run.hash_hex(),
        sub = sub_hex,
        sub_len = sub.len(),
    );
    let work_hex = TreeDigest::of(work.as_bytes()).to_string();
    assert_eq!(rig.farm.manifest(&work_hex), work);
    let action = rig.action(&tree.root).await;
    let kbf = format!(
        concat!(
            r#"{{"v":1,"children":["#,
            r#"{{"name":"action","kind":"file","sha256":"{action}","size":{action_len},"exec":false,"source":{{"key":"cas/f/w/p9","offset":100}}}},"#,
            r#"{{"name":"command","kind":"file","sha256":"{command}","size":{command_len},"exec":false,"source":{{"key":"cas/f/w/p9","offset":0}}}}"#,
            r#"]}}"#
        ),
        action = action.hash_hex(),
        action_len = action.size_bytes,
        command = rig.command.hash_hex(),
        command_len = rig.command.size_bytes,
    );
    let kbf_hex = TreeDigest::of(kbf.as_bytes()).to_string();
    assert_eq!(rig.farm.manifest(&kbf_hex), kbf);
    let root = format!(
        concat!(
            r#"{{"v":1,"children":["#,
            r#"{{"name":".kbf","kind":"dir","sha256":"{kbf}","size":{kbf_len}}},"#,
            r#"{{"name":".kbf-tools","kind":"dir","sha256":"{tools}","size":{tools_len}}},"#,
            r#"{{"name":"README","kind":"file","sha256":"{file}","size":5,"exec":false,"source":{{"key":"cas/f/base/p0","offset":3}}}},"#,
            r#"{{"name":"bin","kind":"symlink","target":"usr/bin"}},"#,
            r#"{{"name":"usr","kind":"dir","sha256":"{usr}","size":{usr_len}}},"#,
            r#"{{"name":"work","kind":"dir","sha256":"{work}","size":{work_len}}}"#,
            r#"]}}"#
        ),
        kbf = kbf_hex,
        kbf_len = kbf.len(),
        tools = rig.base.tools,
        tools_len = rig.base.tools_len,
        file = BASE_FILE,
        usr = rig.base.usr,
        usr_len = rig.base.usr_len,
        work = work_hex,
        work_len = work.len(),
    );
    assert_eq!(
        written.digest().to_string(),
        TreeDigest::of(root.as_bytes()).to_string()
    );
    assert_eq!(rig.farm.manifest(&written.digest().to_string()), root);
    // The same inputs give the same tree, to the byte.
    assert_eq!(
        written.digest().to_string(),
        "8d421df90814641d465b20afaa087e4cba46274aa5f44948abc4170daf0b2a0f"
    );
}

/// What one entry of a flattened tree is, for comparing shapes.
fn shape(entries: &[TreeEntry]) -> Vec<(Option<u32>, String, String)> {
    entries
        .iter()
        .map(|entry| {
            let kind = match &entry.kind {
                EntryKind::Dir { .. } => "dir".to_owned(),
                EntryKind::File { exec, source, .. } => format!(
                    "file exec={exec} source={}",
                    source
                        .as_ref()
                        .map_or("-".to_owned(), |s| format!("{}@{}", s.key, s.offset))
                ),
                EntryKind::Symlink { target } => format!("symlink {target}"),
            };
            (entry.parent, entry.name.to_string(), kind)
        })
        .collect()
}

/// Catches: a manifest the pinned reader refuses or reads otherwise than written (a child
/// lost or out of order, a span, an executable bit or a link target wrong), and an entry
/// count the writer's limits are counted with that is not the reader's.
#[tokio::test]
async fn every_manifest_is_read_by_tierstores_reader() {
    let rig = Rig::new().await;
    let tree = Standard::store(&rig).await;
    let written = rig.write(&tree.root).await.expect("a tree");
    let entries = rig
        .farm
        .flatten(&written)
        .expect("the reader takes the tree");
    let expected: Vec<(Option<u32>, &str, &str)> = vec![
        (None, "", "dir"),
        (Some(0), ".kbf", "dir"),
        (Some(1), "action", "file exec=false source=cas/f/w/p9@100"),
        (Some(1), "command", "file exec=false source=cas/f/w/p9@0"),
        (Some(0), ".kbf-tools", "dir"),
        (Some(4), "kbf-task", "file exec=true source=-"),
        (Some(0), "README", "file exec=false source=cas/f/base/p0@3"),
        (Some(0), "bin", "symlink usr/bin"),
        (Some(0), "usr", "dir"),
        (Some(8), "cc", "file exec=true source=-"),
        (Some(0), "work", "dir"),
        (Some(10), "a.c", "file exec=false source=cas/f/w/p0@17"),
        (Some(10), "big.bin", "file exec=false source=-"),
        (Some(10), "empty", "file exec=false source=-"),
        (Some(10), "link", "symlink a.c"),
        (Some(10), "run.sh", "file exec=true source=cas/f/w/p0@40"),
        (Some(10), "sub", "dir"),
        (Some(16), "x.h", "file exec=false source=cas/f/w/p1@0"),
    ];
    let expected: Vec<(Option<u32>, String, String)> = expected
        .into_iter()
        .map(|(parent, name, kind)| (parent, name.to_owned(), kind.to_owned()))
        .collect();
    assert_eq!(shape(&entries), expected);
    // The base's six, the input root's seven, and the four the action tree adds.
    assert_eq!(entries.len() as u64 - 1, Base::FACTS.entries + 7 + 4);
}

/// Catches: a root that rewrites, drops or reads the base's children (a directory, a
/// top-level symlink, a top-level file with a span), or writes any manifest of the base: the
/// root holds them as the base's root manifest has them, and nothing of the base is looked up,
/// read or written.
#[tokio::test]
async fn the_action_tree_root_names_the_base_children_by_digest() {
    let rig = Rig::new().await;
    let tree = Standard::store(&rig).await;
    let written = rig.write(&tree.root).await.expect("a tree");
    let root = rig.farm.manifest(&written.digest().to_string());
    let base = String::from_utf8(rig.base.root.clone()).expect("text");
    let base_children = base
        .strip_prefix(r#"{"v":1,"children":["#)
        .and_then(|rest| rest.strip_suffix("]}"))
        .expect("a manifest");
    assert!(
        root.contains(base_children),
        "the base's children, as they are, in a row: {root}"
    );
    let base_root = TreeDigest::of(&rig.base.root).to_string();
    let base_keys: Vec<String> = [&rig.base.usr, &rig.base.tools, &base_root]
        .iter()
        .map(|hex| whole_key(hex))
        .collect();
    let puts = rig.farm.puts();
    assert!(
        puts.iter().all(|put| !base_keys.contains(put)),
        "no manifest of the base is written: {puts:?}"
    );
    let state = rig.farm.state();
    let base_named = |d: &Digest| d.hash_hex() == BASE_FILE;
    assert!(!state.asks.iter().flatten().any(base_named));
    assert!(!state.heads.iter().any(base_named));
    assert!(state.reads.is_empty(), "every blob was located: none read");
}

/// Catches: children sorted within each of REAPI's three lists but not across them, or by
/// something other than their bytes.
#[tokio::test]
async fn children_are_sorted_bytewise_across_the_three_node_lists() {
    let rig = Rig::new().await;
    let blob = rig.farm.packed("same", "cas/f/w/p0", 0).await;
    let leaf = rig.store(&directory(vec![], vec![], vec![])).await;
    let root = rig
        .store(&directory(
            vec![file("b", &blob, false), file("_", &blob, false)],
            vec![subdir("Z", &leaf), subdir("a.d", &leaf)],
            vec![link("B", "b"), link("a", "b")],
        ))
        .await;
    let written = rig.write(&root).await.expect("a tree");
    let entries = rig.farm.flatten(&written).expect("a tree");
    let work = entries
        .iter()
        .find(|entry| entry.parent == Some(0) && &*entry.name == "work")
        .expect("work");
    let names: Vec<&str> = entries
        .iter()
        .filter(|entry| entry.parent == Some(work.index))
        .map(|entry| &*entry.name)
        .collect();
    assert_eq!(names, ["B", "Z", "_", "a", "a.d", "b"]);
}

/// Catches: a name or a target written without JSON escaping, or escaped so the reader reads
/// back other bytes: a quote, a backslash, a control character and a non-ASCII letter.
#[tokio::test]
async fn names_and_targets_are_read_back_as_given() {
    let rig = Rig::new().await;
    let blob = rig.farm.packed("same", "cas/f/w/p0", 0).await;
    let odd = "q\"uote\\back\tslash é";
    let root = rig
        .store(&directory(
            vec![file(odd, &blob, false)],
            vec![],
            vec![link("l", odd)],
        ))
        .await;
    let written = rig.write(&root).await.expect("a tree");
    let entries = rig.farm.flatten(&written).expect("a tree");
    assert!(entries.iter().any(|entry| &*entry.name == odd));
    assert!(entries.iter().any(|entry| matches!(
        &entry.kind,
        EntryKind::Symlink { target } if &**target == odd
    )));
}

/// The boundary of an empty input root: `work` is an empty directory, and the tree is read.
#[tokio::test]
async fn an_empty_input_root_is_an_empty_work() {
    let rig = Rig::new().await;
    let root = rig.store(&Directory::default()).await;
    let written = rig.write(&root).await.expect("a tree");
    let empty = r#"{"v":1,"children":[]}"#;
    assert_eq!(
        rig.farm
            .manifest(&TreeDigest::of(empty.as_bytes()).to_string()),
        empty
    );
    let entries = rig.farm.flatten(&written).expect("a tree");
    assert_eq!(entries.len() as u64 - 1, Base::FACTS.entries + 4);
}
