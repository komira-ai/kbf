//! `kbf_farm_*` on `GET /metrics`: what the farm's writes to the state store have come to.

use std::fmt::Write as _;

use crate::farm::Farm;

/// Appends `kbf_farm_lost_races_total` and `kbf_farm_dropped_writes_total` to `out`. The
/// first is left out on a core that does not count lost races apart
/// ([`Farm::lost_races`]): an absent series, not a 0 that hides races counted as drops.
pub(crate) fn render_farm(out: &mut String, farm: &Farm) {
    if let Some(lost) = farm.lost_races() {
        let _ = writeln!(
            out,
            "# HELP kbf_farm_lost_races_total Writes to the state store refused because \
             another frontend took the same operation first (the designed end of a race; the \
             operation runs once), since the process started\n# TYPE \
             kbf_farm_lost_races_total counter\nkbf_farm_lost_races_total {lost}"
        );
    }
    let _ = writeln!(
        out,
        "# HELP kbf_farm_dropped_writes_total Writes to the state store given up on for \
         good (fenced by a newer writer, for a node another frontend owns, not encodable, or \
         over the held cap), since the process started\n# TYPE kbf_farm_dropped_writes_total \
         counter\nkbf_farm_dropped_writes_total {}",
        farm.dropped_writes()
    );
}
