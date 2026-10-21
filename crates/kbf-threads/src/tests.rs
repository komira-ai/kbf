use super::*;

#[test]
fn a_cpu_list_counts_its_cpus() {
    for (text, want) in [
        ("0,44\n", Some(2)),
        ("0-1", Some(2)),
        ("0-3,8\n", Some(5)),
        ("7", Some(1)),
        ("", None),
        ("a,b", None),
        ("3-1", None),
        ("1-", None),
    ] {
        assert_eq!(siblings_in_list(text), want, "{text:?}");
    }
}
