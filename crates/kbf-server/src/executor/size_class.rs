//! The class the job service sizes an action's job by, the value of the job's `size-class`
//! label, its history key (below), and the class of its kind, the value of its `size-fallback`
//! label (below). A job that leaves its CPU and memory at zero is booked what the history of its class
//! says (jobscheduler's `docs/design/sizing.md`, section 7.7), so every run of one piece of work
//! must name one class, and no other work may share it.
//!
//! The work is the build target the action belongs to, read from the action's own `buck-out`
//! paths ([`Shape::target`]: buck2 sends no target with the call), what runs on it (the words
//! of its kind, below) and what the action does to it: a test run ([`Shape::test`]) is one
//! mode and anything else, a compile or a link, the other. The class is
//! `<target> <kind words> build` or `<target> <kind words> test`, so one target's actions of
//! different kinds are different classes: a gate or a coverage run that needs a few hundred
//! MiB does not book the memory of its target's largest compile, a compile does not book its
//! test's, and a test not its compile's. An action whose program gives no kind words is
//! `<target> build` or `<target> test`. The words are written as a label value as jobscheduler
//! writes a target's name: `rustc` building `root//crates/kbf-server:kbf-server` is
//! `root.crates.kbf-server.kbf-server.rustc.build`. Every word of the class is free of
//! configuration and content hashes, so one action of one target names one class in every
//! build. A class longer than 63 bytes is cut (see `written`): it keeps its last words, the kind
//! words and the mode, and the hash of the whole name stands for the rest. A test run whose
//! program is the test binary its target built has that binary's `buck-out` pattern as its
//! kind words, which repeat the target, so its class is nearly always cut
//! (`crates.kbf-server.__kbf-server__.test.test-<16 hex>`).
//!
//! The words are joined as written, so a target whose name ends in what reads as a kind word
//! (`root//a:b.cc` built by a program with no kind words) writes the class of another target's
//! kind (`root//a:b` built by `cc`); the same holds of target names alone (`cell//a:b` and
//! `cell//:a.b`) under jobscheduler's rule. Such targets share a history.
//!
//! An action whose paths name no target has the class of its shape's key
//! ([`crate::action_usage::ShapeKey`], `shape.<16 hex digits>`): what stays the same across builds of one action, so its runs
//! still share a history, and a class of its own, apart from every other such action.
//!
//! No two sorts of class meet: a target's ends in `.build` or `.test`, or, cut because it is
//! long, in `-` and 16 hex digits; a shape's is `shape.` and 16 hex digits, and is never cut.
//!
//! **The kind** ([`SizeClass::kind`]), sent as the job's `size-fallback` label (jobscheduler's
//! `docs/design/sizing.md`, section 7.8): the class whose history sizes the job while its own
//! class has too few runs. A build farm's cache answers every action whose inputs did not
//! change, so a target gathers runs slowly and, alone, is booked the cold size for its first
//! five; the work of one kind is alike across targets and runs many times a minute, so a kind
//! has an estimate after two of its runs, on any target. The kind is what runs, read from the
//! shape's program words ([`Shape::program`], already free of configuration and content
//! hashes): the first word (the program: a bare name, an absolute path's file name, or the
//! pattern of a tool built under `buck-out`), and the second when it is a word of letters,
//! digits, `-` and `_` only (a subcommand or a flag such as `cc` of `zig cc`, `make` of
//! `env make`, `-c` of `clang -c`; never a word holding a `/`, a `.` or an `@`, a path or a file
//! name with an extension, which would name one target's files. A bare file name such as the
//! `out` of `cc -o out a.c` is kept, and makes a kind of its own), then the test-or-build word of the class: `zig cc build kind` is `zig.cc.build.kind`.
//! A shape's program holds a second word only after a bare-name first one, so a tool built
//! under `buck-out` is its own kind, with every subcommand of it. An action with no program
//! (no arguments) has no kind, and its job names no fallback. The same words, without
//! `kind`, are the kind words of the target's class above. A kind's class ends in `.kind`
//! (or, cut, in `-` and 16 hex digits of a name that ends in `.kind`), so it never meets a
//! target's or a shape's class.
//!
//! **The history key** ([`SizeClass::history_key`]), sent as the job's own field
//! `JobSpec.history_key` (jobscheduler's `docs/design/sizing.md`, section 7.7): the key the job
//! service keeps the action's usage history under, which it takes ahead of the `size-class`
//! label. It is `<target> <action kind>` as they are, never lowered, cut or hashed, so a booking
//! names the target it belongs to (`komira//src/json:json mojo_compile`). The target and the kind
//! are the client's when its Execute's `RequestMetadata` names both (`target_id`,
//! `action_mnemonic`); Buck2 names neither (it sends its build id and its own name and version
//! alone), so for Buck2 they are always the action's own paths': the target's label as
//! [`Shape::target`] reads it, the kind's words above, and `test` after them for a test run, so
//! a target's test run is a key of its own. A key must be 1 to 512 bytes of printable ASCII
//! (`js_types::HistoryKey`): a client's pair that is not one leaves the paths' key, and an action
//! with no target, no program, or a target that is not printable ASCII has no key of its own (the
//! job is sent none, and its `size-class` label sizes it). An operation taken up from the state
//! store has no Execute, so its key is always the paths'.
//!
//! The words are written as `js_types::SizeClass::of_target` writes them, byte for byte, by a
//! copy of its rule here (`written`), which the tests hold to jobscheduler's own examples and
//! hashes.

