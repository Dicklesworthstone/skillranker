use skillranker::evaluation::stratified::{AllocationMethod, allocate_sample_sizes};
use std::collections::BTreeMap;

fn sizes(a: usize, b: usize) -> BTreeMap<String, usize> {
    BTreeMap::from([("a".into(), a), ("b".into(), b)])
}

#[test]
fn proportional_allocations_follow_population_not_round_robin_order() {
    let method = AllocationMethod::Proportional { min_floor: 1 };
    assert_eq!(
        allocate_sample_sizes(&sizes(90, 10), 20, &method).unwrap(),
        sizes(18, 2)
    );
    assert_eq!(
        allocate_sample_sizes(&sizes(10, 90), 20, &method).unwrap(),
        sizes(2, 18)
    );
    assert_eq!(
        allocate_sample_sizes(&sizes(3, 3), 3, &method).unwrap(),
        sizes(2, 1)
    );
}

#[test]
fn floors_are_clamped_to_population_and_respect_the_total_budget() {
    for method in [
        AllocationMethod::Proportional { min_floor: 3 },
        AllocationMethod::EqualPerStratum { min_floor: 3 },
    ] {
        assert_eq!(
            allocate_sample_sizes(&sizes(1, 100), 4, &method).unwrap(),
            sizes(1, 3)
        );
        assert!(allocate_sample_sizes(&sizes(1, 100), 3, &method).is_err());
    }
}

#[test]
fn explicit_allocations_validate_budget_and_exact_stratum_namespace_even_at_census() {
    let populations = sizes(3, 4);
    for (budget, allocations) in [
        (4, sizes(3, 3)),
        (4, sizes(1, 1)),
        (7, sizes(1, 1)),
        (7, sizes(0, 4)),
    ] {
        assert!(
            allocate_sample_sizes(
                &populations,
                budget,
                &AllocationMethod::Explicit { allocations }
            )
            .is_err()
        );
    }
    let mut foreign = sizes(2, 2);
    foreign.insert("foreign".into(), 1);
    assert!(
        allocate_sample_sizes(
            &populations,
            4,
            &AllocationMethod::Explicit {
                allocations: foreign
            }
        )
        .is_err()
    );
    assert_eq!(
        allocate_sample_sizes(
            &populations,
            4,
            &AllocationMethod::Explicit {
                allocations: sizes(2, 2)
            }
        )
        .unwrap(),
        sizes(2, 2)
    );
    assert_eq!(
        allocate_sample_sizes(
            &populations,
            10,
            &AllocationMethod::Explicit {
                allocations: populations.clone()
            }
        )
        .unwrap(),
        populations
    );
}

#[test]
fn invalid_populations_and_overflow_fail_instead_of_panicking_or_silently_sampling() {
    let method = AllocationMethod::Proportional { min_floor: 1 };
    assert!(allocate_sample_sizes(&BTreeMap::new(), 1, &method).is_err());
    assert!(allocate_sample_sizes(&sizes(0, 10), 5, &method).is_err());
    assert!(allocate_sample_sizes(&sizes(usize::MAX, 1), 1, &method).is_err());
    assert!(
        allocate_sample_sizes(
            &sizes(3, 4),
            1,
            &AllocationMethod::Proportional {
                min_floor: usize::MAX
            }
        )
        .is_err()
    );
}

#[test]
fn all_small_feasible_allocations_fill_budget_without_exceeding_populations() {
    for a in 1..9 {
        for b in 1..9 {
            for floor in 0..5 {
                for budget in 0..=a + b + 1 {
                    for method in [
                        AllocationMethod::Proportional { min_floor: floor },
                        AllocationMethod::EqualPerStratum { min_floor: floor },
                    ] {
                        let result = allocate_sample_sizes(&sizes(a, b), budget, &method);
                        let required = a.min(floor.max(1)) + b.min(floor.max(1));
                        if budget < required {
                            assert!(result.is_err());
                            continue;
                        }
                        let allocation = result.unwrap();
                        assert_eq!(allocation.values().sum::<usize>(), budget.min(a + b));
                        assert!((a.min(floor.max(1))..=a).contains(&allocation["a"]));
                        assert!((b.min(floor.max(1))..=b).contains(&allocation["b"]));
                    }
                }
            }
        }
    }
}
