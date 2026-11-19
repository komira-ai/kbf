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

/// Asserts that one kind of work is one kind across targets and builds: `zig cc` writing two
/// targets' objects, in two configurations, is `zig.cc.build.kind` for both, a label value,
/// while each target keeps its own class; a program given by its absolute path is the kind of
/// its file name. Catches a kind that holds the target or the configuration (it would learn no
/// faster than the target), and a job sent no kind at all.
#[test]
fn a_kind_is_what_runs_whatever_the_target() {
    let zig = |program: &str, cfg: &str, target: &str| {
        let out = format!("buck-out/v2/art/root/{cfg}/pkg/__{target}__/{target}.o");
        SizeClass::of_action(&shape(&[program, "cc", "-c", "src.c", "-o", &out], &[&out]))
    };
    let one = zig("zig", CFG, "one");
    let two = zig("zig", "0123456789abcdef", "two");
    assert_eq!(one.kind(), Some("zig.cc.build.kind"));
    assert_eq!(two.kind(), one.kind());
    assert_ne!(one.as_str(), two.as_str());
    assert_eq!(zig("/opt/zig/0.15/zig", CFG, "one").kind(), one.kind());
    assert!(is_label_value(one.kind().expect("a kind")));
}

/// Asserts which second word of the program a kind keeps: a subcommand or a flag of letters,
/// digits, `-` and `_` (`env make`, `clang -c`), never one with a `/`, a `.` or an `@`
/// (`cc a.o a.c` is `cc`, `mojo <script>` is `mojo`, `env make <genrule.mk>` is `env make`),
/// while a bare file name is kept (`cc -o out a.c` is `cc out`); and that a tool
/// built under `buck-out` is its own kind, every subcommand of it in one. Catches a kind that
/// keeps a file or script path (one target's, so every target its own kind), and one that
/// drops the subcommand, so `zig cc` and `zig ar` would meet only through the cold size.
#[test]
fn a_kind_keeps_a_subcommand_and_never_a_path() {
    let kind = |argv: &[&str]| {
        SizeClass::of_action(&shape(argv, &["out/o"]))
            .kind()
            .map(str::to_owned)
    };
    let genrule = format!("buck-out/v2/art/root/{CFG}/pkg/__g__/genrule.mk");
    assert_eq!(
        kind(&["env", "make", "-f", &genrule]).as_deref(),
        Some("env.make.build.kind")
    );
    assert_eq!(
        kind(&["clang", "-c", "a.c"]).as_deref(),
        Some("clang.-c.build.kind")
    );
    assert_eq!(
        kind(&["cc", "-o", "a.o", "a.c"]).as_deref(),
        Some("cc.build.kind")
    );
    assert_eq!(
        kind(&["cc", "-o", "out", "a.c"]).as_deref(),
        Some("cc.out.build.kind")
    );
    assert_eq!(
        kind(&["mojo", "prelude/tools/make_comp_db.mojo", "gen"]).as_deref(),
        Some("mojo.build.kind")
    );
    let tool = format!("buck-out/v2/art/prelude/{CFG}/toolchains/__zig_wrapper__/zig");
    assert_eq!(
        kind(&[&tool, "cc", "a.c"]).as_deref(),
        Some("prelude.toolchains.__zig_wrapper__.zig.build.kind")
    );
    assert_eq!(kind(&[&tool, "ar", "a.o"]), kind(&[&tool, "cc", "a.c"]));
}

/// Asserts that a test run and a build of one program are two kinds, as a target's compile and
/// its test are two classes. Catches a test's memory booked for every compile of its kind.
#[test]
fn a_kinds_build_and_its_test_are_two_kinds() {
    let test_out = "buck-out/v2/test/execution/root/0123456789abcdef/fedcba9876543210/default/o";
    let run = SizeClass::of_action(&shape(&["cargo", "test", "--release"], &[test_out]));
    let built = SizeClass::of_action(&shape(&["cargo", "test", "--release"], &["out/o"]));
    assert_eq!(run.kind(), Some("cargo.test.test.kind"));
    assert_eq!(built.kind(), Some("cargo.test.build.kind"));
}

/// Asserts that an action with no arguments, or whose program holds no ASCII letter or digit,
/// has no kind, so its job names no fallback, while it keeps its class. Catches every such
/// action pooled into one kind `build.kind` (the largest of them would size them all).
#[test]
fn an_action_with_no_program_has_no_kind() {
    let none = SizeClass::of_action(&shape(&[], &["out/o"]));
    assert_eq!(none.kind(), None);
    assert!(none.as_str().starts_with("shape."), "{none:?}");
    assert_eq!(
        SizeClass::of_action(&shape(&["日本"], &["out/o"])).kind(),
        None
    );
}

/// Asserts that a kind's class never meets a target's or a shape's: the target `zig//:cc`
/// built is `zig.cc.build` where `zig cc` is `zig.cc.build.kind`, a program named `shape` is
/// not a shape's class, and a kind long enough to be cut is a label value whose kept part ends
/// in `.kind`, then a hash. Catches a kind whose history merges with a target's or a
/// shape's, which would size the one from the other's runs.
#[test]
fn a_kind_never_meets_a_targets_or_a_shapes_class() {
    let out = format!("buck-out/v2/art/zig/{CFG}/__cc__/o");
    let target = SizeClass::of_action(&shape(&["zig", "cc"], &[&out]));
    assert_eq!(target.as_str(), "zig.cc.build");
    assert_eq!(target.kind(), Some("zig.cc.build.kind"));
    let named = SizeClass::of_action(&shape(&["shape"], &["out/o"]));
    assert_eq!(named.kind(), Some("shape.build.kind"));
    assert_ne!(named.kind(), Some(named.as_str()));
    let long = format!(
        "buck-out/v2/art/root/{CFG}/{}__t__/tool",
        "deep_tool_dir/".repeat(6)
    );
    let cut = SizeClass::of_action(&shape(&[&long], &["out/o"]));
    let kind = cut.kind().expect("a kind");
    assert!(is_label_value(kind), "{kind:?}");
    let (head, hash) = kind.rsplit_once('-').expect("a hash");
    assert!(head.ends_with("tool.build.kind"), "{kind:?}");
    assert_eq!(hash.len(), 16, "{kind:?}");
    assert!(hash.bytes().all(|b| b.is_ascii_hexdigit()), "{kind:?}");
}
