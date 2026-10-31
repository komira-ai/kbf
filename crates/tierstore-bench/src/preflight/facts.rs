//! What the machine is: cores, memory, system, kernel and link speed, read from `/proc`,
//! `/etc` and `/sys` through the [`Host`].

use std::path::Path;

use super::host::{Host, read_text};
use crate::error::BenchError;
use crate::result::Hardware;

/// Logical cores: the `processor` lines of `/proc/cpuinfo`.
#[must_use]
pub fn cpu_cores(cpuinfo: &str) -> u32 {
    let lines = cpuinfo
        .lines()
        .filter(|line| line.starts_with("processor"))
        .count();
    u32::try_from(lines).unwrap_or(u32::MAX)
}

/// `MemTotal` of `/proc/meminfo`, in bytes. `None` if it has no readable `MemTotal`.
#[must_use]
pub fn memory_bytes(meminfo: &str) -> Option<u64> {
    let line = meminfo.lines().find(|l| l.starts_with("MemTotal:"))?;
    let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    kib.checked_mul(1024)
}

/// `PRETTY_NAME` of `/etc/os-release`; `unknown` if it has none.
#[must_use]
pub fn os_name(os_release: &str) -> String {
    os_release
        .lines()
        .find_map(|line| line.strip_prefix("PRETTY_NAME="))
        .map_or("unknown", |value| value.trim().trim_matches('"'))
        .to_owned()
}

/// The link speed in Mbit/s from the text of `/sys/class/net/<interface>/speed`; `None` if it
/// is not a positive number (a down link reads `-1`).
#[must_use]
pub fn link_mbps(speed: &str) -> Option<u32> {
    speed.trim().parse::<u32>().ok().filter(|mbps| *mbps > 0)
}

/// Reads the machine's [`Hardware`]. `link_interface` is the interface whose speed is recorded,
/// if any; an interface that reports no speed gives `None`.
///
/// # Errors
/// [`BenchError`] if `/proc/cpuinfo`, `/proc/meminfo`, `/etc/os-release` or the kernel release
/// cannot be read, or `/proc/meminfo` has no `MemTotal`.
pub fn read_hardware(
    host: &dyn Host,
    link_interface: Option<&str>,
) -> Result<Hardware, BenchError> {
    let cpuinfo = read_text(host, Path::new("/proc/cpuinfo"))?;
    let meminfo = read_text(host, Path::new("/proc/meminfo"))?;
    let os_release = read_text(host, Path::new("/etc/os-release"))?;
    let kernel = read_text(host, Path::new("/proc/sys/kernel/osrelease"))?;
    let memory_bytes = memory_bytes(&meminfo)
        .ok_or_else(|| BenchError::Ops("/proc/meminfo has no MemTotal".to_owned()))?;
    let link = link_interface.and_then(|name| {
        let path = Path::new("/sys/class/net").join(name).join("speed");
        read_text(host, &path).ok().and_then(|t| link_mbps(&t))
    });
    Ok(Hardware {
        cpu_cores: cpu_cores(&cpuinfo),
        memory_bytes,
        os: os_name(&os_release),
        kernel: kernel.trim().to_owned(),
        link_mbps: link,
    })
}
