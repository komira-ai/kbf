//! iOS device requests and report entries (`docs/design/ios-devices.md`, sections 5.2
//! and 5.3): one device must satisfy every `ios.device.*` key of a request.

use kbf_caps::{FromPlatformError, NodeCaps, ReportError, Request, RequestError, property_name};

fn from(props: &[(&str, &str)]) -> Result<Request, FromPlatformError> {
    Request::from_platform(props.iter().copied())
}

fn req(props: &[(&str, &str)]) -> Request {
    from(props).unwrap()
}

/// A Mac reporting `devices`, each an `ios.device` entry's value.
fn mac(devices: &[&str]) -> NodeCaps {
    let entries = [("arch", "arm64"), ("os", "macos")]
        .into_iter()
        .chain(devices.iter().map(|d| ("ios.device", *d)));
    NodeCaps::from_report(entries).unwrap()
}

const OLD_PHONE: &str =
    "class=iPhone,id=dev-a,os_build=22G86,os_version=18.6,product_type=iPhone17,3";
const NEW_PAD: &str = "class=iPad,id=dev-b,os_build=23A341,os_version=26.0,product_type=iPad16,3";
const NEW_PHONE: &str =
    "class=iPhone,id=dev-c,os_build=23A341,os_version=26.0,product_type=iPhone17,3";

/// Catches: device attributes matched across the node's devices instead of within one.
/// Device a is an iPhone17,3 on iOS 18.6 and device b an iPad on iOS 26.0, so a request
/// for an iPhone17,3 on iOS 26.0 has each key met by some device but by no single one:
/// a per-key check would match the node and book a device that is wrong on one key.
/// Also catches a match that ignores the attributes, and a closest-device reason that
/// names the wrong device or attribute.
#[test]
fn one_device_must_satisfy_every_attribute() {
    let want = req(&[
        ("ios.device", "1"),
        ("ios.device.os_version", "26.0"),
        ("ios.device.product_type", "iPhone17,3"),
    ]);
    let split = mac(&[OLD_PHONE, NEW_PAD]);
    assert!(!want.matches(&split));
    assert!(want.devices(&split).is_empty());
    let lacks: Vec<String> = want.unmet(&split).iter().map(ToString::to_string).collect();
    // Both devices lack one attribute; the first by id is the closest.
    assert_eq!(
        lacks,
        ["ios.device.os_version=26.0 (device dev-a has 18.6)"]
    );

    let with = mac(&[OLD_PHONE, NEW_PAD, NEW_PHONE]);
    assert!(want.matches(&with));
    assert_eq!(want.devices(&with), ["dev-c"]);

    // No attribute: every device serves.
    assert_eq!(
        req(&[("ios.device", "1")]).devices(&with),
        ["dev-a", "dev-b", "dev-c"]
    );
}

/// Catches: a device request matched by a node that reports no device. A daemon that
/// predates devices reports no `ios.device` entry and would drop `Start.device_id`, so
/// it must never match; and a request asking for no device must not be refused a
/// device-less node.
#[test]
fn a_node_without_devices_matches_no_device_request() {
    let bare = mac(&[]);
    let want = req(&[("OSFamily", "Darwin"), ("ios.device", "1")]);
    assert!(!want.matches(&bare));
    let lacks: Vec<String> = want.unmet(&bare).iter().map(ToString::to_string).collect();
    assert_eq!(lacks, ["ios.device=1"]);
    assert!(want.wants_device());
    let plain = req(&[("OSFamily", "Darwin")]);
    assert!(!plain.wants_device());
    assert!(plain.matches(&bare));
    assert!(plain.devices(&mac(&[NEW_PHONE])).is_empty());
}

/// Catches: an attribute that is missing from the device's entry read as matching.
#[test]
fn a_device_without_the_attribute_does_not_match() {
    let node = mac(&["id=dev-z,class=iPhone"]);
    let want = req(&[("ios.device", "1"), ("ios.device.os_build", "23A341")]);
    assert!(!want.matches(&node));
    let lacks: Vec<String> = want.unmet(&node).iter().map(ToString::to_string).collect();
    assert_eq!(
        lacks,
        ["ios.device.os_build=23A341 (device dev-z has none)"]
    );
}

/// Catches: the device keys read only in one spelling (a `IOS.Device` sent by a client
/// would be ignored and the action run with no device), and a device key no longer
/// one of kbf's own (so `from_platform` drops it).
#[test]
fn device_keys_are_read_in_any_case() {
    for (sent, key) in [
        ("ios.device", "ios.device"),
        ("IOS.Device", "ios.device"),
        ("iOS.Device.OS_Version", "ios.device.os_version"),
        ("ios.device.os", "ios.device.os"),
    ] {
        assert_eq!(property_name(sent).as_deref(), Some(key), "{sent}");
    }
    let want = req(&[("IOS.Device", "1"), ("Ios.Device.Class", "iPad")]);
    assert_eq!(want.devices(&mac(&[NEW_PHONE, NEW_PAD])), ["dev-b"]);
}

