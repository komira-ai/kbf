//! GPU counts from each node class's fixture, and `gpu` requests against them.

use std::path::Path;

use kbf_caps::{
    Consumable, CpuCaps, NodeCaps, ParseError, PciFunction, Request, RequestError, Unmet,
    gpu_cores_from_macos_ioreg, gpus_from_linux_pci, gpus_from_macos_sysctl,
    metal_family_of_apple_chip,
};

/// `(class, vendor)` file text of every PCI function under `fixtures/pci/<node>`, as
/// sysfs lays out `/sys/bus/pci/devices`.
fn pci(node: &str) -> Vec<(String, String)> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/pci")
        .join(node);
    let mut functions: Vec<(String, String)> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .map(|entry| {
            let path = entry.unwrap().path();
            let read = |file: &str| std::fs::read_to_string(path.join(file)).unwrap();
            (read("class"), read("vendor"))
        })
        .collect();
    functions.sort();
    functions
}

fn gpus(functions: &[(String, String)]) -> Result<u64, ParseError> {
    gpus_from_linux_pci(
        functions
            .iter()
            .map(|(class, vendor)| PciFunction { class, vendor }),
    )
}

fn count(node: &str) -> u64 {
    gpus(&pci(node)).unwrap()
}

/// Catches: a probe that counts by class alone (the management controller's VGA
/// function would make every server a GPU node), one that counts a 3D controller as
/// nothing (data-center GPUs are 3D controllers, not VGA), and one that counts the
/// NIC.
#[test]
fn a_gpu_server_counts_its_gpus_and_not_its_management_vga() {
    assert_eq!(count("gpu_server_4x"), 4);
    assert_eq!(count("cpu_server"), 0);
}

/// Catches: a probe that counts by vendor alone (an AMD card's HDMI audio function
/// would count as a second GPU), and one that counts integrated graphics.
#[test]
fn a_workstation_counts_its_card_once() {
    assert_eq!(count("workstation_amd"), 1);
    assert_eq!(gpus(&[]), Ok(0));
}

/// Catches: a value read loosely (no `0x`, the wrong width, a non-hex digit, text
/// after the newline) and taken as some class, instead of named.
#[test]
fn malformed_pci_values_are_named() {
    let vga = "0x030000\n";
    let nvidia = "0x10de\n";
    assert_eq!(
        gpus_from_linux_pci([PciFunction {
            class: "0x030000",
            vendor: "0x10de"
        }]),
        Ok(1),
        "the newline is optional"
    );
    for (class, vendor, bad) in [
        ("030000\n", nvidia, "030000\n"),
        ("0x0300\n", nvidia, "0x0300\n"),
        ("0x03000g\n", nvidia, "0x03000g\n"),
        (vga, "0x10de0\n", "0x10de0\n"),
        (vga, "0x10de\n\n", "0x10de\n\n"),
        (vga, "0x+0de", "0x+0de"),
    ] {
        assert_eq!(
            gpus_from_linux_pci([PciFunction { class, vendor }]),
            Err(ParseError::PciValue(bad.to_owned())),
            "{class:?} {vendor:?}"
        );
    }
}

/// Catches: a Mac reported without its GPU (it would never take GPU work), and a
/// non-Apple-silicon capture counted as one.
#[test]
fn an_apple_silicon_mac_has_one_gpu() {
    let sysctl = include_str!("fixtures/apple_m3_ultra.sysctl");
    assert_eq!(gpus_from_macos_sysctl(sysctl), Ok(1));
    assert_eq!(
        gpus_from_macos_sysctl("hw.optional.avx2_0: 1\n"),
        Err(ParseError::NotArm64Mac)
    );
}

/// Catches: a Mac's GPU core count read from the wrong line (the `model` or
/// `IOClass` line, the first number in the text), a count that is not a whole number
/// or is zero taken as the cores, and text with no such line (a guest's paravirtual GPU,
/// a Mac whose `ioreg` printed nothing) given a count instead of none.
#[test]
fn a_macs_gpu_core_count_is_the_registry_line() {
    let ioreg = include_str!("fixtures/apple_m3_ultra.ioreg");
    assert_eq!(gpu_cores_from_macos_ioreg(ioreg), Some(80));
    for text in [
        "",
        "+-o AGXAcceleratorG15X\n  \"model\" = 80\n",
        "\"gpu-core-count\" = many\n",
        "\"gpu-core-count\" = 0\n",
        "\"gpu-core-count\"\n",
        "\"gpu-core-count-extra\" = 7\n",
    ] {
        assert_eq!(gpu_cores_from_macos_ioreg(text), None, "{text:?}");
    }
    let two = "\"gpu-core-count\" = 10\n\"gpu-core-count\" = 40\n";
    assert_eq!(gpu_cores_from_macos_ioreg(two), Some(10), "the first");
}

