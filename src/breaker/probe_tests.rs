//! Probe expiry and replacement fence every outcome, not just successful replies.
use super::*;

const AT: u64 = BASE_COOLDOWN_MS + 2;
const LEASE: u64 = 3_000;
const OUTCOMES: [Outcome; 5] = [
    Outcome::Success,
    Outcome::Neutral,
    Outcome::Transient {
        retry_after_ms: None,
    },
    Outcome::Transient {
        retry_after_ms: Some(120_000),
    },
    Outcome::Transient {
        retry_after_ms: Some(u64::MAX),
    },
];

fn probing() -> (Row, Ticket) {
    let mut row = Row::default();
    row.open_for(2, BASE_COOLDOWN_MS);
    let ticket = row.admit(AT, LEASE).unwrap();
    assert!(ticket.probe.is_some());
    (row, ticket)
}

#[test]
fn expired_probe_cannot_change_any_state_even_without_a_successor() {
    for outcome in OUTCOMES {
        for delay in [0, 1, MAX_RETRY_AFTER_MS] {
            let (mut row, expired) = probing();
            let now = AT + LEASE + delay;
            let before = row.clone();
            row.settle(&expired, outcome, now);
            assert_eq!(row, before, "expired {outcome:?} at delay {delay}");
            let next = row.admit(now, LEASE).unwrap();
            assert_ne!(next.probe, expired.probe);
            row.settle(&next, Outcome::Success, now + 1);
            assert!(!row.open, "a valid successor still closes the circuit");
        }
    }
}

#[test]
fn replaced_probe_cannot_impose_retry_after_or_touch_the_successor() {
    for outcome in OUTCOMES {
        let (mut row, expired) = probing();
        let next = row.admit(AT + LEASE, LEASE).unwrap();
        // Replacement does not change the circuit generation. Token ownership
        // must protect Retry-After as well as close/reopen/release transitions.
        assert_eq!(next.generation, expired.generation);
        assert_ne!(next.probe, expired.probe);
        let before = row.clone();
        row.settle(&expired, outcome, AT + LEASE + 1);
        assert_eq!(row, before, "stale outcome {outcome:?}");
        assert_eq!(
            row.admit(AT + LEASE + 2, LEASE),
            Err(Refusal::ProbeInFlight)
        );
        row.settle(&next, Outcome::Success, AT + LEASE + 3);
        assert_eq!(row.admit(AT + LEASE + 4, LEASE).unwrap().probe, None);
    }
}

#[test]
fn released_probe_cannot_settle_twice_or_change_the_next_owner() {
    for outcome in OUTCOMES {
        let (mut row, released) = probing();
        row.settle(&released, Outcome::Neutral, AT + 1);
        let released_state = row.clone();
        row.settle(&released, outcome, AT + 2);
        assert_eq!(row, released_state);
        let next = row.admit(AT + 3, LEASE).unwrap();
        assert_ne!(next.probe, released.probe);
        let before = row.clone();
        row.settle(&released, outcome, AT + 4);
        assert_eq!(row, before);
        row.settle(&next, Outcome::Success, AT + 5);
        assert!(!row.open);
    }
}

#[test]
fn open_circuit_requires_both_generation_and_probe_identity() {
    for outcome in OUTCOMES {
        let (row, owner) = probing();
        for invalid in [
            Ticket {
                generation: owner.generation - 1,
                probe: owner.probe.clone(),
            },
            Ticket {
                generation: owner.generation,
                probe: None,
            },
            Ticket {
                generation: owner.generation,
                probe: Some("another-owner".to_owned()),
            },
        ] {
            let mut actual = row.clone();
            actual.settle(&invalid, outcome, AT + 1);
            assert_eq!(actual, row);
        }
    }
}

