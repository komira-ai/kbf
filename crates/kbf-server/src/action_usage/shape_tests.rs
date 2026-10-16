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

/// BXL outputs, at the paths buck2 writes for komira's `test_weld.bxl` `check`
/// (configuration and content form). Catches: the `art-bxl` or `gen-bxl` root
/// unknown (the path kept whole, hashes and all, so the key splits every build), the
/// execution-platform hash kept or not taken as `cfg`, the arguments hash after the
/// function's `__<name>__` kept, or an unpadded (shorter) hash in either place kept.
#[test]
fn bxl_paths_drop_their_hashes() {
    let want = "komira/tools/build/lint/test_weld.bxl/__check__/test_weld/validation.json";
    assert_eq!(
        pat(
            "buck-out/v2/art-bxl/komira/453d11473805230f/tools/build/lint/test_weld.bxl/__check__/ecd72b18569a9587/test_weld/validation.json"
        ),
        some(want, Some("453d11473805230f"))
    );
    assert_eq!(
        pat(
            "buck-out/v2/art-bxl/komira/tools/build/lint/test_weld.bxl/__check__/083ffece723d3c8b/test_weld/validation.json"
        ),
        some(want, None)
    );
    // Unpadded: 13 and 15 digits.
    assert_eq!(
        pat(
            "buck-out/v2/gen-bxl/komira/53d11473805230f/tools/build/lint/test_weld.bxl/__check__/d72b18569a958/test_weld/validation.json"
        ),
        some(want, Some("53d11473805230f"))
    );
    // A short hex directory after the cell is a directory (fewer than 13 digits).
    assert_eq!(
        pat("buck-out/v2/art-bxl/komira/cafe/x.bxl/__f__/083ffece723d3c8b/o"),
        some("komira/cafe/x.bxl/__f__/o", None)
    );
}

/// The root-package anonymous target buck2's prelude makes for the C++ toolchain, in
/// content form, where buck2 glues the content hash to the cell; and a package
/// directory named by hex digits, which is not the hash (the hash always sits
/// between it and the name). Catches: the glued hash kept (the key moves with every
/// content change), the cell cut with it, or a hex package directory dropped.
#[test]
fn an_anonymous_root_package_target_drops_its_glued_hash() {
    assert_eq!(
        pat(
            "buck-out/v2/art-anon/anon990e66921b1edf1c/___compiler_type_flags_anon_rule__/compiler_type_args"
        ),
        some(
            "anon/___compiler_type_flags_anon_rule__/compiler_type_args",
            None
        )
    );
    assert_eq!(
        pat(&format!("buck-out/v2/art-anon/komira/{CFG}3fa9c1/__x__/o")),
        some("komira/__x__/o", Some(CFG))
    );
    assert_eq!(
        pat(&format!(
            "buck-out/v2/art-anon/komira/{CFG}src/beef/3fa9c1/__x__/o"
        )),
        some("komira/src/beef/__x__/o", Some(CFG))
    );
    // A cell glued to a hash too short to be a content hash stays as it is.
    assert_eq!(
        pat(&format!("buck-out/v2/art-anon/{}/__x__/o", &CONTENT[..15])),
        some(&format!("{}/__x__/o", &CONTENT[..15]), None)
    );
}

