//! `Tree::with_input` and the defaults of `Job`.
//!
//! Catches: an input that is not at `in/<name>` (every black-box action reads it there),
//! and a `Job` that is cacheable or has a timeout without being asked.

use kbf_testkit::tree::{Job, Tree};

#[test]
fn an_input_is_one_file_under_in() {
    let tree = Tree::with_input("input.bin", b"abc");
    assert!(tree.files.is_empty() && tree.links.is_empty());
    let [(name, inner)] = &tree.dirs[..] else {
        panic!("one directory: {tree:?}");
    };
    assert_eq!(name, "in");
    assert_eq!(inner.files, vec![("input.bin".to_owned(), b"abc".to_vec())]);
    assert!(inner.dirs.is_empty() && inner.links.is_empty());
}

#[test]
fn a_job_is_uncached_and_untimed_unless_asked() {
    let job = Job::default();
    assert!(!job.cacheable);
    assert_eq!(job.timeout, None);
    assert!(job.outputs.is_empty());
}
