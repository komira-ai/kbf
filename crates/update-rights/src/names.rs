//! The grammars of the declaration: service and account names, paths, units, environment.
//!
//! Every value that reaches a systemd property is checked here first. systemd reads a property
//! value as a list split on white space, with `-`, `+` and `!` in front of a path meaning
//! "ignore a missing path", "ignore the sandbox for this path" and "invert", with `%` for a
//! specifier and `\` for an escape. A grammar that allows none of those leaves nothing for a
//! value to smuggle in.

use std::fmt;

/// The most bytes of a service name.
pub const MAX_SERVICE_NAME: usize = 64;
/// The most bytes of a path.
pub const MAX_PATH: usize = 1024;
/// The most bytes of the value of an environment variable.
pub const MAX_ENV_VALUE: usize = 256;

/// Why a path was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PathError {
    #[error("starts with {0:?}, a systemd prefix")]
    SystemdPrefix(char),
    #[error("is not absolute")]
    NotAbsolute,
    #[error("is longer than {MAX_PATH} bytes")]
    TooLong,
    #[error("holds {0:?}, which is outside the characters a path may use")]
    BadChar(char),
    #[error("has an empty component (a doubled or a trailing slash)")]
    EmptyComponent,
    #[error("has a `.` or `..` component")]
    DotComponent,
    #[error("is empty")]
    Empty,
}

/// Whether `name` is a service name: lowercase letters, digits and `-`, 1 to 64 bytes.
pub fn is_service_name(name: &str) -> bool {
    (1..=MAX_SERVICE_NAME).contains(&name.len())
        & name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Whether `name` is an account name: a lowercase letter or `_` first, then lowercase letters,
/// digits, `_` and `-`, at most 32 bytes.
pub fn is_account_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    let first = bytes
        .next()
        .is_some_and(|b| b.is_ascii_lowercase() || b == b'_');
    first
        & (name.len() <= 32)
        & bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

fn is_path_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | '+' | '@')
}

/// Checks the grammar of an absolute path: it starts with `/` (so with none of the systemd
/// prefixes `-`, `+` and `!`), is at most [`MAX_PATH`] bytes, holds only letters, digits and
/// `/ . _ - + @`, and has no empty, `.` or `..` component. `/` alone is a path.
pub fn check_path(path: &str) -> Result<(), PathError> {
    if let Some(prefix) = path.chars().next().filter(|c| matches!(c, '-' | '+' | '!')) {
        return Err(PathError::SystemdPrefix(prefix));
    }
    if !path.starts_with('/') {
        return Err(PathError::NotAbsolute);
    }
    if path.len() > MAX_PATH {
        return Err(PathError::TooLong);
    }
    if let Some(c) = path.chars().find(|c| !is_path_char(*c)) {
        return Err(PathError::BadChar(c));
    }
    let components = path.strip_prefix('/').unwrap_or_default();
    // `/` alone has one empty component, which is the root and not a mistake.
    if components.is_empty() {
        return Ok(());
    }
    for component in components.split('/') {
        match component {
            "" => return Err(PathError::EmptyComponent),
            "." | ".." => return Err(PathError::DotComponent),
            _ => {}
        }
    }
    Ok(())
}

/// Checks the grammar of a path within a release: the same characters and components as an
/// absolute path, written without the first `/`.
pub fn check_relative(path: &str) -> Result<(), PathError> {
    if path.is_empty() {
        return Err(PathError::Empty);
    }
    check_path(&format!("/{path}"))
}

/// Why a unit name was refused for a service.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UnitError {
    #[error("is not a `.service` unit")]
    NotService,
    #[error("does not begin with the service name, alone or followed by `-`")]
    NotTheServices,
    #[error("has an instance outside `A-Za-z0-9_.-`, 1 to 64 bytes")]
    BadInstance,
}

/// Checks that `unit` is `<service>.service`, `<service>@<instance>.service` or either with
/// `-<more>` after the service name (`web-cache.service` for the service `web`). A unit that
/// does not begin with the service's own name could be a system unit, which a declaration may
/// never claim.
pub fn check_unit(service: &str, unit: &str) -> Result<(), UnitError> {
    let stem = unit.strip_suffix(".service").ok_or(UnitError::NotService)?;
    let (base, instance) = match stem.split_once('@') {
        Some((base, instance)) => (base, Some(instance)),
        None => (stem, None),
    };
    let own = base == service
        || base
            .strip_prefix(service)
            .is_some_and(|rest| rest.starts_with('-') & is_service_name(base));
    if !own {
        return Err(UnitError::NotTheServices);
    }
    let instance_ok = instance.is_none_or(|i| {
        (1..=64).contains(&i.len())
            & i.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
    });
    if !instance_ok {
        return Err(UnitError::BadInstance);
    }
    Ok(())
}

