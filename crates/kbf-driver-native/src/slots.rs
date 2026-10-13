//! Action slots: the lease-user pool a root daemon runs actions as
//! (docs/design/macos-end-state.md, step 3).
//!
//! On a Mac whose daemon runs as root, every action runs as a pool user of its own,
//! one per slot: `kbf-pool-0` .. `kbf-pool-<N-1>`, uids `first` .. `first + N - 1`,
//! primary group id `first`, as kbf-mac-provision makes them (`pool_users`,
//! `pool_first_uid`; the daemon's `--pool-users` and `--pool-first-uid`). What one slot
//! gets, and how its processes are ended, is macOS's part ([`macos`]):
//!
//! - a lease takes a free slot, waiting for one while all are busy, and gives it back
//!   once the lease has ended and its directory is gone;
//! - its lease directory is the slot user's, and its action is started through
//!   `launchctl asuser <uid>`, then this binary's `__as-user` mode, which drops to the
//!   slot's uid and group (no supplementary groups) and executes it: so it runs in
//!   `user/<uid>`'s bootstrap namespace, with the per-user services (`cfprefsd`,
//!   the keychain) of its own user;
//! - the sandbox lets it write its lease directory, `/dev`, the slot user's real home
//!   and its `/var/folders` entry, and no longer denies `defaults write`, `open` or
//!   handing launchd a job: what those protected was a uid every lease shared;
//! - its memory is the sum over the processes whose effective uid is the slot's;
//! - its end, the keeper's and the start-up's are one rule, every process of the uid
//!   ([`kbf_daemon::uid::end`]).
//!
//! No action runs as root: a pool user below [`MIN_UID`] (root's 0 among them) is
//! refused here, and a root daemon without a pool refuses to start
//! ([`check_daemon_user`]). Linux native keeps its process-group tracking
//! ([`crate::procs`]); a pool there is refused.
//!
//! Not yet (step 4 of the design): the clean of a slot between two leases (its home,
//! its `/var/folders` entry, keychains, crontab and `at` jobs, disk images, IPC
//! objects): [`macos::Slot::clean`] runs only the kill so far. Nor does the scheduler
//! book slots: a lease past `N` waits here for a free one.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use kbf_daemon::RuntimeError;

use crate::network::{Isolation, Network};
use crate::record::KeeperFor;

#[cfg(target_os = "macos")]
pub mod macos;

/// The lowest uid a pool user may have: every uid below it is root's, a system
/// account's, or (from 501) one a Mac gives its first human users. The pool's uids
/// start at 600 or above in provisioning too.
pub const MIN_UID: u32 = 600;

/// The most users a pool may have, as in provisioning.
pub const MAX_USERS: u32 = 64;

/// The node report entry that says how many actions run at once.
pub const CAPABILITY: &str = "action_slots";

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
    #[error("a pool needs 1 to {MAX_USERS} users, not {0}")]
    Size(usize),
    #[error("pool user {name} has uid 0: no action runs as root")]
    Root { name: String },
    #[error("pool user {name} has uid {uid}, below {MIN_UID}: a system account's or a person's")]
    LowUid { name: String, uid: u32 },
    #[error("pool user {name} has group id 0")]
    RootGroup { name: String },
    #[error("uid {0} is in the pool twice")]
    Twice(u32),
    #[error("a pool of {count} users from uid {first} runs past the last uid")]
    Overflow { count: u32, first: u32 },
}

impl Pool {
    /// The pool `users`, slot `i` being `users[i]`.
    ///
    /// # Errors
    /// No users or more than [`MAX_USERS`], a uid below [`MIN_UID`] (root's 0
    /// among them), group id 0, or a uid twice.
    pub fn new(users: Vec<SlotUser>) -> Result<Self, PoolError> {
        if users.is_empty() || users.len() > MAX_USERS as usize {
            return Err(PoolError::Size(users.len()));
        }
        let mut seen = BTreeSet::new();
        for user in &users {
            let name = user.name.clone();
            if user.uid == 0 {
                return Err(PoolError::Root { name });
            }
            if user.uid < MIN_UID {
                let uid = user.uid;
                return Err(PoolError::LowUid { name, uid });
            }
            if user.gid == 0 {
                return Err(PoolError::RootGroup { name });
            }
            if !seen.insert(user.uid) {
                return Err(PoolError::Twice(user.uid));
            }
        }
        Ok(Self { users })
    }

