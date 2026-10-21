//! The size budget of a `kbf-installer` package that carries a kbf release
//! (`xtask::pkg_size`).

use xtask::package::Platform;
use xtask::pkg_size::{BUDGETS, budget, problem};

const MB: u64 = 1_000_000;

/// Catches a budget that is off by one at its edge (a package of exactly the budget is
/// within it, one byte more is not), a check that never refuses, and a refusal that does not
/// say which platform, how big the package is and what the budget is, which is all a reader
/// of a failed build has to go on.
#[test]
fn a_package_at_its_budget_is_accepted_and_one_byte_over_is_refused() {
    for (platform, limit) in BUDGETS {
        let name = platform.name();
        assert_eq!(budget(platform), Some(limit), "{name}");
        assert_eq!(problem(platform, 0), None, "{name}");
        assert_eq!(problem(platform, limit), None, "{name}");
        let said = problem(platform, limit + 1).expect("one byte over is refused");
        assert!(said.contains(name), "{said}");
        assert!(said.contains(&(limit + 1).to_string()), "{said}");
        assert!(said.contains(&limit.to_string()), "{said}");
    }
}

/// Catches a Linux platform left without a budget (its package would then be held to
/// nothing), and a budget for the Mac, whose package carries no release yet.
#[test]
fn each_linux_platform_has_a_budget_and_the_mac_has_none_yet() {
    for platform in [Platform::LinuxX86_64, Platform::LinuxArm64] {
        assert!(budget(platform).is_some(), "{}", platform.name());
    }
    assert_eq!(budget(Platform::DarwinArm64), None);
    assert_eq!(problem(Platform::DarwinArm64, u64::MAX), None);
}

/// Catches a budget raised to make a growing package pass: it must stay where the package
/// with a release is expected to weigh (33 to 43 MB, with room for a little more), and
/// raising it means moving this line in the same change, where a reviewer sees both.
#[test]
fn the_budgets_sit_where_the_package_with_a_release_is_expected_to_weigh() {
    for (platform, limit) in BUDGETS {
        assert!(
            (33 * MB..=45 * MB).contains(&limit),
            "{}: {limit}",
            platform.name()
        );
    }
}
