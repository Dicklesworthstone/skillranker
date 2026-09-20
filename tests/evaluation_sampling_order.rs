//! Ordered sampling regressions. Uniform subset membership alone does not
//! establish that prefixes of a returned sample are unbiased.

use skillranker::evaluation::Pcg64Dxsm;
use std::collections::BTreeSet;

#[test]
fn full_population_draws_randomize_order_instead_of_returning_identity() {
    let mut first_positions = BTreeSet::new();
    let mut orders = BTreeSet::new();
    for seed in 0..128 {
        let mut rng = Pcg64Dxsm::from_seed(seed);
        let draw = rng.choice_indices(4, 4, false).unwrap();
        assert_eq!(
            draw.iter().copied().collect::<BTreeSet<_>>(),
            BTreeSet::from([0, 1, 2, 3])
        );
        first_positions.insert(draw[0]);
        orders.insert(draw);
    }
    assert_eq!(
        first_positions,
        BTreeSet::from([0, 1, 2, 3]),
        "all members must be able to lead a full-population draw"
    );
    assert!(
        orders.len() > 1,
        "sampling the full population must not always return identity order"
    );
}

#[test]
fn partial_draws_cover_all_ordered_pairs_without_position_bias() {
    // Six ordered pairs, each with expected count 10,000. A fixed seed makes
    // this reproducible; the broad fixed bound detects positional exclusion,
    // not tiny PRNG bias and not a statistical proof of independence.
    let mut rng = Pcg64Dxsm::from_seed(0x51_72_19);
    let mut counts = [[0usize; 3]; 3];
    for _ in 0..60_000 {
        let draw = rng.choice_indices(3, 2, false).unwrap();
        assert_eq!(draw.len(), 2);
        assert!(draw.iter().all(|&i| i < 3));
        assert_ne!(draw[0], draw[1]);
        counts[draw[0]][draw[1]] += 1;
    }
    for (first, row) in counts.iter().enumerate() {
        for (second, &count) in row.iter().enumerate() {
            if first == second {
                assert_eq!(count, 0);
            } else {
                assert!(
                    (9_200..=10_800).contains(&count),
                    "ordered pair ({first}, {second}) occurred {count} times: {counts:?}"
                );
            }
        }
    }
}

#[test]
fn ordered_draws_replay_from_checkpoint_and_empty_draws_consume_nothing() {
    let mut rng = Pcg64Dxsm::from_seed(2026);
    rng.choice_indices(30, 10, false).unwrap();
    let checkpoint = rng.raw_state();
    for replace in [false, true] {
        assert!(rng.choice_indices(0, 0, replace).unwrap().is_empty());
        assert_eq!(rng.raw_state(), checkpoint);
    }
    let expected = rng.choice_indices(20, 12, false).unwrap();
    let mut restored = Pcg64Dxsm::from_raw_state(checkpoint.0, checkpoint.1);
    assert_eq!(restored.choice_indices(20, 12, false).unwrap(), expected);
    assert_eq!(restored.raw_state(), rng.raw_state());
}
