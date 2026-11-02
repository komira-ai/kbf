//! `--rollback-target`: a services file that names a slot pool is refused while a kept
//! coordinator release to go back to would refuse it, and accepted once none remains.

use kbf_updater::version::Version;

use crate::rollback::{FIRST_POOL_VERSION, check};
use crate::services::{Services, ServicesError, TIERSTORE_SERVICES};

/// The shipped tierstore entry with the documented pool line added.
fn pooled() -> Services {
    let text = TIERSTORE_SERVICES.replace("min_up = 4\n", "min_up = 4\npool = \"storage\"\n");
    assert_ne!(text, TIERSTORE_SERVICES);
    Services::parse(&text).expect("parsed")
}

fn targets(list: &[&str]) -> Vec<String> {
    list.iter().map(|t| (*t).to_owned()).collect()
}

fn refusal(services: &Services, list: &[&str]) -> String {
    match check(services, &targets(list)) {
        Err(ServicesError::Invalid(why)) => why,
        other => panic!("{list:?}: {other:?}"),
    }
}

/// Catches: a pool-enabled file accepted while a kept release before pools could be put back,
/// which would then refuse the file and not start. With 0.0.57 kept (alone, or after a release
/// that reads pools) the file is refused, naming the target, the field, and the pool's service
/// and role, in full.
#[test]
fn a_pool_is_refused_while_a_release_before_pools_is_a_rollback_target() {
    let services = pooled();
    let expected = "tierstore: pool \"storage\" on storage: the rollback target 0.0.57 \
                    (--rollback-target) is a release before 0.0.61 and refuses the field `pool`; \
                    remove the pool, or the target once its image is no longer kept";
    assert_eq!(refusal(&services, &["0.0.57"]), expected);
    assert_eq!(refusal(&services, &["0.0.62", "0.0.57"]), expected);
    assert_eq!(
        refusal(&services, &["0.0.60+g0123456789ab"]),
        expected.replace("0.0.57", "0.0.60+g0123456789ab")
    );
    assert!(refusal(&services, &["0.0"]).contains("rollback target 0.0 "));
}

/// Catches: a rollback target that cannot be ordered being taken as able to read pools. A
/// bare commit id (whose leading digits would order as a huge number), a word, an empty text
/// and one that is no version at all are each refused as not a release version.
#[test]
fn a_rollback_target_that_is_not_a_release_version_refuses_a_pool() {
    let services = pooled();
    for bad in ["0123456789ab", "latest", "", "a b"] {
        assert_eq!(
            refusal(&services, &[bad]),
            format!(
                "tierstore: pool \"storage\" on storage: --rollback-target {bad:?} is not a \
                 release version (such as 0.0.57); remove the pool, or the target once its \
                 image is no longer kept"
            ),
            "{bad:?}"
        );
    }
}

/// Catches: a malformed version ordered as a release. Each of these would be read by
/// [`Version::release_numbers`] as some release at or after the floor (an empty number is
/// skipped, letters after the digits are dropped), so each must be refused as not a release
/// version: empty numbers inside, leading or trailing dots, a lone number, and digits run
/// into a word.
#[test]
fn a_malformed_release_version_is_refused_not_ordered() {
    let services = pooled();
    for bad in [
        "0..61",
        "0.0..61",
        ".0.61",
        "0.0.61.",
        "0.0.61.+g0123456789ab",
        "..",
        "61",
        "0.0.61abc",
        "0.0.61.x",
    ] {
        assert_eq!(
            refusal(&services, &[bad]),
            format!(
                "tierstore: pool \"storage\" on storage: --rollback-target {bad:?} is not a \
                 release version (such as 0.0.57); remove the pool, or the target once its \
                 image is no longer kept"
            ),
            "{bad:?}"
        );
    }
    let well_formed = [
        "0.0.61",
        "0.0.61+g0123456789ab-8",
        "0.0.61-1",
        "0.0.61~rc1",
        "1.0",
    ];
    assert_eq!(check(&services, &targets(&well_formed)), Ok(()));
}

/// Catches: the refusal outliving its reason. With no target listed, or only targets at or
/// after the first release that reads pools (with or without a build suffix, and a later
/// minor), the pool-enabled file is accepted.
#[test]
fn a_pool_is_accepted_once_no_release_before_pools_remains() {
    let services = pooled();
    assert_eq!(check(&services, &[]), Ok(()));
    let capable = [
        FIRST_POOL_VERSION,
        "0.0.61+g0123456789ab-8",
        "0.0.62",
        "0.1",
        "1.0.0",
    ];
    assert_eq!(check(&services, &targets(&capable)), Ok(()));
}

/// Catches: the check reaching files with no pool, or kbf's built-in pool. A file with no
/// pool (the shipped tierstore entry), and no file at all (kbf as shipped, whose storage role
/// is in a pool that no release reads from a file), are accepted with a release before pools
/// kept, and with a target that is no version.
#[test]
fn a_file_without_a_pool_is_accepted_whatever_the_targets() {
    let old = targets(&["0.0.57", "not-a-version"]);
    let plain = Services::parse(TIERSTORE_SERVICES).expect("parsed");
    assert_eq!(check(&plain, &old), Ok(()));
    assert_eq!(check(&Services::default(), &old), Ok(()));
}

/// Catches: a replaced kbf entry's pool slipping past the check. A file whose first entry
/// replaces kbf with its storage role in a pool is a file that names a pool.
#[test]
fn a_replaced_kbf_entry_with_a_pool_is_checked() {
    let text = "[[service]]\nname = \"kbf\"\n[service.roles.storage]\npool = \"storage\"\n";
    let services = Services::parse(text).expect("parsed");
    assert!(refusal(&services, &["0.0.57"]).starts_with("kbf: pool \"storage\" on storage: "));
}

/// The floor the check compares with is the one its messages name.
#[test]
fn the_floor_is_first_pool_version() {
    let floor = Version::parse(FIRST_POOL_VERSION).expect("a version");
    assert_eq!(floor.release_numbers(), [0, 0, 61]);
    let services = pooled();
    let just_before = targets(&["0.0.60"]);
    assert!(check(&services, &just_before).is_err());
    assert_eq!(check(&services, &targets(&["0.0.61"])), Ok(()));
}
