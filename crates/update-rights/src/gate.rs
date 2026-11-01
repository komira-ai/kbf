//! The decision on a new declaration: install it, or defer the update until it is approved.
//!
//! A new declaration is installed when its digest has been approved, or when there is an
//! active declaration and the new one asks for nothing more ([`widening`] is empty). Otherwise
//! the update is `Deferred` with the reason [`RIGHTS_CHANGE`], which holds no slot and does not
//! make the service `blocked`; it is lifted by `approve`.
//!
//! With no active declaration (the first install through the updater, not through the package
//! manager) nothing is approved to be narrower than, so the declaration must be approved: a
//! declaration with `hook.user = "root"` is deferred the first time it is installed.

use crate::approval::Approvals;
use crate::declaration::Declaration;
use crate::digest::Digest;
use crate::widening::widening;

/// The reason an update is deferred for a declaration that asks for more.
pub const RIGHTS_CHANGE: &str = "rights_change";

/// What the first install is compared with: nothing, so "first_install" is what it widens.
pub const FIRST_INSTALL: &str = "first_install";

/// What to do with a new declaration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Gate {
    /// Install it.
    Install,
    /// Defer the update until an operator approves `digest`.
    Deferred(Deferral),
}

/// Why an update is deferred, for the report and the operator who approves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Deferral {
    /// Always [`RIGHTS_CHANGE`].
    pub reason: &'static str,
    /// What `updater approve <service> <digest>` takes.
    pub digest: Digest,
    /// The fields that ask for more, as `widening` names them; [`FIRST_INSTALL`] for a
    /// declaration with nothing to compare with.
    pub widened: Vec<String>,
}

/// Decides on `incoming` (whose file has the digest `digest`) against the `active` declaration.
pub fn gate(
    active: Option<&Declaration>,
    incoming: &Declaration,
    digest: &Digest,
    approvals: &Approvals,
) -> Gate {
    let widened = active.map_or_else(
        || vec![FIRST_INSTALL.to_owned()],
        |active| widening(active, incoming),
    );
    if approvals.contains(digest) | widened.is_empty() {
        return Gate::Install;
    }
    Gate::Deferred(Deferral {
        reason: RIGHTS_CHANGE,
        digest: digest.clone(),
        widened,
    })
}