/// Whether `name` can be an environment variable of a hook: `[A-Za-z_][A-Za-z0-9_]*`.
pub fn is_env_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    let first = bytes
        .next()
        .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_');
    first & bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// The variable names that change what a program loads or how it resolves names: the dynamic
/// loader's (`LD_*`), the character set and locale search paths, the allocator's (`MALLOC_*`) and
/// the resolver's.
pub const HIJACKING_ENV: &[&str] = &[
    "GCONV_PATH",
    "LOCPATH",
    "NLSPATH",
    "HOSTALIASES",
    "RES_OPTIONS",
    // What a shell, an interpreter or a runtime started by the hook would read before its script.
    "BASH_ENV",
    "ENV",
    "IFS",
    "CDPATH",
    "SHELLOPTS",
    "BASHOPTS",
    "PS4",
    "PROMPT_COMMAND",
    "PERL5LIB",
    "PERL5OPT",
    "PERLLIB",
    "PERL5DB",
    "RUBYLIB",
    "RUBYOPT",
    "NODE_OPTIONS",
    "NODE_PATH",
    "JAVA_TOOL_OPTIONS",
    "_JAVA_OPTIONS",
    "JDK_JAVA_OPTIONS",
    "CLASSPATH",
    "LUA_PATH",
    "LUA_CPATH",
    "TCLLIBPATH",
    "PHPRC",
    "PHP_INI_SCAN_DIR",
    "GEM_PATH",
    "GEM_HOME",
    // What the C library, TLS libraries, git and the shell read at start.
    "GLIBC_TUNABLES",
    "OPENSSL_CONF",
    "OPENSSL_ENGINES",
    "OPENSSL_MODULES",
    "SSLKEYLOGFILE",
    "ZDOTDIR",
];

/// Service names that are the names of directories of the system under the write roots (`/var/lib/sudo`,
/// `/run/udev`): a service of that name would be handed the directory by the rule that a root hook
/// writes in a directory named for its service.
pub const SYSTEM_NAMES: &[&str] = &[
    "apt",
    "cloud",
    "cloud-init",
    "cron",
    "dbus",
    "dpkg",
    "netplan",
    "network",
    "polkit-1",
    "ssh",
    "sudo",
    "systemd",
    "tmpfiles",
    "udev",
];

/// Whether a hook may be given the variable `name` with `value`: not a name of [`HIJACKING_ENV`],
/// not `LD_*`, `MALLOC_*`, `PYTHON*`, `DYLD_*` or `GIT_*`, and `PATH` only as a list of absolute directories.
pub fn env_allowed(name: &str, value: &str) -> bool {
    let hijack = ["LD_", "MALLOC_", "PYTHON", "DYLD_", "GIT_"]
        .iter()
        .any(|prefix| name.starts_with(prefix))
        | HIJACKING_ENV.contains(&name);
    let path_ok = (name != "PATH") | value.split(':').all(|dir| check_path(dir).is_ok());
    !hijack & path_ok
}

/// Whether `value` can be an environment value: letters, digits and `. _ / : @ , + = -`, at
/// most [`MAX_ENV_VALUE`] bytes. No white space, quote, `$`, `%` or `\`: systemd would read the
/// property as several words or expand it.
pub fn is_env_value(value: &str) -> bool {
    (value.len() <= MAX_ENV_VALUE)
        & value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._/:@,+=-".contains(&b))
}

/// Whether `text` is `prefix` followed by one or more of `A-Z`, `0-9` and `_`.
fn is_upper_name(prefix: &str, text: &str) -> bool {
    text.strip_prefix(prefix).is_some_and(|rest| {
        !rest.is_empty()
            & rest
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
    })
}

macro_rules! upper_name {
    ($(#[$meta:meta])* $name:ident, $prefix:literal) => {
        $(#[$meta])*
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl TryFrom<String> for $name {
            type Error = String;

            fn try_from(text: String) -> Result<$name, String> {
                if is_upper_name($prefix, &text) {
                    Ok($name(text))
                } else {
                    Err(format!(
                        "{text:?} is not {} followed by capital letters, digits and `_`",
                        $prefix
                    ))
                }
            }
        }

        impl From<$name> for String {
            fn from(name: $name) -> String {
                name.0
            }
        }

        impl $name {
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

upper_name!(
    /// A capability name, as systemd spells it (`CAP_NET_ADMIN`).
    Capability,
    "CAP_"
);
upper_name!(
    /// An address family name, as systemd spells it (`AF_UNIX`).
    Family,
    "AF_"
);
