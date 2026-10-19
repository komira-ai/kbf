//! Platform aliases: a client property that stands for the OS and architecture it was
//! always meant to name.
//!
//! komira's farm config sends `pool=darwin-sized` and `pool=mojo-sized` and nothing
//! else. kbf reads no `pool` property, so without an alias those actions ask for no
//! OS and no architecture and any worker may take them. Platform properties are part
//! of the action digest, so the fix cannot be on the client without cold-starting the
//! action cache. An alias (`--platform-alias pool=darwin-sized:OSFamily=Darwin,ISA=arm64`)
//! is applied by the frontend to what placement reads and to nothing else: the action
//! and its digest are the client's bytes.
//!
//! An alias is configured by the operator and is never inferred from a node's
//! `--label`: routing must not depend on what someone typed on each node.

use std::fmt;
use std::str::FromStr;

use kbf_caps::{FromPlatformError, Request, property_name};
use kbf_types::Platform;

/// Why a `--platform-alias` value is not an alias.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AliasError {
    /// The value is not `NAME=VALUE:KEY=VALUE[,KEY=VALUE...]`.
    #[error(
        "{0:?} is not NAME=VALUE:KEY=VALUE[,KEY=VALUE...] (for example \
         pool=darwin-sized:OSFamily=Darwin,ISA=arm64): each of NAME, VALUE, KEY and VALUE \
         is non-empty without whitespace, ',' or ':', and there is exactly one ':'"
    )]
    Shape(String),
    /// The aliased name is a property kbf reads itself.
    #[error("{0:?} is a property kbf reads; an alias must name one it does not (such as pool)")]
    ReadName(String),
    /// An expansion key is not a property kbf reads, so it would constrain nothing.
    #[error("{key:?} in the expansion of {alias} is not a property kbf reads")]
    UnreadKey { alias: String, key: String },
    /// An expansion asks for one requirement twice (also in two spellings).
    #[error("the expansion of {alias} names {key:?} twice (also as OSFamily/os, ISA/Arch)")]
    Twice { alias: String, key: String },
    /// The expansion is not a platform some kbf daemon can run.
    #[error("the expansion of {alias} is not servable: {why}")]
    Unservable { alias: String, why: String },
    /// Two aliases for one name and value.
    #[error("{0} is aliased twice")]
    Duplicate(String),
}

/// One alias: a client property, and the properties placement reads in its place.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlatformAlias {
    name: String,
    value: String,
    /// Canonical property names (`kbf_caps::property_name`) and their values.
    expansion: Vec<(String, String)>,
}

/// A part of the alias text: non-empty, no whitespace, `,` or `:`.
fn token(s: &str) -> bool {
    !s.is_empty() && !s.contains(|c: char| c.is_whitespace() || c == ',' || c == ':')
}

fn pair(s: &str) -> Option<(&str, &str)> {
    s.split_once('=')
        .filter(|(k, v)| token(k) && token(v) && !v.contains('='))
}

/// The requirement a property name sets: `OSFamily` and `os` set one, `ISA`, `Arch` and
/// `arch` another.
fn slot(canonical: &str) -> &str {
    match canonical {
        "OSFamily" => "os",
        "ISA" | "Arch" => "arch",
        other => other,
    }
}

impl FromStr for PlatformAlias {
    type Err = AliasError;

