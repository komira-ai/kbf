//! The grammars that keep a value from becoming something else in a systemd property.

use update_rights::names::{
    Capability, Family, MAX_PATH, PathError, UnitError, check_path, check_relative, check_unit,
    is_account_name, is_env_name, is_env_value, is_service_name,
};

/// Catches a service name that is not the wire's: names are lowercase letters, digits and `-`,
/// at most 64 bytes.
#[test]
fn service_names() {
    for good in ["a", "tierstore", "js-agent", "k9", &"a".repeat(64)] {
        assert!(is_service_name(good), "{good}");
    }
    for bad in ["", "Tier", "a_b", "a b", "a.b", "a/b", &"a".repeat(65)] {
        assert!(!is_service_name(bad), "{bad}");
    }
}

/// Catches an account name that no account has: a letter or `_` first, then letters, digits,
/// `_` and `-`, at most 32 bytes.
#[test]
fn account_names() {
    for good in [
        "root",
        "_apt",
        "tierstore-update",
        "a1_b-c",
        &"a".repeat(32),
    ] {
        assert!(is_account_name(good), "{good}");
    }
    for bad in [
        "",
        "1abc",
        "-a",
        "Root",
        "a b",
        "a:b",
        "a.b",
        &"a".repeat(33),
    ] {
        assert!(!is_account_name(bad), "{bad}");
    }
}

/// Catches a path that systemd would read as something else: a `-`, `+` or `!` prefix, a
/// relative path, white space, a specifier, an escape, a `..`.
#[test]
fn paths_refused_and_accepted() {
    for good in [
        "/",
        "/a",
        "/usr/sbin/nft",
        "/var/lib/x-y_z.d/a@b+c",
        "/a/b.c",
    ] {
        assert_eq!(check_path(good), Ok(()), "{good}");
    }
    assert_eq!(check_path("-/a"), Err(PathError::SystemdPrefix('-')));
    assert_eq!(check_path("+/a"), Err(PathError::SystemdPrefix('+')));
    assert_eq!(check_path("!/a"), Err(PathError::SystemdPrefix('!')));
    assert_eq!(check_path("a/b"), Err(PathError::NotAbsolute));
    assert_eq!(check_path(""), Err(PathError::NotAbsolute));
    assert_eq!(check_path("./a"), Err(PathError::NotAbsolute));
    assert_eq!(check_path("/a b"), Err(PathError::BadChar(' ')));
    assert_eq!(check_path("/a%i"), Err(PathError::BadChar('%')));
    assert_eq!(check_path("/a\\x20"), Err(PathError::BadChar('\\')));
    assert_eq!(check_path("/a:b"), Err(PathError::BadChar(':')));
    assert_eq!(check_path("/a\n"), Err(PathError::BadChar('\n')));
    assert_eq!(check_path("/é"), Err(PathError::BadChar('é')));
    assert_eq!(check_path("/a//b"), Err(PathError::EmptyComponent));
    assert_eq!(check_path("/a/"), Err(PathError::EmptyComponent));
    assert_eq!(check_path("//a"), Err(PathError::EmptyComponent));
    assert_eq!(check_path("/a/../b"), Err(PathError::DotComponent));
    assert_eq!(check_path("/.."), Err(PathError::DotComponent));
    assert_eq!(check_path("/a/./b"), Err(PathError::DotComponent));
    let long = format!("/{}", "a".repeat(MAX_PATH));
    assert_eq!(check_path(&long), Err(PathError::TooLong));
    assert_eq!(check_path(&long[..MAX_PATH]), Ok(()));
}

/// Catches a binary path that escapes the release or is empty.
#[test]
fn relative_paths() {
    assert_eq!(check_relative("tierstore"), Ok(()));
    assert_eq!(check_relative("bin/tierstore"), Ok(()));
    assert_eq!(check_relative("-x"), Ok(()));
    assert_eq!(check_relative(""), Err(PathError::Empty));
    assert_eq!(check_relative("../x"), Err(PathError::DotComponent));
    assert_eq!(check_relative("a/../x"), Err(PathError::DotComponent));
    assert_eq!(check_relative("/x"), Err(PathError::EmptyComponent));
    assert_eq!(check_relative("x/"), Err(PathError::EmptyComponent));
    assert_eq!(check_relative("a b"), Err(PathError::BadChar(' ')));
}

