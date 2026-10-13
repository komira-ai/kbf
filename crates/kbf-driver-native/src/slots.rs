//! Action slots: the lease-user pool a root daemon is to run actions as
//! (docs/design/macos-end-state.md, step 3). So far only the pool's types and the
//! `as-user` mode an action of the pool is started through: no action runs as a pool
//! user yet, and [`crate::NativeConfig::pool`] is not read.

/// The argument after argv0 that selects this binary's `as-user` mode.
pub const AS_USER_ARG: &str = "__as-user";

/// One pool user an action can run as.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SlotUser {
    /// Its name in the user database; the daemon checks that the uid has it.
    pub name: String,
    pub uid: u32,
    /// Its primary group.
    pub gid: u32,
}

/// The pool users actions run as, one per slot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pool {
    users: Vec<SlotUser>,
}

/// Why a pool was refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PoolError {
    #[error("a pool needs 1 to 64 users, not {0}")]
    Size(usize),
}

impl Pool {
    /// The pool `users`, slot `i` being `users[i]`.
    ///
    /// # Errors
    /// None yet.
    pub fn new(users: Vec<SlotUser>) -> Result<Self, PoolError> {
        Ok(Self { users })
    }

    /// The users, slot by slot.
    #[must_use]
    pub fn users(&self) -> &[SlotUser] {
        &self.users
    }
}

/// [`AS_USER_ARG`]'s mode, when this process's arguments ask for it: drops to the uid
/// and group they name and executes the program (`kbf-mac-session`'s `as-user`, which
/// returns only on failure). `None` otherwise, and always off macOS. A binary that
/// starts actions as pool users (`kbf-daemon`) calls this first in `main`.
#[must_use]
pub fn dispatch() -> Option<std::process::ExitCode> {
    #[cfg(target_os = "macos")]
    {
        let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
        if args.get(1).is_some_and(|a| a == AS_USER_ARG) {
            return Some(kbf_mac_session::macos::gui::as_user(&args[2..]));
        }
    }
    None
}

/// [`dispatch`] for a test binary, which has no `main` of its own: when the arguments
/// ask for the mode, runs it and exits (with 127 whatever went wrong: the mode returns
/// only when it did not execute the program, and has said why on stderr). Called by
/// [`as_user_test_hook!`](crate::as_user_test_hook) before the test harness starts.
#[doc(hidden)]
pub fn hook() {
    #[cfg(target_os = "macos")]
    if dispatch().is_some() {
        std::process::exit(127);
    }
}

/// Makes the test binary it is expanded in answer [`AS_USER_ARG`] as `kbf-daemon`
/// does, so the tests that start actions as pool users start them through it. It runs
/// [`hook`] before the test harness reads its arguments, from the platform's list of
/// start-up functions.
#[macro_export]
macro_rules! as_user_test_hook {
    () => {
        const _: () = {
            extern "C" fn kbf_as_user_hook() {
                $crate::slots::hook();
            }
            #[used]
            #[cfg_attr(target_os = "linux", unsafe(link_section = ".init_array"))]
            #[cfg_attr(
                target_vendor = "apple",
                unsafe(link_section = "__DATA,__mod_init_func,mod_init_funcs")
            )]
            static KBF_AS_USER_HOOK: extern "C" fn() = kbf_as_user_hook;
        };
    };
}