/// Catches each refusal the design names being let through: `ios.device` with a value
/// other than `1` (a count would read as one device), an attribute without
/// `ios.device=1` (which would match every node), an attribute kbf does not know
/// (ignored, it would match any device), an empty attribute value, a repeated key, and
/// a device on a non-Mac platform.
#[test]
fn malformed_device_requests_are_refused() {
    let bad = |key: &str, value: &str| RequestError::BadValue {
        key: key.to_owned(),
        value: value.to_owned(),
    };
    for (props, err) in [
        (vec![("ios.device", "2")], bad("ios.device", "2")),
        (vec![("ios.device", "true")], bad("ios.device", "true")),
        (
            vec![("ios.device.class", "iPhone")],
            RequestError::DeviceAttributeAlone("ios.device.class".to_owned()),
        ),
        (
            vec![("ios.device", "1"), ("ios.device.os", "26.0")],
            RequestError::UnknownDeviceAttribute("ios.device.os".to_owned()),
        ),
        (
            vec![("ios.device", "1"), ("ios.device.", "x")],
            RequestError::UnknownDeviceAttribute("ios.device.".to_owned()),
        ),
        (
            vec![("ios.device", "1"), ("ios.device.class", "")],
            bad("ios.device.class", ""),
        ),
        (
            vec![("ios.device", "1"), ("os", "linux")],
            RequestError::DeviceNotOnMacos("linux".to_owned()),
        ),
    ] {
        assert_eq!(Request::parse(props.iter().copied()), Err(err), "{props:?}");
    }
    assert_eq!(
        from(&[("OSFamily", "linux"), ("ios.device", "1")]),
        Err(FromPlatformError::Invalid(RequestError::DeviceNotOnMacos(
            "linux".to_owned()
        )))
    );
    assert_eq!(
        Request::parse([("ios.device", "1"), ("ios.device", "1")]),
        Err(RequestError::Repeated("ios.device".to_owned()))
    );
    assert_eq!(
        Request::parse([
            ("ios.device", "1"),
            ("ios.device.class", "iPhone"),
            ("ios.device.class", "iPad"),
        ]),
        Err(RequestError::Repeated("ios.device.class".to_owned()))
    );
    // The attribute may come before `ios.device=1`.
    assert!(Request::parse([("ios.device.class", "iPhone"), ("ios.device", "1")]).is_ok());
    assert!(Request::parse([("ios.device", "1"), ("os", "macos")]).is_ok());
    let unknown = Request::parse([("ios.device", "1"), ("ios.device.os", "26.0")]).unwrap_err();
    assert!(
        unknown.to_string().contains("ios.device.os_version"),
        "{unknown}"
    );
}

/// Catches: a report's devices dropped or mixed up (the attributes of one device filed
/// under another), a model identifier split at its comma (`iPhone17,3` read as
/// `iPhone17` and a field `3`, so no request for it would ever match), and fields kbf
/// does not match on refused, which would stop a newer daemon registering.
#[test]
fn a_report_lists_each_device_with_its_attributes() {
    let node = mac(&[NEW_PHONE, "battery=80,id=dev-d,class=iPad"]);
    let ids: Vec<&str> = node.devices.keys().map(String::as_str).collect();
    assert_eq!(ids, ["dev-c", "dev-d"]);
    assert_eq!(node.devices["dev-c"]["os_version"], "26.0");
    // A model identifier holds a comma: it is one value, not a field `3`.
    assert_eq!(node.devices["dev-c"]["product_type"], "iPhone17,3");
    assert_eq!(node.devices["dev-c"].len(), 4);
    assert_eq!(node.devices["dev-c"].get("id"), None);
    assert_eq!(node.devices["dev-d"]["class"], "iPad");
    assert_eq!(node.devices["dev-d"]["battery"], "80");
}

/// Catches: a malformed `ios.device` entry read as some other device (a missing id, an
/// empty field, a piece with no field before it, a value holding `=`, a field given
/// twice), and two entries for one id merged or the second one silently kept.
#[test]
fn malformed_device_entries_are_refused() {
    for value in [
        "class=iPhone",
        "id=",
        "id=a,class=",
        "17,3,id=a",
        "id=a,=x",
        "id=a=b",
        "id=a,class=iPhone,class=iPad",
        "id=a,id=b",
        "",
    ] {
        assert_eq!(
            NodeCaps::from_report([("arch", "arm64"), ("ios.device", value)]),
            Err(ReportError::BadDevice(value.to_owned())),
            "{value:?}"
        );
    }
    assert_eq!(
        NodeCaps::from_report([
            ("arch", "arm64"),
            ("ios.device", "id=a,class=iPhone"),
            ("ios.device", "id=a,class=iPad"),
        ]),
        Err(ReportError::RepeatedDevice("a".to_owned()))
    );
}

/// Catches: the device keys matched on too short a prefix (`ios.device` with no `.`
/// after it, `ios.`, `ios`), so a property that only begins like a device key is read
/// instead of left alone, as every property kbf does not read is.
#[test]
fn names_that_only_begin_like_a_device_key_are_left_alone() {
    for sent in [
        "ios.devices",
        "ios.deviceclass",
        "ios.simulator",
        "iosx",
        "ios",
    ] {
        assert_eq!(property_name(sent), None, "{sent}");
        assert_eq!(from(&[(sent, "1")]), Ok(Request::default()), "{sent}");
    }
}
