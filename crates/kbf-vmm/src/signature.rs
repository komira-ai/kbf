//! Reading `codesign` output.

use crate::system::Signature;

/// The virtualization entitlement key.
pub const VIRTUALIZATION: &str = "com.apple.security.virtualization";

/// Builds a [`Signature`] from `codesign --display --verbose=4` (stderr text, which holds
/// the code directory flags) and `codesign --display --entitlements - --xml` (the
/// entitlements plist).
#[must_use]
pub fn parse(display: &str, entitlements_xml: &str) -> Signature {
    let flags = display
        .lines()
        .find_map(|l| l.split_once("flags=").map(|(_, rest)| rest))
        .unwrap_or("");
    // `flags=0x10002(adhoc,runtime)`: the names are in the parentheses.
    let names = flags
        .split_once('(')
        .and_then(|(_, rest)| rest.split_once(')'))
        .map_or("", |(inside, _)| inside);
    let has = |n: &str| names.split(',').any(|f| f.trim() == n);
    let key = format!("<key>{VIRTUALIZATION}</key>");
    let enabled = entitlements_xml
        .split_once(&key)
        .is_some_and(|(_, rest)| rest.trim_start().starts_with("<true/>"));
    Signature {
        adhoc: has("adhoc"),
        hardened_runtime: has("runtime"),
        virtualization_entitlement: enabled,
        raw: format!("{}\n{}", display.trim(), entitlements_xml.trim()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::holds;

    const ENT: &str = r#"<?xml version="1.0"?><plist><dict><key>com.apple.security.virtualization</key><true/></dict></plist>"#;

    /// Catches a reader that misses ad hoc, the hardened runtime or the entitlement when
    /// all three are there (the shape `sign-darwin` produces).
    #[test]
    fn the_signed_shape_is_recognized() {
        let s = parse(
            "Executable=/x\nCodeDirectory v=20500 size=1 flags=0x10002(adhoc,runtime) hashes=1\n",
            ENT,
        );
        assert!(
            holds(&[s.adhoc, s.hardened_runtime, s.virtualization_entitlement]),
            "{s:?}"
        );
        assert!(s.raw.contains("flags=0x10002"));
    }

    /// Catches a reader that takes a flag name from anywhere in the text, takes the
    /// entitlement key as enabled when its value is false, or finds it when it is absent.
    #[test]
    fn each_part_is_read_on_its_own() {
        let no_runtime = parse("flags=0x2(adhoc)\n", ENT);
        assert!(holds(&[no_runtime.adhoc, !no_runtime.hardened_runtime]));
        let not_adhoc = parse("flags=0x10000(runtime)\n", ENT);
        assert!(holds(&[!not_adhoc.adhoc, not_adhoc.hardened_runtime]));
        let off = r#"<key>com.apple.security.virtualization</key><false/>"#;
        assert!(!parse("flags=0x10002(adhoc,runtime)", off).virtualization_entitlement);
        assert!(!parse("flags=0x10002(adhoc,runtime)", "<dict/>").virtualization_entitlement);
        let none = parse("no flags here", "");
        assert!(holds(&[
            !none.adhoc,
            !none.hardened_runtime,
            !none.virtualization_entitlement
        ]));
        // The word in the executable path is not a flag.
        let tricky = parse("Executable=/adhoc,runtime\nflags=0x0(none)\n", "");
        assert!(holds(&[!tricky.adhoc, !tricky.hardened_runtime]));
    }
}