/// Catches: a Metal family taken from the wrong chip generation (M3 and M4 are both
/// family 9, so a table keyed on the digit alone gets one wrong), the tier suffix
/// (`Pro`, `Ultra`) changing the answer, and a name that is not an Apple chip, or a
/// generation not in the table, given a family: the node reports none, never a guess.
#[test]
fn a_chips_metal_family_follows_its_generation() {
    for (brand, family) in [
        ("Apple M1", 7),
        ("Apple M1 Max", 7),
        ("Apple M2 Pro", 8),
        ("Apple M3 Ultra", 9),
        ("Apple M4", 9),
        ("Apple M5 Max", 10),
    ] {
        assert_eq!(metal_family_of_apple_chip(brand), Some(family), "{brand}");
    }
    for brand in [
        "",
        "Apple M",
        "Apple M0",
        "Apple M6",
        "Apple M12",
        "Intel(R) Xeon(R)",
        "Apple A17 Pro",
        "Apple Mx",
    ] {
        assert_eq!(metal_family_of_apple_chip(brand), None, "{brand:?}");
    }
}

fn node(gpus: u64) -> NodeCaps {
    let cpu = include_str!("fixtures/skylake_sp_platinum_8180.cpuinfo");
    let mut n = NodeCaps::new(CpuCaps::from_linux_cpuinfo(cpu).unwrap());
    n.consumables.insert(Consumable::Gpus, gpus);
    n
}

/// Catches: `gpu` still compared as an exact string (a `gpu=1` request would miss a
/// four-GPU node), compared backwards, or a node without a GPU entry taken as having
/// some.
#[test]
fn gpu_is_a_count() {
    let one = Request::parse([("gpu", "1")]).unwrap();
    assert!(one.matches(&node(1)));
    assert!(one.matches(&node(count("gpu_server_4x"))));
    assert_eq!(
        one.unmet(&node(0)),
        [Unmet::Consumable {
            what: Consumable::Gpus,
            want: 1,
            have: 0
        }]
    );
    let bare = NodeCaps::new(node(0).cpu);
    assert!(!one.matches(&bare), "a missing GPU entry counted");
    assert!(!Request::parse([("gpu", "2")]).unwrap().matches(&node(1)));
    assert_eq!(Consumable::Gpus.to_string(), "gpu");
}

/// Catches: a `gpu` value that is not a count accepted (as an exact string it would
/// match nothing and queue forever), and a repeated `gpu` key accepted.
#[test]
fn gpu_takes_one_count() {
    let err = |props: &[(&str, &str)]| Request::parse(props.iter().copied()).unwrap_err();
    for v in ["yes", "-1", "nvidia"] {
        assert_eq!(
            err(&[("gpu", v)]),
            RequestError::BadValue {
                key: "gpu".to_owned(),
                value: v.to_owned()
            }
        );
    }
    assert_eq!(
        err(&[("gpu", "1"), ("gpu", "1")]),
        RequestError::Repeated("gpu".to_owned())
    );
}

/// Catches: `gpu.kind` unknown to the matcher (a request for it refused, or ignored so a
/// Linux node with a GPU serves a request for a native one), matched as a minimum, and a
/// node that does not report it (a Linux GPU node, a guest) taken for a native one. A Mac
/// reports `gpu.cores` and `gpu.metal_family` too; the report is read without them
/// failing, and a request cannot name them yet.
#[test]
fn gpu_kind_is_an_exact_key_only_a_native_gpu_reports() {
    let read = |extra: &[(&'static str, &'static str)]| {
        let mut entries = vec![("arch", "arm64"), ("os", "macos"), ("gpu", "1")];
        entries.extend_from_slice(extra);
        NodeCaps::from_report(entries).expect("a report")
    };
    let mac = read(&[
        ("gpu.kind", "native"),
        ("gpu.cores", "80"),
        ("gpu.metal_family", "9"),
    ]);
    assert_eq!(
        mac.exact.get("gpu.kind").map(String::as_str),
        Some("native")
    );
    let native = Request::parse([("gpu.kind", "native")]).expect("a known key");
    assert!(native.matches(&mac));
    assert!(!native.matches(&read(&[])), "no gpu.kind reported");
    assert!(!native.matches(&read(&[("gpu.kind", "virtual")])));
    assert_eq!(
        native
            .unmet(&read(&[]))
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        ["gpu.kind=native"]
    );
    for key in ["gpu.cores", "gpu.metal_family"] {
        assert_eq!(
            Request::parse([(key, "1")]),
            Err(RequestError::UnknownKey(key.to_owned()))
        );
    }
    let twice = [
        ("arch", "arm64"),
        ("gpu.kind", "native"),
        ("gpu.kind", "native"),
    ];
    assert!(NodeCaps::from_report(twice).is_err());
}
