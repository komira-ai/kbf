//! The shape normalizer on buck2-shaped paths and argv (`buck-out/v2/art/...` as
//! buck2 lays them out today, `gen` as it did), and the key's stability.

use super::*;

const CFG: &str = "7a3c1e9f0b2d4c68";
const CFG2: &str = "0f1e2d3c4b5a6978";
const EXEC: &str = "1122334455667788";
const CONTENT: &str = "0123456789abcdef";
const CONTENT2: &str = "fedcba9876543210";

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

fn pat(path: &str) -> Option<(String, Option<String>)> {
    pattern(path)
}

fn some(p: &str, cfg: Option<&str>) -> Option<(String, Option<String>)> {
    Some((p.to_owned(), cfg.map(str::to_owned)))
}

/// Catches: the configuration hash kept (one target's key split per configuration),
/// the `output_artifacts` placeholder or a content hash kept (a content-hash path keys
/// apart from the configuration-hash path of the same output), or the configuration
/// hash dropped from `cfg` (telemetry could not split configurations).
#[test]
fn configuration_and_content_hash_forms_give_one_pattern() {
    let want = "komira/src/komira_encoding/__komira_encoding__/out/libfoo.a";
    assert_eq!(
        pat(&format!(
            "buck-out/v2/art/komira/{CFG}/src/komira_encoding/__komira_encoding__/out/libfoo.a"
        )),
        some(want, Some(CFG))
    );
    assert_eq!(
        pat(
            "buck-out/v2/art/komira/src/komira_encoding/__komira_encoding__/output_artifacts/out/libfoo.a"
        ),
        some(want, None)
    );
    assert_eq!(
        pat(&format!(
            "buck-out/v2/art/komira/src/komira_encoding/__komira_encoding__/{CONTENT}/out/libfoo.a"
        )),
        some(want, None)
    );
}

/// Catches: an execution-platform pair (`<cfg>-<exec>`) left in the pattern, or cut
/// to its first half in `cfg`.
#[test]
fn an_exec_configuration_pair_is_stripped_whole() {
    assert_eq!(
        pat(&format!(
            "buck-out/v2/art/prelude/{CFG}-{EXEC}/rust/tools/__rustc_action__/rustc_action.py"
        )),
        some(
            "prelude/rust/tools/__rustc_action__/rustc_action.py",
            Some(&format!("{CFG}-{EXEC}"))
        )
    );
}

/// Catches: the `gen` root (older buck2, and its unhashed symlinks) keyed apart from
/// `art`, or a path's prefix before `buck-out` (an absolute project root) kept.
#[test]
fn gen_and_art_and_an_absolute_prefix_give_one_pattern() {
    let art = pat(&format!("buck-out/v2/art/cell/{CFG}/pkg/__t__/t.o"));
    let gen_ = pat(&format!("buck-out/v2/gen/cell/{CFG}/pkg/__t__/t.o"));
    let abs = pat(&format!(
        "/srv/repo/buck-out/v2/gen/cell/{CFG}/pkg/__t__/t.o"
    ));
    assert_eq!(art, some("cell/pkg/__t__/t.o", Some(CFG)));
    assert_eq!(gen_, art);
    assert_eq!(abs, art);
}

/// Catches: an anonymous target's configuration hash (glued to its package) or its
/// attributes hash kept, so every attribute change splits the key.
#[test]
fn an_anonymous_target_drops_both_hashes() {
    assert_eq!(
        pat(&format!(
            "buck-out/v2/art-anon/komira/{CFG}src/x/3fa9c1/__x__0__/o"
        )),
        some("komira/src/x/__x__0__/o", Some(CFG))
    );
    assert_eq!(
        pat("buck-out/v2/gen-anon/komira/src/x/deadbeef/__x__0__/o"),
        some("komira/src/x/__x__0__/o", None)
    );
    // Outside an anonymous root a hex package segment is a package.
    assert_eq!(
        pat("buck-out/v2/art/komira/src/cafe/__x__/o"),
        some("komira/src/cafe/__x__/o", None)
    );
}

/// Catches: a dynamic action's `__action__<k>__` collapsed (its actions would share a
/// key), or its content hash kept.
#[test]
fn a_dynamic_action_keeps_its_index() {
    assert_eq!(
        pat(&format!(
            "buck-out/v2/art/cell/pkg/__t__/__action__3__/{CONTENT}/link.o"
        )),
        some("cell/pkg/__t__/__action__3__/link.o", None)
    );
    assert_ne!(
        pat("buck-out/v2/art/cell/pkg/__t__/__action__3__/output_artifacts/link.o"),
        pat("buck-out/v2/art/cell/pkg/__t__/__action__4__/output_artifacts/link.o"),
    );
}

