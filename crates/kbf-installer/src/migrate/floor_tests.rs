use super::floor::*;
use super::*;
use crate::helper::memory::Memory;

struct Fixed(Result<(u64, u64), &'static str>);

impl FrontendFloor for Fixed {
    fn current(&self) -> Result<(u64, u64), String> {
        self.0.map_err(str::to_owned)
    }
}

fn memory(epoch: u64, id: u64, f_epoch: u64, f_id: u64) -> Memory {
    Memory {
        acked_epoch: epoch,
        acked_order_id: id,
        frontend_epoch: f_epoch,
        frontend_order_id: f_id,
        ..Memory::default()
    }
}

fn url(min_generation: Option<u64>) -> Seed {
    Seed {
        source: Source::Url,
        min_generation,
    }
}

fn frontend(min_generation: Option<u64>) -> Seed {
    Seed {
        source: Source::Frontend,
        min_generation,
    }
}

fn floor(epoch: u64, order_id: u64, generation: u64) -> schema::Floor {
    schema::Floor {
        epoch,
        order_id,
        generation,
    }
}

/// Catches replay memory lost in the move: the (epoch, order id) the old helper acted on
/// is carried, and when the node and frontend kinds disagree the newer pair (epoch first)
/// is the floor, since the two kinds shared one counter.
///
/// Mutants seen red: only the node pair read; the pairs compared by id alone.
#[test]
fn the_old_helpers_order_memory_is_carried_over() {
    let seeded = seed(&memory(3, 42, 3, 50), &url(Some(5)), &NoFrontend).expect("seed");
    assert_eq!(seeded.floor, floor(3, 50, 5));
    let seeded = seed(&memory(4, 1, 3, 900), &url(Some(5)), &NoFrontend).expect("seed");
    assert_eq!(
        seeded.floor,
        floor(4, 1, 5),
        "a newer epoch wins whatever the ids"
    );
    let seeded = seed(&memory(0, 0, 0, 0), &url(Some(5)), &NoFrontend).expect("seed");
    assert_eq!(seeded.floor, floor(0, 0, 5));
}

/// Catches the generation floor seeded from the wrong place: from the frontend when it is
/// the source (its epoch lifts the pair when newer; the larger of it and the flag
/// wins), from `--min-generation` for a URL source, and for a URL source with no flag it is
/// zero with a plain warning and nothing for `installer.toml`.
///
/// Mutants seen red: the flag ignored with a frontend; the warning dropped.
#[test]
fn the_generation_floor_comes_from_the_frontend_the_flag_or_a_loud_nothing() {
    let seeded = seed(&memory(3, 42, 0, 0), &frontend(None), &Fixed(Ok((5, 30)))).expect("seed");
    assert_eq!(seeded.floor, floor(5, 0, 30));
    assert_eq!(seeded.min_generation, Some(30));
    assert!(seeded.warnings.is_empty());
    let seeded = seed(&memory(3, 42, 0, 0), &frontend(None), &Fixed(Ok((3, 30)))).expect("seed");
    assert_eq!(
        seeded.floor,
        floor(3, 42, 30),
        "the frontend's older epoch does not lower the pair"
    );
    let seeded = seed(
        &memory(3, 42, 0, 0),
        &frontend(Some(40)),
        &Fixed(Ok((3, 30))),
    )
    .expect("seed");
    assert_eq!(seeded.floor.generation, 40);

    let loud = seed(&memory(0, 0, 0, 0), &url(None), &NoFrontend).expect("seed");
    assert_eq!((loud.floor, loud.min_generation), (floor(0, 0, 0), None));
    assert!(
        loud.warnings[0].contains("NO REPLAY FLOOR"),
        "{:?}",
        loud.warnings
    );
}

/// Catches a frontend that cannot be asked being papered over: with the flag it falls back
/// to the flag and warns; without, the seeding refuses (the operator decides).
#[test]
fn an_unreachable_frontend_falls_back_to_the_flag_or_refuses() {
    let down = Fixed(Err("connection refused"));
    let seeded = seed(&memory(0, 0, 0, 0), &frontend(Some(7)), &down).expect("seed");
    assert_eq!(seeded.floor.generation, 7);
    assert!(
        seeded.warnings[0].contains("connection refused"),
        "{:?}",
        seeded.warnings
    );
    let error = seed(&memory(0, 0, 0, 0), &frontend(None), &down).expect_err("refused");
    assert!(
        matches!(error, MigrateError::FloorUnavailable(_)),
        "{error}"
    );
    assert!(error.to_string().contains("connection refused"));
    assert!(NoFrontend.current().is_err());
}

/// Catches a floor that goes down when a migration is repeated: `raise` takes the higher
/// pair and the higher generation, field by field in the right order.
///
/// Mutant seen red: `raise` returning its second argument.
#[test]
fn a_floor_only_goes_up() {
    assert_eq!(raise(floor(3, 42, 17), floor(3, 10, 99)), floor(3, 42, 99));
    assert_eq!(raise(floor(3, 42, 17), floor(4, 1, 5)), floor(4, 1, 17));
    assert_eq!(raise(floor(3, 42, 17), floor(2, 900, 1)), floor(3, 42, 17));
}
