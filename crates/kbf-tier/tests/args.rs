//! The tier's flags.

use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use kbf_tier::{TierArgs, TierOptions};

#[derive(Debug, Parser)]
struct Cli {
    #[command(flatten)]
    tier: TierArgs,
}

fn parse(args: &[&str]) -> Result<TierArgs, clap::Error> {
    Cli::try_parse_from(std::iter::once("kbf-server").chain(args.iter().copied())).map(|c| c.tier)
}

/// Catches: a tier that is on without `--write-tier-dir` (the OSS default must stay
/// today's guarantee), a directory without a budget or a budget without a directory
/// accepted, a zero budget or seal delay accepted, and flags not reaching the
/// options (the seal delay's default of 30 s, the segment limit).
#[test]
fn the_tier_is_off_unless_a_directory_is_given() {
    let off = parse(&[]).unwrap();
    assert_eq!(off.config(), None);
    assert_eq!(off.write_tier_seal_after, 30);

    let on = parse(&["--write-tier-dir", "/t", "--write-tier-bytes", "100"]).unwrap();
    assert_eq!(
        on.config(),
        Some((PathBuf::from("/t"), TierOptions::new(100)))
    );
    assert_eq!(TierOptions::new(100).seal_after, Duration::from_secs(30));
    assert_eq!(TierOptions::new(100).segment_limit, 128 << 20);

    let tuned = parse(&[
        "--write-tier-dir",
        "/t",
        "--write-tier-bytes",
        "7",
        "--write-tier-seal-after",
        "5",
    ])
    .unwrap();
    let (_, options) = tuned.config().unwrap();
    assert_eq!(options.seal_after, Duration::from_secs(5));
    assert_eq!(options.budget, 7);

    for bad in [
        &["--write-tier-dir", "/t"][..],
        &["--write-tier-bytes", "7"],
        &["--write-tier-dir", "/t", "--write-tier-bytes", "0"],
        &["--write-tier-seal-after", "0"],
    ] {
        assert!(parse(bad).is_err(), "{bad:?}");
    }
}

/// Catches: an abandon flag accepted without a tier directory (it would mark objects
/// missing on a start that has no tier to replace them), a zero id accepted, and the
/// flag's id not reaching [`TierArgs::abandon`].
#[test]
fn abandon_names_a_tier_and_needs_a_directory() {
    assert_eq!(parse(&[]).unwrap().abandon(), None);
    let on = parse(&[
        "--write-tier-dir",
        "/t",
        "--write-tier-bytes",
        "7",
        "--write-tier-abandon",
        "9",
    ])
    .unwrap();
    assert_eq!(on.abandon(), Some(kbf_tier::TierId::new(9)));
    for bad in [
        &["--write-tier-abandon", "9"][..],
        &[
            "--write-tier-dir",
            "/t",
            "--write-tier-bytes",
            "7",
            "--write-tier-abandon",
            "0",
        ],
    ] {
        assert!(parse(bad).is_err(), "{bad:?}");
    }
}
