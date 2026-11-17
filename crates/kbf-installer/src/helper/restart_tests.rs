use clap::ValueEnum;

use super::*;

fn systemd(unit: &str) -> Restart {
    Restart::Systemd(unit.to_owned())
}

/// Asserts that the restart is `systemctl restart -- <unit>`, with the unit after `--` so that a
/// unit name cannot be taken for an option.
#[test]
fn a_systemd_restart_is_systemctl_restart_of_the_unit() {
    let Step::Run(cmd) = systemd("kbf-storage.service").step() else {
        panic!("a restart runs a command");
    };
    assert_eq!(cmd.program.as_str(), "/usr/bin/systemctl");
    assert_eq!(cmd.args, ["restart", "--", "kbf-storage.service"]);
}

/// Asserts which units are a frontend's: the units `setup` names for the frontend mode, the
/// plain unit and every instance of its template, and no unit of another mode.
#[test]
fn only_the_frontend_modes_units_are_a_frontends() {
    let table: Vec<_> = Mode::value_variants()
        .iter()
        .flat_map(|mode| {
            let units = mode_units(*mode);
            units.iter().map(move |unit| (*mode, *unit))
        })
        .collect();
    let judged: Vec<_> = table
        .iter()
        .map(|(_, unit)| (*unit, frontend_unit(&systemd(unit)) == Some(*unit)))
        .collect();
    let expected: Vec<_> = table
        .iter()
        .map(|(mode, unit)| (*unit, matches!(mode, Mode::Frontend)))
        .collect();
    assert_eq!(judged, expected);
}

/// Asserts the family of a unit name: the frontend's plain unit and any instance of its
/// template are the frontend's, and a name that is the family's only by prefix, an instance that
/// is empty or would leave the slice, another mode's unit and a name with no `.service` are not.
#[test]
fn a_units_family_is_the_frontends_or_not() {
    let forms = [
        ("kbf-frontend@a.service", true),
        ("kbf-frontend@b.service", true),
        ("kbf-frontend.service", true),
        ("kbf-frontend@main-2_x.y:z.service", true),
        ("kbf-storage.service", false),
        ("kbf-frontendx.service", false),
        ("kbf-frontend-b.service", false),
        ("kbf-frontend@.service", false),
        ("kbf-frontend@a/b.service", false),
        ("kbf-frontend@../b.service", false),
        ("kbf-frontend", false),
        ("", false),
    ];
    let seen: Vec<_> = forms
        .iter()
        .map(|(unit, _)| (*unit, frontend_unit(&systemd(unit)).is_some()))
        .collect();
    assert_eq!(seen, forms);
}
