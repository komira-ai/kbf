use super::{SizeClass, fnv1a, written};
use crate::action_usage::Shape;

/// A configuration hash buck2 puts after the cell.
const CFG: &str = "7a3c1e9f0b2d4c68";

fn shape(argv: &[&str], outputs: &[&str]) -> Shape {
    let owned = |words: &[&str]| words.iter().map(|w| (*w).to_owned()).collect::<Vec<_>>();
    Shape::of(&owned(argv), &owned(outputs))
}

/// Whether `class` is a value jobscheduler's `size-class` label takes, as `js_types::Labels`
/// checks one (1 to 63 bytes of `[a-z0-9._-]`), starting and ending with a letter or a digit.
fn is_label_value(class: &str) -> bool {
    let edge = |c: Option<char>| c.is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    (1..=63).contains(&class.len())
        && class
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
        && edge(class.chars().next())
        && edge(class.chars().last())
}

/// Asserts that a name is written exactly as `js_types::SizeClass::of_target` (jobscheduler
/// 1cb35cd9) writes it, for its own examples and for long names, cut and hashed; the expected
/// values are that function's output. Catches a copy that drifts from jobscheduler's rule: a
/// separator kept, doubled or dropped, a letter left upper, a cut at another byte or inside a
/// word, another hash, or a hash of the kept part only. Any of these would name another class
/// than the one jobscheduler documents for the target, and split a target's history.
#[test]
fn a_name_is_written_as_jobscheduler_writes_a_target() {
    let deep = format!("root//{}:integration_test", "deep/".repeat(20));
    let cases = [
        (
            "root//crates/js-sched:js-sched".to_owned(),
            "root.crates.js-sched.js-sched",
        ),
        (
            "root//crates/kbf-server:kbf-server build".to_owned(),
            "root.crates.kbf-server.kbf-server.build",
        ),
        ("cell//:name build".to_owned(), "cell.name.build"),
        (
            "root//Foo/Bar:My_Test test".to_owned(),
            "root.foo.bar.my_test.test",
        ),
        (
            "root//a:b (prelude//platforms:default#0123)".to_owned(),
            "root.a.b.prelude.platforms.default.0123",
        ),
        ("//pkg:été".to_owned(), "pkg.t"),
        ("-_//:a_-".to_owned(), "a"),
        (
            format!("a{}", "b".repeat(63)),
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-16a1d065369639a6",
        ),
        (
            deep.clone(),
            "deep.deep.deep.deep.deep.deep.integration_test-04ad921dee837d4a",
        ),
        (
            format!("{deep} build"),
            "deep.deep.deep.deep.integration_test.build-5114dc965f8e796c",
        ),
        (
            format!(
                "root//komira/crates/{}:a_test_target_with_a_long_name build",
                "very_long_package_name/".repeat(3)
            ),
            "a_test_target_with_a_long_name.build-c2c6065f601fc264",
        ),
        (
            format!("{}:{}", "a".repeat(30), "b".repeat(45)),
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-3aa06ef8153bf7ef",
        ),
        (
            format!("{}:{}", "a".repeat(29), "b".repeat(46)),
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-7cf8eb79e63c1476",
        ),
        (
            format!("{}:{}:{}", "a".repeat(29), "b".repeat(20), "c".repeat(25)),
            "bbbbbbbbbbbbbbbbbbbb.ccccccccccccccccccccccccc-74f3889cefbb57dd",
        ),
        (
            format!("{}-{}", "a".repeat(40), "b".repeat(45)),
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-e4aaa2549354cafe",
        ),
    ];
    for (name, class) in cases {
        assert_eq!(written(&name), class, "{name:?}");
        assert!(is_label_value(class), "{class:?}");
    }
}

/// Asserts that a name of 63 bytes is kept whole and one of 64 is cut to exactly 63, and that a
/// name with no ASCII letter or digit is written empty (jobscheduler's `None`). Catches an
/// off-by-one at the label's length limit, and a separator-only name written as a class.
#[test]
fn the_label_length_is_the_limit_and_a_bare_name_is_empty() {
    let fits = format!("a{}", "b".repeat(62));
    assert_eq!(written(&fits), fits);
    assert_eq!(written(&format!("a{}", "b".repeat(63))).len(), 63);
    for bare in ["", "//:", "-_-", "...", "日本"] {
        assert_eq!(written(bare), "", "{bare:?}");
    }
}