    /// The pool kbf-mac-provision makes: `kbf-pool-<i>` for `i < count`, uid
    /// `first + i`, primary group id `first`.
    ///
    /// # Errors
    /// As [`Pool::new`], and a pool whose last uid would not fit.
    pub fn provisioned(count: u32, first: u32) -> Result<Self, PoolError> {
        let last = first.checked_add(count.saturating_sub(1));
        if last.is_none() {
            return Err(PoolError::Overflow { count, first });
        }
        Self::new(
            (0..count)
                .map(|i| SlotUser {
                    name: format!("kbf-pool-{i}"),
                    uid: first + i,
                    gid: first,
                })
                .collect(),
        )
    }

    /// The users, slot by slot.
    #[must_use]
    pub fn users(&self) -> &[SlotUser] {
        &self.users
    }
}

/// Why a root daemon without a pool does not start.
const ROOT_WITHOUT_POOL: &str = "the daemon runs as root and has no lease-user pool \
     (--pool-users): it runs no action as root";

/// Whether the daemon may run actions as it is: a root daemon only with a pool (it
/// would otherwise run every action as root), a pool only with a root daemon (only
/// root can start a process as another uid and kill all of a uid's).
///
/// # Errors
/// What is refused, and why.
#[cfg(target_os = "macos")]
pub fn check_daemon_user(root: bool, pool: Option<&Pool>) -> Result<(), String> {
    match (root, pool) {
        (true, None) => Err(ROOT_WITHOUT_POOL.to_owned()),
        (false, Some(_)) => Err("a lease-user pool needs a daemon running as root".to_owned()),
        _ => Ok(()),
    }
}

/// Whether the daemon may run actions as it is: off macOS, never with a pool, and never
/// as root (it would run every action as root).
///
/// # Errors
/// What is refused, and why.
#[cfg(not(target_os = "macos"))]
pub fn check_daemon_user(root: bool, pool: Option<&Pool>) -> Result<(), String> {
    match (root, pool) {
        (_, Some(_)) => Err("a lease-user pool runs on macOS only".to_owned()),
        (true, None) => Err(ROOT_WITHOUT_POOL.to_owned()),
        (false, None) => Ok(()),
    }
}

/// How a lease's action runs: as the daemon's own user, its processes followed by
/// process group ([`crate::procs`]), or as the pool user of the slot it took (macOS).
#[derive(Debug, Default)]
pub(crate) enum Runner {
    #[default]
    Daemon,
    #[cfg(target_os = "macos")]
    Slot(macos::Slot),
}

impl Runner {
    /// The program and arguments that run `program` with `args` under `network` in
    /// `lease`: the sandbox's wrap ([`Isolation::wrap`], with `rules`), or a slot's
    /// launch ([`macos::Slot::launch`], with its own rules).
    ///
    /// # Errors
    /// A slot refused to run the action (its uid is 0).
    pub fn wrap(
        &self,
        isolation: &Isolation,
        (network, lease, rules): (Network, &Path, &str),
        program: PathBuf,
        args: &[String],
    ) -> Result<(PathBuf, Vec<String>), RuntimeError> {
        match self {
            Self::Daemon => Ok(isolation.wrap(network, lease, rules, program, args)),
            #[cfg(target_os = "macos")]
            Self::Slot(slot) => slot.launch(isolation, network, lease, program, args),
        }
    }

