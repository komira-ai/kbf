//! One run over a real Postgres server: the pack index the tool records in is the Postgres one.
//!
//! The test needs a server and fails without one: `KBF_TEST_POSTGRES_URL` names a server it may
//! create and drop databases on. It makes a database of its own.

use kbf_pack_adopt::{Seams, open_postgres};
use kbf_pack_index::schema::test_database;
use kbf_state::postgres::PostgresConfig;

use crate::support::{BUCKET, Farm, SOCKET_URL, says};

/// Asserts that an apply records the segments in the Postgres pack index (the pack of each, and
/// the row of each blob), that the reader finds every blob through that index, and that a second
/// apply records nothing new.
#[test]
fn an_apply_records_the_segments_in_the_postgres_pack_index() {
    let database = test_database();
    let config = PostgresConfig::parse(&database.url()).expect("the test database URL parses");
    let farm = Farm::new("runs-postgres");
    let opener = config.clone();
    let seams = Seams {
        open_index: Box::new(move |_: &PostgresConfig| open_postgres(&opener)),
    };
    let apply = ["--mode", "apply", "--pack-store-bucket", BUCKET];

    let ran = farm.run_with(&seams, SOCKET_URL, &apply);

    assert_eq!(ran.code, 0, "{}", ran.err);
    says(&ran.out, "packs: new 3, already recorded 0");
    let index = open_postgres(&config).expect("opens");
    for key in &farm.keys {
        let pack = index.pack(key).unwrap().expect("recorded");
        assert_eq!(&pack.key, key);
        assert_eq!(pack.blob_count, pack.live_blobs);
    }
    let located = index.locate(&farm.digests()).unwrap();
    assert!(located.iter().all(Option::is_some));
    assert_eq!(farm.read_through(index), farm.found_all());
    let again = farm.run_with(&seams, SOCKET_URL, &apply);
    says(&again.out, "packs: new 0, already recorded 3");
}
