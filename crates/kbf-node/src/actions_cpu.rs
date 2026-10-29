//! `--actions-cpu-max`: whether the container driver's actions cgroup keeps its CPU limit.

use clap::ValueEnum;
// The container driver builds on Linux only; elsewhere the flag parses and is refused unless
// `auto`, and nothing reads its mode.
#[cfg(target_os = "linux")]
use kbf_driver_container::{ActionsCpu, ActionsCpuMax};

use super::Driver;

/// The flag, flattened into the command line.
#[derive(Clone, Copy, Debug, clap::Args)]
pub(crate) struct ActionsCpuArgs {
    /// Whether the actions cgroup's `cpu.max` is lifted (container). At start it holds the
    /// leases' CPUs, the node's minus `--reserve-cpus`; the server's Welcome then says,
    /// on every connect and reconnect, whether the farm places by the load the nodes
    /// measure (`Welcome.placement_load`), which a CPU limit would hide (a cgroup throttled
    /// at its limit never measures more than it). `auto` follows the Welcome: `cpu.max` is
    /// `max` when it says so, and the leases' CPUs when it says otherwise or nothing; `on`
    /// lifts the limit and `off` keeps it, whatever the Welcome says. Until the first
    /// Welcome the limit of the start stays. The heartbeat reports what was written
    /// (`NodeLoad.actions_cpu_limited`). Refused with a driver other than `container`.
    #[arg(long, value_enum, default_value_t = Mode::Auto, value_name = "auto|on|off")]
    actions_cpu_max: Mode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum Mode {
    /// Follow the Welcome.
    Auto,
    /// No limit.
    On,
    /// The reserve's limit.
    Off,
}

#[cfg(target_os = "linux")]
impl From<Mode> for ActionsCpuMax {
    fn from(mode: Mode) -> Self {
        match mode {
            Mode::Auto => Self::Auto,
            Mode::On => Self::On,
            Mode::Off => Self::Off,
        }
    }
}

impl ActionsCpuArgs {
    /// Refuses a mode other than `auto` on a node that runs no container driver. Not
    /// generic, so its refusal is one code path whatever data plane the node is built over.
    ///
    /// # Errors
    /// The refusal, naming the mode.
    pub(crate) fn refuse(self, drivers: &[Driver]) -> Result<(), String> {
        let name = match self.actions_cpu_max {
            Mode::Auto => return Ok(()),
            Mode::On => "on",
            Mode::Off => "off",
        };
        if drivers.contains(&Driver::Container) {
            return Ok(());
        }
        Err(format!(
            "--actions-cpu-max={name} sets the CPU limit of the container driver's actions \
             cgroup: only --driver container has one (Linux only)"
        ))
    }

    /// What the driver applies at each Welcome, given the leases' CPUs.
    #[cfg(target_os = "linux")]
    pub(crate) fn config(self, lease_cpus: u64) -> ActionsCpu {
        ActionsCpu {
            mode: self.actions_cpu_max.into(),
            lease_cpus,
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;
    // The mapping test reads the driver's modes, which are Linux only.
    #[cfg(target_os = "linux")]
    use clap::Parser;

    #[cfg(target_os = "linux")]
    use super::*;
    use crate::Cli;

    /// `--actions-cpu-max` is `auto` unless set, takes `auto`, `on` and `off` and nothing
    /// else, and each reaches the driver as its own mode. Catches a default other than
    /// `auto` (a farm that does not set it would stop following the Welcome), two modes
    /// swapped, and a value outside the three accepted.
    #[cfg(target_os = "linux")]
    #[test]
    fn actions_cpu_max_is_parsed_strictly_and_mapped_to_its_mode() {
        let parse = |extra: Option<&str>| {
            let mut all: Vec<String> = [
                "kbf-daemon",
                "--driver=fake",
                "--server=https://127.0.0.1:1",
                "--ca-cert=ca",
                "--cert=cert",
                "--key=key",
                "--node-id=n",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect();
            all.extend(extra.map(str::to_owned));
            Cli::try_parse_from(all)
        };
        let mode = |extra: Option<&str>| parse(extra).expect("parses").actions_cpu.config(1).mode;
        assert_eq!(mode(None), ActionsCpuMax::Auto);
        assert_eq!(mode(Some("--actions-cpu-max=auto")), ActionsCpuMax::Auto);
        assert_eq!(mode(Some("--actions-cpu-max=on")), ActionsCpuMax::On);
        assert_eq!(mode(Some("--actions-cpu-max=off")), ActionsCpuMax::Off);
        for bad in ["", "ON", "true", "max", "1"] {
            let arg = format!("--actions-cpu-max={bad}");
            let err = parse(Some(&arg)).expect_err(bad);
            assert_eq!(err.kind(), clap::error::ErrorKind::InvalidValue, "{bad}");
        }
    }

    /// Catches a command line clap refuses at its first parse, such as two flattened flag
    /// sets with the same group id (each derive names its group after its struct).
    #[test]
    fn the_node_command_line_is_well_formed() {
        Cli::command().debug_assert();
    }
}