    fn from_str(text: &str) -> Result<Self, AliasError> {
        let shape = || AliasError::Shape(text.to_owned());
        let (from, to) = text.split_once(':').ok_or_else(shape)?;
        let (name, value) = pair(from).ok_or_else(shape)?;
        let id = format!("{name}={value}");
        if property_name(name).is_some() {
            return Err(AliasError::ReadName(name.to_owned()));
        }
        let mut expansion: Vec<(String, String)> = Vec::new();
        for part in to.split(',') {
            let (key, v) = pair(part).ok_or_else(shape)?;
            let canonical = property_name(key).ok_or_else(|| AliasError::UnreadKey {
                alias: id.clone(),
                key: key.to_owned(),
            })?;
            if expansion
                .iter()
                .any(|(have, _)| slot(have) == slot(&canonical))
            {
                return Err(AliasError::Twice {
                    alias: id,
                    key: key.to_owned(),
                });
            }
            expansion.push((canonical.into_owned(), v.to_owned()));
        }
        Request::from_platform(expansion.iter().map(|(k, v)| (k.as_str(), v.as_str()))).map_err(
            |e| AliasError::Unservable {
                alias: id.clone(),
                why: match e {
                    FromPlatformError::Invalid(e) => e.to_string(),
                    FromPlatformError::NeverServed(why) => why,
                },
            },
        )?;
        Ok(Self {
            name: name.to_owned(),
            value: value.to_owned(),
            expansion,
        })
    }
}

impl fmt::Display for PlatformAlias {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}={} -> ", self.name, self.value)?;
        for (i, (k, v)) in self.expansion.iter().enumerate() {
            write!(f, "{}{k}={v}", if i == 0 { "" } else { "," })?;
        }
        Ok(())
    }
}

impl PlatformAlias {
    /// Whether the request's property `name`=`value` is this alias: the name in any
    /// case (as every property name is read), the value exactly.
    fn is(&self, name: &str, value: &str) -> bool {
        self.name.eq_ignore_ascii_case(name) && self.value == value
    }
}

/// The aliases a frontend expands, none by default.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PlatformAliases(Vec<PlatformAlias>);

/// A request whose alias asks for something the request also says differently.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "platform alias {alias} expands to {key}={value}, which conflicts with the platform \
     property {other}={other_value} the request also sent"
)]
pub struct Conflict {
    alias: String,
    key: String,
    value: String,
    other: String,
    other_value: String,
}

