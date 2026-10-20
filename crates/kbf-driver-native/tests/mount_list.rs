//! The reading of `/sbin/mount` and the comparison of two readings that
//! `tests/user_folders.rs` makes around a lease, tested where they can run on any
//! machine (the macOS test only feeds them the real listing).

#[path = "mount_list/helpers.rs"]
mod helpers;
