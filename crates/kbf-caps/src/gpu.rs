//! A node's GPU count: whole GPUs, each booked by one lease at a time.
//!
//! - Linux: the PCI functions the kernel lists under `/sys/bus/pci/devices`, each read
//!   as the text of its `class` and `vendor` files. A function counts as one GPU when
//!   it is a VGA-compatible or 3D controller (class `0x0300xx` or `0x0302xx`) made by
//!   NVIDIA (`0x10de`) or AMD (`0x1002`). Other display functions do not count: a
//!   server's management controller exposes a VGA function, and integrated graphics
//!   is not a device to hand to an action.
//! - macOS: an Apple silicon Mac has one GPU, shared by the whole host. The node books
//!   it as one slot (`gpu` = 1), so one GPU lease runs on a Mac at a time. A Mac also
//!   reports what kind of GPU that is: [`GPU_KIND_NATIVE`] (the daemon runs on the
//!   hardware, not in a guest), its core count ([`gpu_cores_from_macos_ioreg`], read from
//!   the IOKit registry, which `sysctl` has no entry for) and its Metal GPU family
//!   ([`metal_family_of_apple_chip`], from the chip's name).

use crate::cpu::ParseError;
use crate::macos;

/// PCI vendor ids whose display functions count as GPUs: NVIDIA and AMD.
const GPU_VENDORS: [u32; 2] = [0x10de, 0x1002];

/// PCI base class and subclass of a GPU: VGA-compatible and 3D controller.
const GPU_CLASSES: [u32; 2] = [0x0300, 0x0302];

/// One PCI function as sysfs shows it: the text of its `class` and `vendor` files.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PciFunction<'a> {
    /// The `class` file: `0x` and six hex digits (base class, subclass, interface).
    pub class: &'a str,
    /// The `vendor` file: `0x` and four hex digits.
    pub vendor: &'a str,
}

/// The number of GPUs among a Linux node's PCI functions; see the module docs.
///
/// # Errors
/// A `class` or `vendor` text is not `0x` and hex digits of its width (a trailing
/// newline allowed, as sysfs writes it).
pub fn gpus_from_linux_pci<'a, I>(functions: I) -> Result<u64, ParseError>
where
    I: IntoIterator<Item = PciFunction<'a>>,
{
    let mut gpus = 0;
    for f in functions {
        let class = hex(f.class, 6)? >> 8;
        let vendor = hex(f.vendor, 4)?;
        if GPU_CLASSES.contains(&class) && GPU_VENDORS.contains(&vendor) {
            gpus += 1;
        }
    }
    Ok(gpus)
}

/// The number of GPUs of a Mac from its `sysctl hw.optional` text: one on Apple silicon.
///
/// # Errors
/// The text does not parse, or is not from an Apple silicon Mac; see
/// [`crate::CpuCaps::from_macos_sysctl`].
pub fn gpus_from_macos_sysctl(text: &str) -> Result<u64, ParseError> {
    macos::features(text).map(|_| 1)
}

/// The value of the `gpu.kind` report entry of a Mac whose daemon runs on the hardware:
/// the GPU is the host's own, not a guest's paravirtual device. A node in a guest never
/// reports it, and reports no GPU.
pub const GPU_KIND_NATIVE: &str = "native";

/// The core count of a Mac's GPU from `ioreg -r -d 1 -c AGXAccelerator`: the number in
/// the first `"gpu-core-count" = <n>` line (the tree art `ioreg` draws before a property,
/// `| `, is skipped). `None` when no such line has a number (a
/// guest's paravirtual GPU, or a Mac whose `ioreg` printed nothing).
#[must_use]
pub fn gpu_cores_from_macos_ioreg(text: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        let (key, value) = line.split_once('=')?;
        // `ioreg` draws its tree in front of a node's property lines (`| `, `| | `).
        let key = key.trim_matches(|c: char| c == '|' || c.is_whitespace());
        if key != "\"gpu-core-count\"" {
            return None;
        }
        value.trim().parse().ok().filter(|&n| n > 0)
    })
}

/// The Metal GPU family of an Apple silicon chip, from its name as
/// `machdep.cpu.brand_string` gives it (`Apple M3 Ultra`): the N of the highest
/// `MTLGPUFamilyAppleN` the chip supports. M1 is 7, M2 is 8, M3 and M4 are 9.
/// `None` for a name that is not `Apple M<n>` or a generation this table does not know
/// yet: the node then reports no family, and a newer chip is added here.
#[must_use]
pub fn metal_family_of_apple_chip(brand: &str) -> Option<u32> {
    let generation = brand.strip_prefix("Apple M")?;
    let digits = generation.split(|c: char| !c.is_ascii_digit()).next()?;
    match digits.parse::<u32>().ok()? {
        1 => Some(7),
        2 => Some(8),
        3 | 4 => Some(9),
        _ => None,
    }
}

/// `text` as `0x` and exactly `digits` hex digits, one trailing newline allowed.
fn hex(text: &str, digits: usize) -> Result<u32, ParseError> {
    let bad = || ParseError::PciValue(text.to_owned());
    let value = text.strip_suffix('\n').unwrap_or(text);
    let value = value.strip_prefix("0x").ok_or_else(bad)?;
    if value.len() != digits || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(bad());
    }
    u32::from_str_radix(value, 16).map_err(|_| bad())
}
