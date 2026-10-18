//! The replay memory a migrated host starts with: the (epoch, order id) the old helper
//! acted on, carried over, and the generation floor, which the old layout never had and
//! so comes from the frontend (when it is the release source), from the operator's
//! `--min-generation`, or from nowhere, loudly (pull-updates.md section 7.4, step 3).
//! A floor only goes up: a repeated migration never lowers it.

use super::error::MigrateError;
use super::schema::Floor;
use crate::helper::memory::Memory;

/// Where a host gets its releases, which decides where the floor is seeded from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Url,
    Frontend,
}

#[derive(Clone, Copy, Debug)]
pub struct Seed {
    pub source: Source,
    pub min_generation: Option<u64>,
}

/// The frontend's current (epoch, generation) for the desired release, fetched over the
/// configured source: a claim from a server the host trusts to name a floor, not a
/// release. The real one is the fetch client of contract P7.
pub trait FrontendFloor {
    fn current(&self) -> Result<(u64, u64), String>;
}

/// The frontend client of a build that has none yet: it never answers.
pub struct NoFrontend;

impl FrontendFloor for NoFrontend {
    fn current(&self) -> Result<(u64, u64), String> {
        Err("this build has no frontend client (contract P7)".to_owned())
    }
}

/// What the seeding decided.
#[derive(Debug, PartialEq, Eq)]
pub struct Seeded {
    pub floor: Floor,
    /// What goes into `installer.toml` as `min_generation`, when it is above zero.
    pub min_generation: Option<u64>,
    pub warnings: Vec<String>,
}

/// The newer of two (epoch, order id) pairs, by epoch first.
fn newer(a: (u64, u64), b: (u64, u64)) -> (u64, u64) {
    a.max(b)
}

/// The floor of a host whose old helper remembered `memory`. The two kinds of order
/// shared one id counter, so the newer pair of the node and frontend kinds is a floor no
/// later order of either kind is below.
pub fn seed(
    memory: &Memory,
    seed: &Seed,
    frontend: &dyn FrontendFloor,
) -> Result<Seeded, MigrateError> {
    let old = newer(
        (memory.acked_epoch, memory.acked_order_id),
        (memory.frontend_epoch, memory.frontend_order_id),
    );
    let mut warnings = Vec::new();
    let (epoch, generation) = match (seed.source, seed.min_generation) {
        (Source::Frontend, flag) => match frontend.current() {
            Ok((epoch, generation)) => (epoch, generation.max(flag.unwrap_or(0))),
            Err(why) => match flag {
                Some(generation) => {
                    warnings.push(format!(
                        "the frontend's generation could not be had ({why}); the floor is seeded from --min-generation {generation}"
                    ));
                    (0, generation)
                }
                None => {
                    return Err(MigrateError::FloorUnavailable(format!(
                        "the frontend's current generation could not be had ({why}): retry, or give --min-generation"
                    )));
                }
            },
        },
        (Source::Url, Some(generation)) => (0, generation),
        (Source::Url, None) => {
            warnings.push(
                "NO REPLAY FLOOR: no frontend to ask and no --min-generation: replay memory starts from generation zero, \
                 so a stale signed order for an older release is not refused until you set min_generation"
                    .to_owned(),
            );
            (0, 0)
        }
    };
    let (epoch, order_id) = newer(old, (epoch, 0));
    Ok(Seeded {
        floor: Floor {
            epoch,
            order_id,
            generation,
        },
        min_generation: (generation > 0).then_some(generation),
        warnings,
    })
}

/// `floor` raised to `other` where `other` is higher; never lowered.
pub fn raise(floor: Floor, other: Floor) -> Floor {
    let (epoch, order_id) = newer((floor.epoch, floor.order_id), (other.epoch, other.order_id));
    Floor {
        epoch,
        order_id,
        generation: floor.generation.max(other.generation),
    }
}
