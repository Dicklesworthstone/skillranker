use skillranker::evaluation::design_weighted::MultiEndpointAlphaAllocation;
use std::collections::BTreeMap;

#[test]
fn tiny_budgets_cannot_be_overspent_by_an_absolute_tolerance() {
    for budget in [1e-12, 1e-100, 1e-300] {
        assert!(
            MultiEndpointAlphaAllocation::explicit(
                budget,
                BTreeMap::from([("risk".into(), budget * 2.0)])
            )
            .is_err()
        );
        let valid = MultiEndpointAlphaAllocation::explicit(
            budget,
            BTreeMap::from([("risk".into(), budget)]),
        )
        .unwrap();
        assert_eq!(valid.get("risk"), Some(budget));
    }
}

#[test]
fn endpoint_sets_must_be_nonempty_unique_and_named() {
    assert!(MultiEndpointAlphaAllocation::equal_split(0.05, &["risk", "risk"]).is_err());
    assert!(MultiEndpointAlphaAllocation::equal_split(0.05, &[""]).is_err());
    assert!(MultiEndpointAlphaAllocation::explicit(0.05, BTreeMap::new()).is_err());
    assert!(
        MultiEndpointAlphaAllocation::explicit(0.05, BTreeMap::from([(" ".into(), 0.01)])).is_err()
    );
}

#[test]
fn unrepresentable_splits_fail_without_returning_zero_allocations() {
    let smallest = f64::from_bits(1);
    assert!(MultiEndpointAlphaAllocation::equal_split(smallest, &["a", "b"]).is_err());
    assert_eq!(
        MultiEndpointAlphaAllocation::equal_split(smallest, &["a"])
            .unwrap()
            .get("a"),
        Some(smallest)
    );
}

#[test]
fn ordinary_and_small_equal_splits_preserve_every_endpoint() {
    for count in [1, 3, 7, 1000] {
        let names: Vec<_> = (0..count).map(|n| format!("endpoint-{n}")).collect();
        for budget in [0.05, 1e-100, 1e-300] {
            let split = MultiEndpointAlphaAllocation::equal_split(budget, &names).unwrap();
            assert_eq!(split.endpoints().len(), count);
            for name in &names {
                let value = split.get(name).unwrap();
                assert!(value.is_finite() && value > 0.0);
                assert!((value / (budget / count as f64) - 1.0).abs() < 1e-12);
            }
        }
    }
}