#[test]
fn live_probe_just_before_expiry_still_settles_every_outcome() {
    for outcome in OUTCOMES {
        let (mut row, owner) = probing();
        let now = AT + LEASE - 1;
        row.settle(&owner, outcome, now);
        match outcome {
            Outcome::Success => {
                assert!(!row.open);
                assert_eq!(row.generation, owner.generation + 1);
                assert_eq!(row.admit(now, LEASE).unwrap().probe, None);
            }
            Outcome::Neutral => {
                assert!(row.open);
                assert_eq!(row.generation, owner.generation);
                assert!(row.probe_token.is_none());
                assert_ne!(row.admit(now, LEASE).unwrap().probe, owner.probe);
            }
            Outcome::Transient { retry_after_ms } => {
                assert!(row.open);
                assert_eq!(row.generation, owner.generation + 1);
                assert_eq!(row.cooldown_ms, BASE_COOLDOWN_MS * 2);
                assert_eq!(row.open_until_ms, now + BASE_COOLDOWN_MS * 2);
                assert_eq!(
                    row.retry_after_until_ms,
                    retry_after_ms.map_or(0, |delay| now + delay.min(MAX_RETRY_AFTER_MS))
                );
                assert!(matches!(
                    row.admit(now, LEASE),
                    Err(Refusal::CoolingDown { .. })
                ));
            }
        }
    }
}

#[test]
fn closed_circuit_keeps_failure_streak_and_success_reset_semantics() {
    let mut row = Row::default();
    let first = row.admit(0, LEASE).unwrap();
    let second = row.admit(0, LEASE).unwrap();
    let neutral = row.admit(0, LEASE).unwrap();
    row.settle(
        &first,
        Outcome::Transient {
            retry_after_ms: Some(60_000),
        },
        1,
    );
    assert_eq!(row.failures, 1);
    assert_eq!(row.retry_after_until_ms, 60_001);
    row.settle(&neutral, Outcome::Neutral, 2);
    assert_eq!(row.failures, 1);
    row.settle(&second, Outcome::Success, 3);
    assert_eq!(row, Row::default());
    assert_eq!(row.admit(4, LEASE).unwrap().probe, None);
}

fn open_circuit(breaker: &Breaker) {
    for now in 0..3 {
        let ticket = breaker.admit(now, LEASE).unwrap();
        breaker.settle(
            &ticket,
            Outcome::Transient {
                retry_after_ms: None,
            },
            now,
        );
    }
}

#[test]
fn public_process_local_breaker_preserves_recovery_after_expired_outcomes() {
    for outcome in OUTCOMES {
        let breaker = Breaker::new(None, "probe-fence-test");
        open_circuit(&breaker);
        let expired = breaker.admit(AT, LEASE).unwrap();
        breaker.settle(&expired, outcome, AT + LEASE);
        let next = breaker.admit(AT + LEASE + 1, LEASE).unwrap();
        assert!(next.probe.is_some(), "expiry is not successful recovery");
        assert_ne!(next.probe, expired.probe);
        breaker.settle(&expired, outcome, AT + LEASE + 2);
        assert_eq!(
            breaker.admit(AT + LEASE + 3, LEASE),
            Err(Refusal::ProbeInFlight)
        );
        breaker.settle(&next, Outcome::Success, AT + LEASE + 4);
        assert_eq!(breaker.admit(AT + LEASE + 5, LEASE).unwrap().probe, None);
        assert_eq!(breaker.protection(), Protection::ProcessLocal);
    }
}

#[test]
fn shared_wal_reopen_preserves_successors_lease_after_stale_retry_after() {
    let path = std::env::temp_dir()
        .join(format!(
            "sr-breaker-probe-fence-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ))
        .join(BREAKER_FILE);
    let origin = "shared-probe-fence-test";
    let first = Breaker::new(Some(path.clone()), origin);
    open_circuit(&first);
    let expired = first.admit(AT, LEASE).unwrap();
    // A separate instance acquires using a fresh connection to the same WAL
    // database. No simulated storage or provider request is involved.
    let second = Breaker::new(Some(path.clone()), origin);
    let next = second.admit(AT + LEASE, LEASE).unwrap();
    assert_ne!(next.probe, expired.probe);
    for outcome in OUTCOMES {
        first.settle(&expired, outcome, AT + LEASE + 1);
        assert_eq!(first.protection(), Protection::Shared);
        let reopened = Breaker::new(Some(path.clone()), origin);
        assert_eq!(
            reopened.admit(AT + LEASE + 2, LEASE),
            Err(Refusal::ProbeInFlight),
            "obsolete {outcome:?} must not install cooldown or release the probe"
        );
        assert_eq!(reopened.protection(), Protection::Shared);
    }
    second.settle(&next, Outcome::Success, AT + LEASE + 3);
    let reopened = Breaker::new(Some(path), origin);
    assert_eq!(reopened.admit(AT + LEASE + 4, LEASE).unwrap().probe, None);
    assert_eq!(reopened.protection(), Protection::Shared);
}
