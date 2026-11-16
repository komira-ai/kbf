//! Whether an action may use the network, and what the driver does about it.
//!
//! An action has the network unless it turns it off with the platform property
//! `network`: absent, `on` or `standard` gives it the network (the internet and the
//! LAN); `off` or `none` keeps it off (loopback still works). Anything else is the
//! client's error.
//!
//! The driver enforces nothing: the action runs with the node's network whatever it
//! asked, and the node reports that as the capability `network_isolation` with the
//! value [`ISOLATION`] (`none`), so nothing about it is hidden. The request is still
//! read and checked ([`network_of`]), so a value the platform does not define is refused.

use kbf_daemon::RuntimeError;
use kbf_proto::reapi::{Action, Command, Platform};

/// The platform property an action asks for the network with.
pub const PROPERTY: &str = "network";

/// The node report key that says how the network is kept off.
pub const CAPABILITY: &str = "network_isolation";

/// The value of the [`CAPABILITY`] entry: the network is not kept off at all, so an
/// action that asks for no network still has it.
pub const ISOLATION: &str = "none";

/// What an action asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Network {
    Off,
    On,
}

/// The action's `network` property, from the Action's platform or, for clients older
/// than REAPI 2.2, the Command's.
///
/// # Errors
/// [`RuntimeError::Invalid`] for a value other than those the module lists.
#[allow(deprecated)]
pub fn network_of(action: &Action, command: &Command) -> Result<Network, RuntimeError> {
    let platform: Option<&Platform> = action.platform.as_ref().or(command.platform.as_ref());
    let value = platform
        .and_then(|p| p.properties.iter().find(|p| p.name == PROPERTY))
        .map(|p| p.value.as_str());
    match value {
        Some("off" | "none") => Ok(Network::Off),
        None | Some("on" | "standard") => Ok(Network::On),
        Some(other) => Err(RuntimeError::Invalid(format!(
            "platform property {PROPERTY}={other:?}: use on, standard, off or none"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use kbf_proto::reapi::platform::Property;

    use super::*;

    fn platform(value: &str) -> Option<Platform> {
        Some(Platform {
            properties: vec![Property {
                name: PROPERTY.to_owned(),
                value: value.to_owned(),
            }],
        })
    }

    /// Catches: the network off by default (the old rule), an unknown value taken as
    /// on or off instead of refused, the Command's deprecated platform ignored, and
    /// the Command's platform overriding the Action's.
    #[test]
    #[allow(deprecated)]
    fn the_network_is_on_unless_turned_off() {
        let none = (Action::default(), Command::default());
        assert_eq!(network_of(&none.0, &none.1).ok(), Some(Network::On));
        for (value, want) in [
            ("off", Network::Off),
            ("none", Network::Off),
            ("on", Network::On),
            ("standard", Network::On),
        ] {
            let action = Action {
                platform: platform(value),
                ..Action::default()
            };
            assert_eq!(network_of(&action, &none.1).ok(), Some(want), "{value}");
        }
        let bad = Action {
            platform: platform("yes"),
            ..Action::default()
        };
        assert!(matches!(
            network_of(&bad, &none.1),
            Err(RuntimeError::Invalid(_))
        ));
        // The Command's platform turns the network off (`off`, not `on`: `on` is also
        // the default, so a check with it passes when the Command is ignored).
        let command = Command {
            platform: platform("off"),
            ..Command::default()
        };
        assert_eq!(
            network_of(&Action::default(), &command).ok(),
            Some(Network::Off)
        );
        let action = Action {
            platform: platform("on"),
            ..Action::default()
        };
        assert_eq!(network_of(&action, &command).ok(), Some(Network::On));
    }
}
