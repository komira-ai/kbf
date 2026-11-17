//! The class the job service sizes an action's job by: the value of the job's `size-class`
//! label. A job that leaves its CPU and memory at zero is booked what the history of its class
//! says (jobscheduler's `docs/design/sizing.md`, section 7.7), so every run of one piece of work
//! must name one class, and no other work may share it.
//!
//! The work is the build target the action belongs to, read from the action's own `buck-out`
//! paths ([`Shape::target`]: buck2 sends no target with the call), and what the action does to
//! it: a test run ([`Shape::test`]) is one kind and anything else, a compile or a link, the
//! other. One target's compile and its test are two classes, `<target> build` and
//! `<target> test`, so a test that needs much more memory than its compile does not book that
//! for the compile, nor the compile's size for the test. The words are written as a label
//! value as jobscheduler writes a target's name: `root//crates/kbf-server:kbf-server` built is
//! `root.crates.kbf-server.kbf-server.build`.
//!
//! An action whose paths name no target has the class of its shape's key
//! ([`crate::action_usage::ShapeKey`], `shape.<16 hex digits>`): what stays the same across builds of one action, so its runs
//! still share a history, and a class of its own, apart from every other such action.
//!
//! No two kinds of class meet: a target's ends in `.build` or `.test`, or, cut because it is
//! long, in `-` and 16 hex digits; a shape's is `shape.` and 16 hex digits, and is never cut.
//!
//! The words are written as `js_types::SizeClass::of_target` writes them, byte for byte, by a
//! copy of its rule here: the jobscheduler revision this workspace takes predates it. The tests
//! hold the copy to jobscheduler's own examples and hashes.

use crate::action_usage::Shape;

/// The label whose value names a job's class: jobscheduler's `js_types::SIZE_CLASS_LABEL`.
pub(crate) const SIZE_CLASS_LABEL: &str = "size-class";

/// Longest label value, in bytes.
const MAX_VALUE_LEN: usize = 63;

/// The bytes a long name keeps: a label value's 63, less `-` and 16 hex digits.
const KEPT_LEN: usize = MAX_VALUE_LEN - 17;

/// The 64-bit FNV-1a offset basis.
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;

/// The 64-bit FNV-1a prime.
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// A job's size class: a valid value of the label [`SIZE_CLASS_LABEL`] (1 to 63 bytes of
/// `[a-z0-9._-]`) that starts and ends with a letter or a digit.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SizeClass(String);

impl SizeClass {
    /// The class of the action whose shape is `shape` (see the module docs).
    #[must_use]
    pub fn of_action(shape: &Shape) -> Self {
        match &shape.target {
            // Never empty: the kind is a word.
            Some(target) => {
                let kind = if shape.test { "test" } else { "build" };
                Self(written(&format!("{} {kind}", target.label)))
            }
            None => Self(format!("shape.{}", shape.key)),
        }
    }

    /// The class as the label's value.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
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
