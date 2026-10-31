//! The real ports of the hook, from its flags. Not written yet.

use super::HookWorld;
use super::flags::Flags;

/// The world of this host.
///
/// # Errors
/// Always, for now.
pub fn world(flags: &Flags) -> Result<HookWorld, String> {
    let _ = flags;
    Err("the real ports are not written yet".to_owned())
}
