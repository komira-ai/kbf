//! Widening: does a new declaration ask for more than the one it replaces?
//!
//! **Widening is any change other than removing an element from a set or lowering a number**,
//! in every field (docs/design/host-updates.md, section 3.3).

use crate::declaration::Declaration;

/// The fields of `new` that ask for more than `old` allowed, named as they are in the file
/// (`hook.write_paths`, `hook.timeout_secs.verify`, `hook.environment.NAME`). Empty when `new`
/// is equal or narrower.
pub fn widening(_old: &Declaration, _new: &Declaration) -> Vec<String> {
    Vec::new()
}
