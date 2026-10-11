use super::*;

/// `sfltool dumpbtm` as macOS prints it (trimmed): two records, an entry with an
/// embedded item, fields the item leaves out.
const DUMP: &str = "\
========================
 Records for UID 0 : FFFFEEEE-DDDD-CCCC-BBBB-AAAA00000000
========================

 ServiceManagement migrated: true
 SharedFileList migrated: true

 Items:

 #1:
                 UUID: 7c2a33d1-0b47-4b6e-a1c9-2b0f1d3e4a51
                 Name: \u{ab}Unknown\u{bb}
       Developer Name: \u{ab}Unknown\u{bb}
                 Type: legacy daemon (0x10010)
                Flags: [ legacy ] (0x1)
          Disposition: [enabled, allowed, visible, notified] (0xb)
           Identifier: 16.com.example.helper
                  URL: file:///Library/LaunchDaemons/com.example.helper.plist
      Executable Path: /Library/PrivilegedHelperTools/com.example.helper
           Generation: 1
    Parent Identifier: 2.com.example.app
   Embedded Item Identifiers:
     #1: 16.com.example.embedded

========================
 Records for UID 501 : 11111111-2222-3333-4444-555555555555
========================

 Items:

 #1:
                 UUID: 0d6f
                 Type: app (0x2)
           Identifier: 2.com.example.app
           Generation: 3
";

/// Catches: an embedded item's `#1: <id>` line taken for a new entry, an entry put
/// under the wrong record's uid, the UUID or generation (which change on their own)
/// kept in the item, or a field of one entry carried into the next.
#[test]
fn dumpbtm_entries_become_one_item_each() {
    assert_eq!(
        parse_dumpbtm(DUMP).unwrap(),
        [
            "background-tasks uid=0 id=\"16.com.example.helper\" type=\"legacy daemon (0x10010)\" \
             url=\"file:///Library/LaunchDaemons/com.example.helper.plist\" \
             exec=\"/Library/PrivilegedHelperTools/com.example.helper\" \
             disposition=\"[enabled, allowed, visible, notified] (0xb)\"",
            "background-tasks uid=501 id=\"2.com.example.app\" type=\"app (0x2)\" url=\"\" \
             exec=\"\" disposition=\"\"",
        ]
    );
}

/// Catches: output `sfltool` did not print in its known form taken as "no entries"
/// (a changed format would hide every background task), while empty output, a Mac
/// with no records, is no entries.
#[test]
fn dumpbtm_in_another_form_is_an_error() {
    assert_eq!(parse_dumpbtm(""), Ok(Vec::new()));
    assert_eq!(parse_dumpbtm(" \n"), Ok(Vec::new()));
    let error = parse_dumpbtm("Items:\n #1:\n Identifier: x\n").unwrap_err();
    assert!(error.contains("before any record header"), "{error}");
    let error = parse_dumpbtm("something else\n").unwrap_err();
    assert!(error.contains("no \"Records for UID\" header"), "{error}");
    // A record with no entries is fine.
    assert_eq!(parse_dumpbtm(" Records for UID 0 : X\n"), Ok(Vec::new()));
    for line in ["#1:", "#12:"] {
        assert!(is_entry_header(line), "{line}");
    }
    for line in ["#:", "#1: x", "#a:", "1:", "#1"] {
        assert!(!is_entry_header(line), "{line}");
    }
}

/// Catches: a diff that misses an item on one side.
#[test]
fn the_diff_has_both_sides() {
    let set = |items: &[&str]| items.iter().map(|&i| i.to_owned()).collect::<BTreeSet<_>>();
    assert_eq!(
        diff(&set(&["a", "b"]), &set(&["b", "c"])),
        Diff {
            added: vec!["c".to_owned()],
            removed: vec!["a".to_owned()],
        }
    );
}