/// Catches: a hash or `output_artifacts` dropped anywhere but in the one segment
/// right after a `__<name>__` (an action's own hex-named directory, or a directory
/// named `output_artifacts`, would vanish from its pattern and two outputs merge),
/// or BXL's unpadded hash rule applied to a target's outputs.
#[test]
fn only_the_segment_after_the_name_is_a_hash() {
    assert_eq!(
        pat(&format!(
            "buck-out/v2/art/cell/{CFG}/pkg/__t__/d/{CONTENT2}/output_artifacts/a.o"
        )),
        some(
            &format!("cell/pkg/__t__/d/{CONTENT2}/output_artifacts/a.o"),
            Some(CFG)
        )
    );
    assert_eq!(
        pat(&format!(
            "buck-out/v2/art/cell/pkg/__t__/{CONTENT}/{CONTENT2}/a.o"
        )),
        some(&format!("cell/pkg/__t__/{CONTENT2}/a.o"), None)
    );
    assert_eq!(
        pat(&format!(
            "buck-out/v2/art/cell/{CFG}/pkg/{CONTENT2}/__t__/a.o"
        )),
        some(&format!("cell/pkg/{CONTENT2}/__t__/a.o"), Some(CFG))
    );
    // Only BXL writes an unpadded hash after the name: a target's 13-digit directory
    // stays.
    assert_eq!(
        pat("buck-out/v2/art/cell/pkg/__t__/0123456789abc/a.o"),
        some("cell/pkg/__t__/0123456789abc/a.o", None)
    );
}

/// Catches: `cfg` taken from another output than the first in pattern order (or in
/// the order the client sent them), so one action's `cfg` changes with the order of
/// its outputs or names the execution platform's output rather than the target's.
#[test]
fn cfg_is_the_first_patterns() {
    let outputs = [
        format!("buck-out/v2/art/cell/{CFG}/pkg/__t__/b.o"),
        format!("buck-out/v2/art/cell/{CFG2}/pkg/__t__/a.o"),
        "c.o".to_owned(),
    ];
    let argv = strings(&["cc"]);
    let sent = Shape::of(&argv, &outputs);
    let reversed: Vec<String> = outputs.iter().rev().cloned().collect();
    assert_eq!(sent.cfg.as_deref(), Some(CFG2));
    assert_eq!(Shape::of(&argv, &reversed).cfg.as_deref(), Some(CFG2));
}

/// Catches: an inline script that enters the key when its flag is a cluster
/// (`-euc`, `-Bc`), so the script body (and any configuration hash in it) is in the
/// key; or a flag that only contains a `c` (`-C`, `--c`, `-ce` before a script file)
/// taken as one.
#[test]
fn a_flag_cluster_ending_in_c_is_inline() {
    assert_eq!(
        program(&strings(&["runner", "-euc", "echo a > x"])),
        ["runner", "-euc"]
    );
    assert_eq!(
        program(&strings(&["/usr/bin/env", "interp", "-Bc", "print(1)"])),
        ["env", "interp", "-Bc"]
    );
    assert_eq!(
        program(&strings(&["runner", "-C", "dir", "x"])),
        ["runner", "dir", "x"]
    );
    assert_eq!(
        program(&strings(&["runner", "--c", "dir"])),
        ["runner", "dir"]
    );
    assert_eq!(
        program(&strings(&["runner", "-ce", "script"])),
        ["runner", "script"]
    );
    assert_eq!(program(&strings(&["runner", "-"])), ["runner"]);
}

/// Catches: an argument file's path kept as sent (its configuration hash splits
/// the key per configuration, a content hash every build), or the `@` dropped.
#[test]
fn an_argument_file_is_normalized_after_its_at() {
    let argsfile =
        |cfg: &str| format!("@buck-out/v2/art/komira/{cfg}/src/foo/__foo__/foo.argsfile");
    let a = program(&strings(&["clang++", &argsfile(CFG)]));
    assert_eq!(a, ["clang++", "@komira/src/foo/__foo__/foo.argsfile"]);
    assert_eq!(program(&strings(&["clang++", &argsfile(CFG2)])), a);
    assert_eq!(
        program(&strings(&[
            "clang++",
            "@buck-out/v2/art/komira/src/foo/__foo__/output_artifacts/foo.argsfile"
        ])),
        a
    );
    assert_eq!(
        program(&strings(&[
            "clang++",
            &format!("@buck-out/v2/tmp/k/{CONTENT}/a")
        ])),
        ["clang++", "@buck-out/tmp"]
    );
    assert_eq!(
        program(&strings(&["clang++", "@args.txt"])),
        ["clang++", "@args.txt",]
    );
}
