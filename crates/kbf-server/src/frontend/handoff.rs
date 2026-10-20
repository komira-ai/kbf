//! The flags of automatic promotion of joiners (`docs/design/rebalance-handoff.md`, section
//! 5): `--auto-promote` (default off), `--handoff-settle-secs` and
//! `--handoff-allow-degraded`, and the [`Handoff`] they make.
//!
//! The settle time and the degraded switch do not require the switch: the config template
//! (`kbf-cli`) writes every flag with a default as a live line, so a flag that required
//! another would refuse a frontend started from the template. Without `--auto-promote` they
//! change nothing.

use std::time::Duration;

use clap::{Arg, ArgAction, ArgMatches};

use crate::members::Handoff;

const AUTO_PROMOTE: &str = "auto_promote";
const SETTLE: &str = "handoff_settle_secs";
const ALLOW_DEGRADED: &str = "handoff_allow_degraded";

/// The settle time when the flag is not given, in seconds: [`Handoff::default`]'s.
const DEFAULT_SETTLE: &str = "30";

/// The three flags.
pub(super) fn args() -> [Arg; 3] {
    [
        Arg::new(AUTO_PROMOTE)
            .long("auto-promote")
            .action(ArgAction::SetTrue)
            .requires("state_store")
            .help(
                "Promote a storage node that joined a farm holding data from joining to active \
                 by its own heartbeat once it is filled: every member has flushed what it \
                 staged before the join, the joiner has indexed every object in the cold store \
                 under the frontend's tail epoch, its placement fingerprint matches, no member \
                 is down, and the settle time has passed. Off by default: an operator promotes \
                 a joiner (`POST /v1/members/{id}`, state active), which stays the override. \
                 Frontends first, then nodes, then this flag.",
            ),
        Arg::new(SETTLE)
            .long("handoff-settle-secs")
            .value_name("SECONDS")
            .value_parser(clap::value_parser!(u64))
            .default_value(DEFAULT_SETTLE)
            .help(
                "How long, in seconds, all the conditions of --auto-promote must keep holding \
                 at the joiner's beats before it is promoted. Used only with --auto-promote.",
            ),
        Arg::new(ALLOW_DEGRADED)
            .long("handoff-allow-degraded")
            .action(ArgAction::SetTrue)
            .help(
                "Let --auto-promote promote a joiner although a member was down during its \
                 fill. A member that is down and inside its dwell still holds the first phase \
                 back. Off by default: a down member may hold the only copy of a blob. Used \
                 only with --auto-promote.",
            ),
    ]
}

/// The setting the flags in `m` make: `None` without `--auto-promote`.
pub(super) fn from_matches(m: &ArgMatches) -> Option<Handoff> {
    m.get_flag(AUTO_PROMOTE).then(|| Handoff {
        settle: Duration::from_secs(
            m.get_one::<u64>(SETTLE)
                .copied()
                .unwrap_or(Handoff::default().settle.as_secs()),
        ),
        allow_degraded: m.get_flag(ALLOW_DEGRADED),
    })
}
