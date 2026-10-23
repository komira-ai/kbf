//! What an activation order may name, against what the release store calls a platform's
//! components (`kbf_types::ReleasePlatform::components`): the store builds an order's
//! component list from them, so an order for a platform the store calls complete must pass
//! the installer's own check of the names.

use kbf_types::ReleasePlatform;

use super::*;

/// Catches a platform whose store components the installer refuses: for each platform, a host
/// of it is ordered to activate a release whose components are exactly the ones the store needs
/// for that platform. The name check must let it through to the fetch (the fake host's
/// platform report decides the rest, which is not what this holds). The Mac's store component
/// was `kbf-daemon`, which the check refused ("this platform needs exactly [kbf]"), so no
/// Mac could take a roll.
///
/// Catches: `DarwinArm64.components()` as `["kbf-daemon"]` (the Mac's order is refused
/// before the fetch).
#[test]
fn an_order_naming_the_components_the_store_needs_passes_the_name_check() {
    for platform in ReleasePlatform::ALL {
        let host = if platform == ReleasePlatform::DarwinArm64 {
            Platform::MacOs
        } else {
            Platform::Linux
        };
        let mut rig = Rig::on(&format!("store-components-{platform}"), host);
        let mut r = rig.rec(1, 1, 1, Verb::Activate, B);
        let template = r.components[0].clone();
        r.components = platform
            .components()
            .iter()
            .map(|name| Locator {
                name: (*name).to_owned(),
                ..template.clone()
            })
            .collect();
        r.expected
            .as_mut()
            .expect("an activation has facts")
            .platform = platform.as_str().to_owned();
        rig.put(&r);
        rig.cycle(T0);
        assert!(
            !rig.refusal().contains("names components"),
            "{platform}: {}",
            rig.refusal()
        );
        assert_eq!(rig.fetcher.count(), 1, "{platform}: {}", rig.refusal());
    }
}