/// Catches: a scratch path kept (its whole-identifier hash differs every build, so
/// the key would too).
#[test]
fn scratch_paths_are_left_out() {
    let scratch = format!("buck-out/v2/tmp/komira/{CONTENT}/scratch");
    assert_eq!(pat(&scratch), None);
    let with = Shape::of(&strings(&["cc"]), &strings(&["a.o", &scratch]));
    let without = Shape::of(&strings(&["cc"]), &strings(&["a.o"]));
    assert_eq!(with.key, without.key);
    assert_eq!(with.outputs, ["a.o"]);
}

/// Catches: a path outside buck-out rewritten (Bazel's, or a plain source path), or a
/// hex segment dropped from it.
#[test]
fn other_paths_are_kept_as_sent() {
    for path in [
        "bazel-out/k8-opt/bin/a.o",
        "out/0123456789abcdef/a.o",
        "buck-out/v2/log/x",
    ] {
        assert_eq!(pat(path), some(path, None), "{path}");
    }
}

/// The program names here are stand-ins (`runner` for the shell a genrule runs,
/// `interp` for an interpreter): the rule names no program, so what it does with one
/// name it does with any.
///
/// Catches: a bare program not unwrapped (every genrule, Python tool or inline
/// script would share its runner's key), a path not ending the program (a tool's
/// arguments, which may vary, would enter the key), an inline script that enters the
/// key, a flag or `NAME=value` taken for a program, an absolute program path kept
/// whole (keys split by where a worker installs it), a buck-out program not
/// normalized, or more than three words.
#[test]
fn the_program_unwraps_bare_names_until_a_path() {
    let genrule = format!("buck-out/v2/art/prelude/{CFG}/genrule/__genrule__/sh/genrule.sh");
    assert_eq!(
        program(&strings(&["/usr/bin/env", "runner", "-e", &genrule])),
        ["env", "runner", "prelude/genrule/__genrule__/sh/genrule.sh"]
    );
    assert_eq!(
        program(&strings(&[
            "/usr/bin/env",
            "LANG=C",
            "-i",
            "interp3.12",
            "tool.py"
        ])),
        ["env", "interp3.12", "tool.py"]
    );
    let rustc = format!(
        "buck-out/v2/art/prelude/{CFG}-{EXEC}/rust/tools/__rustc_action__/{CONTENT}/rustc_action.py"
    );
    assert_eq!(
        program(&strings(&["interp", &rustc, "--", "rustc"])),
        [
            "interp",
            "prelude/rust/tools/__rustc_action__/rustc_action.py"
        ]
    );
    let busybox = format!("buck-out/v2/art/komira/tools/busybox/__busybox__/{CONTENT}/busybox");
    assert_eq!(
        program(&strings(&[&busybox, "runner", "-c", "echo one"])),
        ["komira/tools/busybox/__busybox__/busybox"]
    );
    assert_eq!(
        program(&strings(&["runner", "-e", "-c", "echo one"])),
        ["runner", "-c"]
    );
    assert_eq!(
        program(&strings(&["runner", "-e", "-c", "echo one"])),
        program(&strings(&["runner", "-c", "rm -f two"])),
    );
    assert_eq!(
        program(&strings(&["/usr/bin/clang", "-c", "a.c"])),
        ["clang", "-c"]
    );
    assert_eq!(
        program(&strings(&["cc", "-o", "a.o", "a.c"])),
        ["cc", "a.o", "a.c"]
    );
    let zig = format!(
        "buck-out/v2/art/komira/{CFG2}/tools/build/mojo/__zig_cc_launcher__/zig_cc_launcher"
    );
    assert_eq!(
        program(&strings(&[&zig, "zig", "cc"])),
        ["komira/tools/build/mojo/__zig_cc_launcher__/zig_cc_launcher"]
    );
    assert_eq!(
        program(&strings(&["tools/kbf-m2-act", "x"])),
        ["tools/kbf-m2-act"]
    );
    assert_eq!(program(&[]), ["-"]);
    assert_eq!(
        program(&strings(&["env", "env", "env", "env", "x"])),
        ["env", "env", "env"]
    );
    assert_eq!(program(&strings(&["runner", "-e"])), ["runner"]);
}

/// Catches: outputs not sorted or not deduplicated (buck2's compatibility mode sends
/// one path in both `output_files` and `output_directories`; the order is the
/// client's), so one action gets two keys.
#[test]
fn outputs_are_a_set() {
    let argv = strings(&["cc"]);
    let a = Shape::of(&argv, &strings(&["b.o", "a.o", "a.o"]));
    let b = Shape::of(&argv, &strings(&["a.o", "b.o"]));
    assert_eq!(a.key, b.key);
    assert_eq!(a.outputs, ["a.o", "b.o"]);
}

