//! The order in which the endpoints are tried for a key.

use super::rank::{rank, weight};

/// The URLs of `count` endpoints.
fn names(count: usize) -> Vec<String> {
    (1..=count)
        .map(|n| format!("http://192.0.2.{n}:9300"))
        .collect()
}

/// Asserts the weight is the first eight bytes of the SHA-256 of the endpoint, a zero byte and
/// the key, as a big-endian number, and the order of three endpoints for one key is the order of
/// those numbers. The values were made outside the code (`printf 'a\0b' | sha256sum`), so a
/// change of the hash, of the separator or of the byte order of the weight moves them: it
/// would move every key to another endpoint at a release.
#[test]
fn the_weight_and_the_order_are_fixed_by_a_known_answer() {
    assert_eq!(weight("a", "b"), 0x59b2_71ae_1bbc_b1d3);
    let key = "cas/b/x";
    assert_eq!(weight("http://192.0.2.1:9300", key), 0x0ece_4951_cd68_31ee);
    assert_eq!(weight("http://192.0.2.2:9300", key), 0x6950_343c_cb4c_4cac);
    assert_eq!(weight("http://192.0.2.3:9300", key), 0x26eb_5cbd_ed4d_9a8e);
    assert_eq!(rank(&names(3), key), [1, 2, 0]);
}

/// Asserts that the order is a function of the names alone: the same call gives the same order,
/// every place appears once, and the same endpoints listed in another order are tried in the
/// same order by name.
#[test]
fn the_order_does_not_depend_on_the_order_of_the_list() {
    let listed = names(6);
    let mut turned = listed.clone();
    turned.reverse();
    for n in 0..200 {
        let key = format!("cas/b/lease-{n}/pack");
        let order = rank(&listed, &key);
        assert_eq!(order, rank(&listed, &key));
        let mut places = order.clone();
        places.sort_unstable();
        assert_eq!(places, [0, 1, 2, 3, 4, 5]);
        let by_name: Vec<&String> = order.iter().map(|place| &listed[*place]).collect();
        let turned_order = rank(&turned, &key);
        let turned_by_name: Vec<&String> =
            turned_order.iter().map(|place| &turned[*place]).collect();
        assert_eq!(by_name, turned_by_name, "{key}");
    }
}

/// Asserts that the first choice of 6000 keys spreads over six endpoints, each taking between
/// 800 and 1200 of them (1000 is the even share), so that no endpoint is left out or loaded
/// twice as much as another.
#[test]
fn the_keys_spread_over_the_endpoints() {
    let listed = names(6);
    let mut firsts = [0_u32; 6];
    for n in 0..6000 {
        firsts[rank(&listed, &format!("cas/b/{n}"))[0]] += 1;
    }
    for (place, count) in firsts.iter().enumerate() {
        assert!(
            (800..=1200).contains(count),
            "endpoint {place} is first for {count}: {firsts:?}"
        );
    }
}

/// Asserts the rendezvous property: with one endpoint taken out of the list, a key whose first
/// choice was another endpoint keeps it, and a key whose first choice was the one taken out goes
/// to the endpoint that was second for it. It would catch a ranking that reshuffles the keys of
/// the endpoints that stay when the list changes.
#[test]
fn taking_an_endpoint_out_moves_only_its_keys() {
    let listed = names(6);
    let mut fewer = listed.clone();
    let gone = fewer.remove(2);
    let (mut moved, mut stayed) = (0, 0);
    for n in 0..1000 {
        let key = format!("cas/b/{n}");
        let before = rank(&listed, &key);
        let after = rank(&fewer, &key);
        let first_before = &listed[before[0]];
        let first_after = &fewer[after[0]];
        if *first_before == gone {
            let second = &listed[before[1]];
            assert_eq!(first_after, second, "{key}");
            moved += 1;
        } else {
            assert_eq!(first_after, first_before, "{key}");
            stayed += 1;
        }
    }
    assert!(
        moved > 0 && stayed > moved,
        "{moved} moved, {stayed} stayed"
    );
}

/// Asserts that two endpoints with the same name, which the file refuses, are still ordered: by
/// their places.
#[test]
fn equal_weights_are_told_apart_by_the_place() {
    let twins = vec!["x".to_owned(), "x".to_owned(), "x".to_owned()];
    assert_eq!(rank(&twins, "k"), [0, 1, 2]);
    assert!(rank(&[], "k").is_empty());
}
