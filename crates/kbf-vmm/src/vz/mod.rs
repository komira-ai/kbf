//! Virtualization.framework, through `objc2-virtualization`. This is the only module that
//! calls the framework; everything above it sees `System` and plain values.
//!
//! # FFI-BOUNDARY
//! Every Objective-C object is held in a `Retained<T>` (objc2 releases it when the last
//! owner drops it). Objects created here are owned by the function that created them, or
//! by the `Running` value in `helper` for the lifetime of a VM:
//! - `VZVirtualMachine`: created on and used only on `Running::queue`, released with
//!   `Running`.
//! - `VZVirtualMachineConfiguration` and the device configurations: built on the calling
//!   thread, copied by the VM at creation, released when the builder returns.
//! - `Observer` (delegate): retained by `Running` (the framework holds delegates weakly).
//! - `VZVirtioSocketListener`: retained by `Running` (the device holds it weakly).
//! - Pipe file descriptors of console ports: the helper owns both ends; the guest-facing
//!   `NSFileHandle`s are created with `closeOnDealloc: false`, and the helper closes the
//!   descriptors when it exits.
//!
//! # SAFETY
//! The framework requires every VM call to run on the VM's serial queue; [`Queued`] wraps
//! a value that is only ever touched inside `DispatchQueue::exec_*` on that queue. The
//! wrapper's `Send` impl is the one place that claims it.

use objc2::rc::Retained;
use objc2_foundation::{NSData, NSError, NSString, NSURL};
use objc2_virtualization::{VZErrorCode, VZMacMachineIdentifier};
use std::path::Path;

pub mod config;
pub mod helper;
pub mod install;

/// A value that is only used on one dispatch queue.
pub struct Queued<T>(T);

// SAFETY: `Queued` values are created on one thread and then only dereferenced inside
// closures that run on the serial queue the value belongs to (see the module docs).
unsafe impl<T> Send for Queued<T> {}

impl<T> Queued<T> {
    pub fn new(v: T) -> Self {
        Self(v)
    }
    pub fn get(&self) -> &T {
        &self.0
    }
}

/// A file URL for `path`.
pub fn url(path: &Path) -> Retained<NSURL> {
    NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()))
}

/// The error code name for a framework error code, when it has one.
fn code_name(code: isize) -> Option<&'static str> {
    let c = VZErrorCode(code);
    let table: [(VZErrorCode, &str); 12] = [
        (VZErrorCode::Internal, "Internal"),
        (
            VZErrorCode::InvalidVirtualMachineConfiguration,
            "InvalidVirtualMachineConfiguration",
        ),
        (
            VZErrorCode::InvalidVirtualMachineState,
            "InvalidVirtualMachineState",
        ),
        (
            VZErrorCode::InvalidVirtualMachineStateTransition,
            "InvalidVirtualMachineStateTransition",
        ),
        (VZErrorCode::InvalidDiskImage, "InvalidDiskImage"),
        (
            VZErrorCode::VirtualMachineLimitExceeded,
            crate::spec::LIMIT_EXCEEDED,
        ),
        (VZErrorCode::OutOfDiskSpace, "OutOfDiskSpace"),
        (VZErrorCode::NotSupported, "NotSupported"),
        (VZErrorCode::InvalidRestoreImage, "InvalidRestoreImage"),
        (
            VZErrorCode::InstallationRequiresUpdate,
            "InstallationRequiresUpdate",
        ),
        (VZErrorCode::InstallationFailed, "InstallationFailed"),
        (
            VZErrorCode::DeviceInitializationFailure,
            "DeviceInitializationFailure",
        ),
    ];
    table.iter().find(|(k, _)| *k == c).map(|(_, n)| *n)
}

/// An `NSError` as the message and, for a Virtualization error, the code name.
pub fn describe(e: &NSError) -> (String, Option<String>) {
    let domain = e.domain().to_string();
    let code = e.code();
    let name = if domain == "VZErrorDomain" {
        code_name(code)
    } else {
        None
    };
    (
        format!("{} ({domain} {code})", e.localizedDescription()),
        name.map(str::to_owned),
    )
}

/// The data representation of a new machine identifier.
pub fn new_machine_identifier() -> Vec<u8> {
    // SAFETY: `new` returns an owned identifier; `dataRepresentation` an owned copy of its
    // bytes; both are released at the end of this function.
    unsafe {
        let id: Retained<VZMacMachineIdentifier> = VZMacMachineIdentifier::new();
        let data: Retained<NSData> = id.dataRepresentation();
        data.to_vec()
    }
}