/// The key is pinned: a change of hash, of tag, or of framing is a new key for every
/// action ever logged, so it must come with a new version tag.
///
/// Catches: `DefaultHasher` or another hash swapped in, the version tag dropped or
/// changed without this test, a shorter or longer key.
#[test]
fn the_key_is_pinned() {
    let shape = Shape::of(
        &strings(&["/usr/bin/env", "runner", "-e", "genrule.sh"]),
        &strings(&[&format!(
            "buck-out/v2/art/komira/{CFG}/src/foo/__foo__/out/libfoo.a"
        )]),
    );
    assert_eq!(shape.program, ["env", "runner", "genrule.sh"]);
    assert_eq!(shape.outputs, ["komira/src/foo/__foo__/out/libfoo.a"]);
    // Recomputed outside Rust: SHA-256 over u64-LE-length-prefixed parts, the tag
    // first, then each list as its count followed by its items.
    assert_eq!(shape.key.to_string(), "d3a4df9da7659720");
}

/// Catches: parts joined without framing, so `a/b` + `c` and `a` + `b/c` (or a
/// program word and an output) hash alike.
#[test]
fn the_parts_are_framed() {
    let argv = strings(&["cc"]);
    assert_ne!(
        Shape::of(&argv, &strings(&["a/b", "c"])).key,
        Shape::of(&argv, &strings(&["a", "b/c"])).key,
    );
    assert_ne!(
        Shape::of(&strings(&["env", "x"]), &[]).key,
        Shape::of(&strings(&["env"]), &strings(&["x"])).key,
    );
}

/// Catches: a key that moves with the configuration or the content hashes (the whole
/// point: one target's action keeps its key across builds), and two of one target's
/// actions (two compiles) or two targets sharing a key.
#[test]
fn one_action_keeps_its_key_and_others_differ() {
    let argv = |cfg: &str| {
        strings(&[
            &format!(
                "buck-out/v2/art/komira/{cfg}/tools/build/mojo/__zig_cc_launcher__/zig_cc_launcher"
            ),
            "zig",
            "cc",
            "-c",
        ])
    };
    let compile = |cfg: &str, file: &str| {
        Shape::of(
            &argv(cfg),
            &strings(&[&format!(
                "buck-out/v2/art/komira/{cfg}/src/foo/__foo__/objects/{file}.o"
            )]),
        )
    };
    let opt = compile(CFG, "a");
    let dbg = compile(CFG2, "a");
    assert_eq!(opt.key, dbg.key);
    assert_eq!(
        (opt.cfg.as_deref(), dbg.cfg.as_deref()),
        (Some(CFG), Some(CFG2))
    );
    let content = Shape::of(
        &strings(&[
            &format!(
                "buck-out/v2/art/komira/tools/build/mojo/__zig_cc_launcher__/{CONTENT2}/zig_cc_launcher"
            ),
            "zig",
        ]),
        &strings(&["buck-out/v2/art/komira/src/foo/__foo__/output_artifacts/objects/a.o"]),
    );
    assert_eq!(content.key, opt.key);
    assert_eq!(content.cfg, None);
    assert_ne!(compile(CFG, "b").key, opt.key);
    let other_target = Shape::of(
        &argv(CFG),
        &strings(&[&format!(
            "buck-out/v2/art/komira/{CFG}/src/bar/__bar__/objects/a.o"
        )]),
    );
    assert_ne!(other_target.key, opt.key);
}

/// The edges of the path reader. Catches: a scratch program not named as one (its
/// hashed path would split the key), a `buck-out` path that ends at the cell
/// refused or mangled, a glued anonymous configuration hash taken where the package
/// does not start with 16 hex digits (or is cut inside a character), the cell itself
/// taken for an attributes hash, or an execution pair accepted with a malformed half.
#[test]
fn edge_paths() {
    let scratch = format!("buck-out/v2/tmp/komira/{CONTENT}/run.sh");
    assert_eq!(program(&strings(&[&scratch, "x"])), ["buck-out/tmp"]);
    assert_eq!(pat("buck-out/v2/art/komira"), some("komira", None));
    let glued = format!("buck-out/v2/art-anon/komira/{}é/__x__/o", &CONTENT[..15]);
    assert_eq!(
        pat(&glued),
        some(&format!("komira/{}é/__x__/o", &CONTENT[..15]), None)
    );
    assert_eq!(
        pat("buck-out/v2/art-anon/komira/srcsrcsrcsrcsrcsrc/__x__/o"),
        some("komira/srcsrcsrcsrcsrcsrc/__x__/o", None)
    );
    assert_eq!(
        pat("buck-out/v2/art-anon/abc/__x__/o"),
        some("abc/__x__/o", None)
    );
    assert_eq!(
        pat(&format!("buck-out/v2/art/cell/{CFG}-zz/pkg/__t__/o")),
        some(&format!("cell/{CFG}-zz/pkg/__t__/o"), None)
    );
    assert_eq!(
        pat("buck-out/v2/art/cell/my-pkg/__t__/o"),
        some("cell/my-pkg/__t__/o", None)
    );
}