    /// Makes the lease directory `dir` the slot user's ([`macos::Slot::hand_over`]);
    /// nothing for the daemon's user, whose it is.
    ///
    /// # Errors
    /// It could not be handed over.
    #[cfg_attr(
        not(target_os = "macos"),
        expect(unused_variables, reason = "no slots off macOS")
    )]
    pub async fn hand_over(&self, dir: &Path) -> Result<(), RuntimeError> {
        match self {
            Self::Daemon => Ok(()),
            #[cfg(target_os = "macos")]
            Self::Slot(slot) => slot.hand_over(dir).await,
        }
    }

    /// The keeper the action gets.
    pub fn keeper(&self) -> KeeperFor {
        match self {
            Self::Daemon => KeeperFor::Group,
            #[cfg(target_os = "macos")]
            Self::Slot(slot) => KeeperFor::Uid(slot.ready().user.uid),
        }
    }

    /// Refuses a simulator to an action that runs as a slot user: `simctl` runs as the
    /// daemon, and its devices would be in root's device set, which the action does not
    /// see (step 6 of docs/design/macos-end-state.md moves them into the slot user's).
    ///
    /// # Errors
    /// The lease asked for a simulator and runs in a slot.
    #[cfg_attr(
        not(target_os = "macos"),
        expect(unused_variables, reason = "no slots off macOS")
    )]
    pub fn allows_simulator(&self, asked: bool) -> Result<(), RuntimeError> {
        match self {
            Self::Daemon => Ok(()),
            #[cfg(target_os = "macos")]
            Self::Slot(_) if asked => Err(RuntimeError::Failed(
                "simulator: not yet for an action run as a pool user".to_owned(),
            )),
            #[cfg(target_os = "macos")]
            Self::Slot(_) => Ok(()),
        }
    }

    /// Gives a slot back once the lease has ended and its directory is gone
    /// ([`macos::Slot::release`]).
    pub fn release(self) {
        match self {
            Self::Daemon => {}
            #[cfg(target_os = "macos")]
            Self::Slot(slot) => slot.release(),
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn user(name: &str, uid: u32, gid: u32) -> SlotUser {
        SlotUser {
            name: name.to_owned(),
            uid,
            gid,
        }
    }

    /// Catches a pool that lets an action run as root (uid 0), as a system account or
    /// a person (a uid below 600, whose every process the kill would end), with group
    /// 0, or as one uid in two slots (two leases sharing a user); and a provisioned
    /// pool whose names, uids or group differ from kbf-mac-provision's.
    #[test]
    fn a_pool_never_holds_root_a_low_uid_or_a_uid_twice() {
        let refused = [
            (vec![], PoolError::Size(0)),
            (
                vec![user("a", 0, 700)],
                PoolError::Root {
                    name: "a".to_owned(),
                },
            ),
            (
                vec![user("b", 501, 700)],
                PoolError::LowUid {
                    name: "b".to_owned(),
                    uid: 501,
                },
            ),
            (
                vec![user("c", 700, 0)],
                PoolError::RootGroup {
                    name: "c".to_owned(),
                },
            ),
            (
                vec![user("d", 700, 700), user("e", 700, 700)],
                PoolError::Twice(700),
            ),
            (vec![user("f", 700, 700); 65], PoolError::Size(65)),
        ];
        for (users, want) in refused {
            assert_eq!(Pool::new(users), Err(want.clone()), "{want}");
        }
        let edge = Pool::new(vec![user("g", MIN_UID, 1)]).expect("the lowest uid");
        assert_eq!(edge.users(), [user("g", 600, 1)]);
        let pool = Pool::provisioned(2, 700).expect("pool");
        assert_eq!(
            pool.users(),
            [user("kbf-pool-0", 700, 700), user("kbf-pool-1", 701, 700)]
        );
        assert_eq!(
            Pool::provisioned(2, u32::MAX),
            Err(PoolError::Overflow {
                count: 2,
                first: u32::MAX
            })
        );
        assert_eq!(Pool::provisioned(0, 700), Err(PoolError::Size(0)));
        assert!(matches!(
            Pool::provisioned(1, 0),
            Err(PoolError::Root { .. })
        ));
    }

    /// Catches a root daemon that starts without a pool (every action would run as
    /// root), a pool taken by a daemon that is not root (it cannot start anything as
    /// another uid) or off macOS, and a refusal of the ordinary daemon (no pool, not
    /// root).
    #[test]
    fn only_a_root_daemon_on_macos_runs_a_pool_and_it_must() {
        let pool = Pool::provisioned(1, 700).expect("pool");
        assert_eq!(check_daemon_user(false, None), Ok(()));
        let root = check_daemon_user(true, None).expect_err("root without a pool");
        assert!(root.contains("no action as root"), "{root}");
        let as_user = check_daemon_user(false, Some(&pool)).expect_err("a pool, not root");
        let as_root = check_daemon_user(true, Some(&pool));
        #[cfg(target_os = "macos")]
        {
            assert!(as_user.contains("as root"), "{as_user}");
            assert_eq!(as_root, Ok(()));
        }
        #[cfg(not(target_os = "macos"))]
        {
            assert!(as_user.contains("macOS only"), "{as_user}");
            assert!(as_root.expect_err("off macOS").contains("macOS only"));
        }
    }

    /// Catches the `as-user` mode taken for arguments that do not ask for it (the test
    /// harness's own run would end in it).
    #[test]
    fn only_the_as_user_argument_selects_the_mode() {
        assert!(dispatch().is_none());
        hook();
    }
}