/// Catches a unit that is not the service's own: a system unit, a unit of another service whose
/// name merely begins the same, a bad instance.
#[test]
fn units_belong_to_the_service() {
    for good in [
        "tierstore.service",
        "tierstore@9301.service",
        "tierstore-extra.service",
        "tierstore-extra@a_b.c-d.service",
    ] {
        assert_eq!(check_unit("tierstore", good), Ok(()), "{good}");
    }
    assert_eq!(
        check_unit("tierstore", "tierstore"),
        Err(UnitError::NotService)
    );
    assert_eq!(
        check_unit("tierstore", "tierstore.socket"),
        Err(UnitError::NotService)
    );
    for foreign in [
        "sshd.service",
        "tierstorex.service",
        "tierstor.service",
        "tierstore-X.service",
        "tierstore_x.service",
        "xtierstore.service",
        ".service",
    ] {
        assert_eq!(
            check_unit("tierstore", foreign),
            Err(UnitError::NotTheServices),
            "{foreign}"
        );
    }
    for bad in [
        "tierstore@.service",
        "tierstore@a b.service",
        "tierstore@a/b.service",
        "tierstore@%i.service",
    ] {
        assert_eq!(
            check_unit("tierstore", bad),
            Err(UnitError::BadInstance),
            "{bad}"
        );
    }
    let long = format!("tierstore@{}.service", "a".repeat(65));
    assert_eq!(check_unit("tierstore", &long), Err(UnitError::BadInstance));
    let longest = format!("tierstore@{}.service", "a".repeat(64));
    assert_eq!(check_unit("tierstore", &longest), Ok(()));
}

/// Catches an environment entry that systemd would split, quote or expand.
#[test]
fn environment_entries() {
    for good in ["A", "_a", "A_1", "Path"] {
        assert!(is_env_name(good), "{good}");
    }
    for bad in ["", "1A", "A-B", "A B", "A=B"] {
        assert!(!is_env_name(bad), "{bad}");
    }
    for good in ["", "x", "a/b:c@d,e+f=g-h_i.j", &"v".repeat(256)] {
        assert!(is_env_value(good), "{good}");
    }
    for bad in [
        "a b",
        "a\"b",
        "$HOME",
        "%h",
        "a\\b",
        "a\nb",
        "é",
        &"v".repeat(257),
    ] {
        assert!(!is_env_value(bad), "{bad}");
    }
}

/// Catches a capability or family name that is not systemd's spelling.
#[test]
fn capability_and_family_names() {
    let cap = Capability::try_from("CAP_NET_ADMIN".to_owned()).unwrap();
    assert_eq!(cap.as_str(), "CAP_NET_ADMIN");
    assert_eq!(cap.to_string(), "CAP_NET_ADMIN");
    assert_eq!(String::from(cap), "CAP_NET_ADMIN");
    let family = Family::try_from("AF_UNIX".to_owned()).unwrap();
    assert_eq!(family.as_str(), "AF_UNIX");
    for bad in [
        "",
        "CAP_",
        "cap_net_admin",
        "NET_ADMIN",
        "CAP_NET ADMIN",
        "~CAP_X",
        "AF_X",
    ] {
        assert!(Capability::try_from(bad.to_owned()).is_err(), "{bad}");
    }
    for bad in ["", "AF_", "af_unix", "UNIX", "AF_UN IX", "CAP_X"] {
        assert!(Family::try_from(bad.to_owned()).is_err(), "{bad}");
    }
    let message = Family::try_from("x".to_owned()).unwrap_err();
    assert!(message.contains("AF_"), "{message}");
}

/// Every refusal says why, in words.
#[test]
fn errors_read_as_sentences() {
    let all = [
        PathError::SystemdPrefix('-').to_string(),
        PathError::NotAbsolute.to_string(),
        PathError::TooLong.to_string(),
        PathError::BadChar('%').to_string(),
        PathError::EmptyComponent.to_string(),
        PathError::DotComponent.to_string(),
        PathError::Empty.to_string(),
        UnitError::NotService.to_string(),
        UnitError::NotTheServices.to_string(),
        UnitError::BadInstance.to_string(),
    ];
    assert!(all.iter().all(|text| !text.is_empty()));
}