/// Whether two properties of one requirement ask for the same thing, in whatever
/// spelling: `OSFamily=Darwin` and `os=macos` do, `ISA=arm64` and `Arch=aarch64` do.
fn says_the_same(a: (&str, &str), b: (&str, &str)) -> bool {
    match (Request::from_platform([a]), Request::from_platform([b])) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

impl PlatformAliases {
    /// The aliases `list`.
    ///
    /// # Errors
    /// [`AliasError::Duplicate`] for a name and value aliased twice.
    pub fn new(list: Vec<PlatformAlias>) -> Result<Self, AliasError> {
        for (i, a) in list.iter().enumerate() {
            if list[..i].iter().any(|b| b.is(&a.name, &a.value)) {
                return Err(AliasError::Duplicate(format!("{}={}", a.name, a.value)));
            }
        }
        Ok(Self(list))
    }

    /// Whether no alias is configured.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The configured aliases, one per line, as they are logged at start.
    pub fn describe(&self) -> impl Iterator<Item = String> + '_ {
        self.0.iter().map(ToString::to_string)
    }

    /// `sent` with each alias property replaced by its expansion: the platform
    /// placement reads. The alias property itself is consumed. A property of the
    /// request that sets the same requirement as an expansion entry is a [`Conflict`]
    /// unless it asks for the same thing (`OSFamily=Darwin` and `os=macos` do), which stands.
    ///
    /// # Errors
    /// [`Conflict`] as above.
    pub fn expand(&self, sent: Platform) -> Result<Platform, Conflict> {
        if self.0.is_empty() {
            return Ok(sent);
        }
        let mut out = Platform::new();
        let mut aliases = Vec::new();
        for (name, value) in sent.canonical() {
            match self.0.iter().find(|a| a.is(name, value)) {
                Some(alias) => aliases.push(alias),
                // A name of `sent` is unique and `out` takes a subset of them.
                None => {
                    let _ = out.insert(name, value);
                }
            }
        }
        for alias in aliases {
            for (key, value) in &alias.expansion {
                let same_slot = out
                    .canonical()
                    .find(|(other, _)| property_name(other).is_some_and(|c| slot(&c) == slot(key)));
                match same_slot {
                    None => {
                        let _ = out.insert(key.as_str(), value.as_str());
                    }
                    Some((other, other_value))
                        if says_the_same((key, value), (other, other_value)) => {}
                    Some((other, other_value)) => {
                        return Err(Conflict {
                            alias: format!("{}={}", alias.name, alias.value),
                            key: key.clone(),
                            value: value.clone(),
                            other: other.to_owned(),
                            other_value: other_value.to_owned(),
                        });
                    }
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alias(text: &str) -> Result<PlatformAlias, AliasError> {
        text.parse()
    }

    /// Catches an alias shape accepted that cannot do its job, and one refused that can.
    #[test]
    fn the_shape_is_name_value_colon_pairs() {
        let good = alias("pool=darwin-sized:OSFamily=Darwin,ISA=arm64").unwrap();
        assert_eq!(
            good.expansion,
            [
                ("OSFamily".into(), "Darwin".into()),
                ("ISA".into(), "arm64".into())
            ]
        );
        // A name in any case is the same name; kbf's own spelling of a key is accepted.
        alias("Pool=x:os=macos,arch=arm64").unwrap();
        for bad in [
            "",
            "pool",
            "pool=x",
            "pool:OSFamily=Linux",
            "pool=x:",
            ":OSFamily=Linux",
            "=x:OSFamily=Linux",
            "pool=:OSFamily=Linux",
            "pool=x:OSFamily=Linux:ISA=arm64",
            "pool=x:OSFamily=Linux,",
            "pool=x:,OSFamily=Linux",
            "pool=x:OSFamily",
            "pool=x:OSFamily=",
            "pool=x y:OSFamily=Linux",
            "pool=a=b:OSFamily=Linux",
        ] {
            assert!(matches!(alias(bad), Err(AliasError::Shape(_))), "{bad:?}");
        }
    }

    /// Catches an alias that could never take effect being accepted at start: it names
    /// a property kbf reads (the client's `OSFamily` would be eaten), expands to one
    /// kbf does not read, sets a requirement twice, or names an OS no daemon runs.
    #[test]
    fn an_alias_that_cannot_work_is_refused() {
        assert!(matches!(
            alias("OSFamily=Linux:ISA=arm64"),
            Err(AliasError::ReadName(_))
        ));
        assert!(matches!(
            alias("label.pool=x:ISA=arm64"),
            Err(AliasError::ReadName(_))
        ));
        assert!(matches!(
            alias("pool=x:flavour=y"),
            Err(AliasError::UnreadKey { .. })
        ));
        for twice in [
            "OSFamily=Linux,os=linux",
            "ISA=arm64,Arch=arm64",
            "ISA=arm64,arch=arm64",
        ] {
            assert!(
                matches!(
                    alias(&format!("pool=x:{twice}")),
                    Err(AliasError::Twice { .. })
                ),
                "{twice}"
            );
        }
        assert!(matches!(
            alias("pool=x:OSFamily=Windows"),
            Err(AliasError::Unservable { .. })
        ));
        assert!(matches!(
            alias("pool=x:ISA=x86-32"),
            Err(AliasError::Unservable { .. })
        ));
    }

    /// Catches two aliases for one name and value (which one wins would depend on the
    /// order of the flags).
    #[test]
    fn one_name_and_value_is_aliased_once() {
        let a = alias("pool=x:OSFamily=Linux").unwrap();
        let b = alias("POOL=x:OSFamily=Darwin").unwrap();
        assert!(matches!(
            PlatformAliases::new(vec![a.clone(), b]),
            Err(AliasError::Duplicate(_))
        ));
        let other = alias("pool=y:OSFamily=Darwin").unwrap();
        PlatformAliases::new(vec![a, other]).unwrap();
    }
}
