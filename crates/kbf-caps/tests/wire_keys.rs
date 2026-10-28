//! The property names kbf clients and daemons exchange, pinned where kbf can see them.
//!
//! The behaviour behind them is tested in `js-caps`; these are the names themselves, so a
//! revision of that crate that renames one fails here and not on a farm.

use kbf_caps::{DAEMON_OSES, REAPI_KEYS, REPORT_ONLY_KEYS, RESERVED_KEYS, property_name};

#[test]
fn the_platform_property_names_are_the_ones_clients_send() {
    assert_eq!(REAPI_KEYS, ["OSFamily", "ISA", "Arch"]);
    assert_eq!(DAEMON_OSES, ["linux", "macos"]);
    assert_eq!(
        RESERVED_KEYS,
        [
            "kbf-lease",
            "kbf-cpu",
            "kbf-mac-admin",
            "kbf-book-cpus",
            "kbf-book-mem-gib"
        ]
    );
    assert_eq!(
        REPORT_ONLY_KEYS,
        ["vm.slots", "vm.max_cpus", "vm.max_mem_gib"]
    );
}

#[test]
fn property_names_are_read_in_any_case() {
    assert_eq!(property_name("osfamily").as_deref(), Some("OSFamily"));
    assert_eq!(property_name("KBF-LEASE").as_deref(), Some("kbf-lease"));
}