/// Asserts the hash's value for known input, as jobscheduler's tests pin it. Catches another
/// hash, offset or prime, which would rename every long class and orphan its history.
#[test]
fn the_hash_is_fnv1a_64() {
    assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
    assert_eq!(fnv1a(b"a"), 0xaf63_dc4c_8601_ec8c);
    assert_eq!(fnv1a(b"foobar"), 0x8594_4171_f739_67e8);
}

/// Asserts that a compile of a target is the class `<target> build` and a test run of the same
/// target is `<target> test`, whatever configuration or content hash its paths carry, and that
/// two builds of the target (other inputs, other configuration) are one class. Catches a test
/// sharing its compile's history, or a class that changes from build to build (and so never
/// learns), such as one that holds the configuration hash.
#[test]
fn a_targets_compile_and_its_test_are_two_classes_stable_across_builds() {
    let compile = |cfg: &str, input: &str| {
        let out = format!("buck-out/v2/art/root/{cfg}/crates/kbf-server/__kbf-server__/lib.rlib");
        SizeClass::of_action(&shape(&["rustc", input, "-o", &out], &[&out]))
    };
    let built = compile(CFG, "src/lib.rs");
    assert_eq!(built.as_str(), "root.crates.kbf-server.kbf-server.build");
    assert_eq!(compile("0123456789abcdef", "src/other.rs"), built);
    let binary = format!("buck-out/v2/art/root/{CFG}/crates/kbf-server/__kbf-server__/test");
    let run = SizeClass::of_action(&shape(
        &[&binary, "--test-threads=1"],
        &["buck-out/v2/test/execution/root/0123456789abcdef/fedcba9876543210/default/o"],
    ));
    assert_eq!(run.as_str(), "root.crates.kbf-server.kbf-server.test");
    assert_ne!(run, built);
}

/// Asserts that an action whose paths name no target is the class of its shape's key, the same
/// for every run of that shape and apart from another shape's, and that such a class meets no
/// target's. Catches every target-less action pooled into one class (the largest of them would
/// size them all), or a class that is not a label value.
#[test]
fn an_action_with_no_target_is_the_class_of_its_shape() {
    let plain = shape(&["cc", "-c", "a.c"], &["out/a.o"]);
    assert_eq!(plain.target, None);
    let class = SizeClass::of_action(&plain);
    assert_eq!(class.as_str(), format!("shape.{}", plain.key));
    assert!(is_label_value(class.as_str()), "{class:?}");
    assert_eq!(
        SizeClass::of_action(&shape(&["cc", "-c", "a.c"], &["out/a.o"])),
        class
    );
    let other = SizeClass::of_action(&shape(&["ld", "a.o"], &["out/a"]));
    assert_ne!(other, class);
    let out = format!("buck-out/v2/art/shape/{CFG}/pkg/__t__/o");
    let named = SizeClass::of_action(&shape(&["cc"], &[&out]));
    assert_eq!(named.as_str(), "shape.pkg.t.build");
}

/// Asserts that a target whose class is long enough to be cut is still a label value, keeps the
/// end of the target's package, its own name and its kind, and differs between its compile and its test. Catches a long
/// class the job service refuses, or a cut that drops the kind so that the two kinds meet.
#[test]
fn a_long_targets_classes_are_label_values_and_stay_apart() {
    let pkg = "very_long_package_name/".repeat(3);
    let out = format!("buck-out/v2/art/root/{CFG}/komira/crates/{pkg}__integration__/o");
    let built = SizeClass::of_action(&shape(&["cc"], &[&out]));
    let binary = format!("buck-out/v2/art/root/{CFG}/komira/crates/{pkg}__integration__/t");
    let run = SizeClass::of_action(&shape(
        &[&binary],
        &["buck-out/v2/test/execution/root/0123456789abcdef/fedcba9876543210/default/o"],
    ));
    for class in [&built, &run] {
        assert!(is_label_value(class.as_str()), "{class:?}");
    }
    let hashed = |class: &SizeClass, kept: &str| {
        let (head, hash) = class.as_str().rsplit_once('-').expect("a hash");
        assert_eq!(head, kept, "{class:?}");
        assert_eq!(hash.len(), 16, "{class:?}");
    };
    hashed(&built, "very_long_package_name.integration.build");
    hashed(&run, "very_long_package_name.integration.test");
}