use js_types::HistoryKey;
use kbf_front::ActionLabel;

use crate::action_usage::Shape;

/// The label whose value names a job's class: jobscheduler's `js_types::SIZE_CLASS_LABEL`.
pub(crate) const SIZE_CLASS_LABEL: &str = "size-class";

/// The label whose value names a job's fallback class, its kind: jobscheduler's
/// `js_types::SIZE_FALLBACK_LABEL`.
pub(crate) const SIZE_FALLBACK_LABEL: &str = "size-fallback";

/// Longest label value, in bytes.
const MAX_VALUE_LEN: usize = 63;

/// The bytes a long name keeps: a label value's 63, less `-` and 16 hex digits.
const KEPT_LEN: usize = MAX_VALUE_LEN - 17;

/// The 64-bit FNV-1a offset basis.
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;

/// The 64-bit FNV-1a prime.
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// A job's size class, the class of its kind, and the history key its paths give
/// ([`SizeClass::history_key`]); the class and the kind each a valid value of the labels
/// [`SIZE_CLASS_LABEL`] and [`SIZE_FALLBACK_LABEL`] (1 to 63 bytes of `[a-z0-9._-]`) that starts
/// and ends with a letter or a digit.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SizeClass {
    /// The class: the target, the kind's words and whether it is a test, or the shape's key.
    class: String,
    /// The kind: what runs, and whether it is a test; `None` for an action with no program.
    kind: Option<String>,
    /// The history key the action's own paths give ([`SizeClass::history_key`]): its target's
    /// label and the kind's words, as they are; `None` for an action with no target or no
    /// program, or whose key is no [`HistoryKey`].
    history_key: Option<String>,
}

impl SizeClass {
    /// The class of the action whose shape is `shape`, and its kind (see the module docs).
    #[must_use]
    pub fn of_action(shape: &Shape) -> Self {
        let test = if shape.test { "test" } else { "build" };
        let words = kind_words(&shape.program);
        let class = match (&shape.target, &words) {
            // Never empty: the test-or-build word is a word.
            (Some(target), Some(words)) => written(&format!("{} {words} {test}", target.label)),
            (Some(target), None) => written(&format!("{} {test}", target.label)),
            (None, _) => format!("shape.{}", shape.key),
        };
        let history_key = match (&shape.target, &words) {
            (Some(target), Some(words)) if shape.test => {
                history_key(&format!("{} {words} test", target.label))
            }
            (Some(target), Some(words)) => history_key(&format!("{} {words}", target.label)),
            _ => None,
        };
        Self {
            class,
            kind: words.map(|words| written(&format!("{words} {test} kind"))),
            history_key,
        }
    }

    /// The key the job service keeps the action's usage history under (its `JobSpec.history_key`,
    /// jobscheduler's `docs/design/sizing.md`, section 7.7): `<target> <action kind>`, as they
    /// are, never lowered, cut or hashed. The target and the kind are the ones the client `sent`
    /// in the Execute's `RequestMetadata` (`target_id`, `action_mnemonic`) when it sent both and
    /// they make a [`HistoryKey`], else the ones the action's own paths give (its target's label,
    /// the kind's words, and `test` for a test run); `None` when neither makes one (see the
    /// module docs).
    pub(crate) fn history_key(&self, sent: &ActionLabel) -> Option<String> {
        let from_client = match (&sent.target, &sent.mnemonic) {
            (Some(target), Some(mnemonic)) => history_key(&format!("{target} {mnemonic}")),
            _ => None,
        };
        from_client.or_else(|| self.history_key.clone())
    }

    /// The class as the label's value.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.class
    }

    /// The kind as the value of the label [`SIZE_FALLBACK_LABEL`]; `None` for an action with no
    /// program.
    pub(crate) fn kind(&self) -> Option<&str> {
        self.kind.as_deref()
    }
}

/// `text` when it is a [`HistoryKey`] (1 to 512 bytes of printable ASCII); `None` otherwise, for
/// the key is sent as it is or not at all.
fn history_key(text: &str) -> Option<String> {
    HistoryKey::parse(text)
        .ok()
        .map(|key| key.as_str().to_owned())
}

/// The words that name the kind of an action whose shape's program words are `program` (see
/// the module docs), before the test-or-build word: `None` when the program is no word with a
/// letter or a digit (no arguments, whose program is `-`).
fn kind_words(program: &[String]) -> Option<String> {
    let first = program.first().filter(|first| !written(first).is_empty())?;
    let second = program.get(1).filter(|word| {
        word.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    });
    Some(second.map_or_else(|| first.clone(), |second| format!("{first} {second}")))
}

/// `name` written as a class, as `js_types::SizeClass::of_target` writes a build target's
/// name; empty when the name holds no ASCII letter or digit. Letters are lowered and every run
/// of bytes outside `[a-z0-9_-]` is one `.`, and the result starts and ends with a letter or a
/// digit (`root//crates/js-sched:js-sched` is `root.crates.js-sched.js-sched`). A result
/// longer than 63 bytes keeps its last 46 bytes from the first whole word in them (a word cut
/// in two is dropped, unless the 46 bytes are one word), then `-` and the 16 hex digits of the
/// 64-bit FNV-1a hash of the whole result.
fn written(name: &str) -> String {
    let mut readable = String::with_capacity(name.len());
    for c in name.chars().map(|c| c.to_ascii_lowercase()) {
        if c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-') {
            readable.push(c);
        } else if !readable.ends_with('.') {
            readable.push('.');
        }
    }
    let readable = readable.trim_matches(|c: char| !c.is_ascii_alphanumeric());
    if readable.len() <= MAX_VALUE_LEN {
        return readable.to_owned();
    }
    // Every byte is ASCII here, so any index is a char boundary.
    let (dropped, mut kept) = readable.split_at(readable.len() - KEPT_LEN);
    if !dropped.ends_with('.')
        && let Some((_, rest)) = kept.split_once('.')
    {
        kept = rest;
    }
    let kept = kept.trim_start_matches(|c: char| !c.is_ascii_alphanumeric());
    format!("{kept}-{:016x}", fnv1a(readable.as_bytes()))
}

/// The 64-bit FNV-1a hash of `bytes`.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(FNV_OFFSET, |hash, &byte| {
        (hash ^ u64::from(byte)).wrapping_mul(FNV_PRIME)
    })
}

#[cfg(test)]
#[path = "size_class_tests.rs"]
mod tests;
