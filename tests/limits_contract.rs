use skillranker::limits::*;

fn bytes(len: usize) -> Vec<u8> {
    vec![b'x'; len]
}

#[test]
fn default_limits_are_positive_and_cap_plus_one_checked() {
    let mut names = std::collections::BTreeSet::new();
    for limit in ALL_DEFAULT_LIMITS {
        assert!(
            names.insert(limit.name),
            "duplicate default limit {}",
            limit.name
        );
        assert!(limit.max() > 0, "{}", limit.name);
        assert_eq!(
            limit.cap_plus_one().unwrap(),
            limit.max() + 1,
            "{}",
            limit.name
        );
        if limit.max() > 1 {
            limit.check(limit.max() - 1).unwrap();
        }
        limit.check(limit.max()).unwrap();
        assert!(matches!(
            limit.check(limit.max() + 1),
            Err(LimitError::AboveLimit { .. })
        ));
    }

    assert!(ALL_DEFAULT_LIMITS.len() >= 38);

    let huge = ResourceLimit::try_new("huge", LimitUnit::Bytes, usize::MAX).unwrap();
    assert_eq!(
        huge.cap_plus_one(),
        Err(LimitError::ArithmeticOverflow { name: "huge" })
    );
}

#[test]
fn zero_is_not_unlimited_for_user_configured_values() {
    assert_eq!(
        ResourceLimit::try_new("user_limit", LimitUnit::Items, 0),
        Err(LimitError::ZeroIsNotUnlimited { name: "user_limit" })
    );
    assert_eq!(
        BoundedU64::new("timeout_ms", LimitUnit::Milliseconds, 0, 10_000),
        Err(LimitError::ZeroIsNotUnlimited { name: "timeout_ms" })
    );
    assert!(matches!(
        BoundedU64::new("timeout_ms", LimitUnit::Milliseconds, 10_001, 10_000),
        Err(LimitError::AboveLimit {
            name: "timeout_ms",
            observed: 10_001,
            limit: 10_000,
            unit: LimitUnit::Milliseconds
        })
    ));
    assert_eq!(
        DurationMillis::new("deadline_ms", 0, 3_000),
        Err(LimitError::ZeroIsNotUnlimited {
            name: "deadline_ms"
        })
    );
}

#[test]
fn plan_initial_resource_limits_are_declared() {
    let expected = [
        (HOOK_STDIN_BYTES, MIB, LimitUnit::Bytes),
        (NATIVE_TRANSCRIPT_TAIL_BYTES, 2 * MIB, LimitUnit::Bytes),
        (NATIVE_TRANSCRIPT_TAIL_RECORDS, 2_000, LimitUnit::Records),
        (ONE_TRANSCRIPT_RECORD_BYTES, 256 * KIB, LimitUnit::Bytes),
        (CASS_STDOUT_BYTES, 8 * MIB, LimitUnit::Bytes),
        (RECENT_NORMALIZED_MESSAGES, 12, LimitUnit::Items),
        (RENDERED_CONTEXT_SCALARS, 12_000, LimitUnit::UnicodeScalars),
        (SKILL_FILE_BYTES, 256 * KIB, LimitUnit::Bytes),
        (SKILL_FRONTMATTER_BYTES, 16 * KIB, LimitUnit::Bytes),
        (DISCOVERY_FILES, 10_000, LimitUnit::Records),
        (DISCOVERY_PARSED_BYTES, 32 * MIB, LimitUnit::Bytes),
        (WIDE_EXCERPT_SCALARS, 160, LimitUnit::UnicodeScalars),
        (RERANK_DESCRIPTION_SCALARS, 1_000, LimitUnit::UnicodeScalars),
        (RERANK_BODY_EXCERPT_SCALARS, 700, LimitUnit::UnicodeScalars),
        (SERIALIZED_REQUEST_BYTES, 96 * KIB, LimitUnit::Bytes),
        (DECODED_RESPONSE_BYTES, 2 * MIB, LimitUnit::Bytes),
        (OBSERVATION_DELTA_BYTES, 8 * MIB, LimitUnit::Bytes),
        (MAX_EXPLICIT_REQUESTS, 32, LimitUnit::Items),
        (YAML_DOCUMENT_DEPTH, 64, LimitUnit::Depth),
        (YAML_ALIASES, 128, LimitUnit::Items),
    ];
    for (limit, max, unit) in expected {
        assert_eq!(limit.max(), max, "{}", limit.name);
        assert_eq!(limit.unit, unit, "{}", limit.name);
    }
    YAML_ALIASES.check(128).unwrap();
    assert_eq!(
        YAML_ALIASES.check(129),
        Err(LimitError::AboveLimit {
            name: "yaml_aliases",
            observed: 129,
            limit: 128,
            unit: LimitUnit::Items,
        })
    );
}

#[test]
fn byte_count_depth_and_record_boundaries_have_success_twins() {
    HOOK_STDIN_BYTES
        .check_bytes(&bytes(HOOK_STDIN_BYTES.max() - 1))
        .unwrap();
    HOOK_STDIN_BYTES
        .check_bytes(&bytes(HOOK_STDIN_BYTES.max()))
        .unwrap();
    assert!(matches!(
        HOOK_STDIN_BYTES.check(HOOK_STDIN_BYTES.max() + 1),
        Err(LimitError::AboveLimit {
            unit: LimitUnit::Bytes,
            ..
        })
    ));

    EXPLICIT_ROSTER_RECORDS.check(9_999).unwrap();
    EXPLICIT_ROSTER_RECORDS.check(10_000).unwrap();
    assert!(matches!(
        EXPLICIT_ROSTER_RECORDS.check(10_001),
        Err(LimitError::AboveLimit {
            unit: LimitUnit::Records,
            ..
        })
    ));

    NORMALIZED_CONTEXT_DEPTH.check(63).unwrap();
    NORMALIZED_CONTEXT_DEPTH.check(64).unwrap();
    assert!(matches!(
        NORMALIZED_CONTEXT_DEPTH.check(65),
        Err(LimitError::AboveLimit {
            unit: LimitUnit::Depth,
            ..
        })
    ));
}

#[test]
fn unicode_scalar_limits_are_not_byte_limits() {
    let text = "aé🦀";
    assert_eq!(text.len(), 7);
    assert_eq!(unicode_scalar_count(text), 3);

    let scalar_limit =
        ResourceLimit::try_new("tiny_scalars", LimitUnit::UnicodeScalars, 3).unwrap();
    scalar_limit.check_unicode_scalars(text).unwrap();
    assert!(matches!(
        scalar_limit.check_unicode_scalars("abcd"),
        Err(LimitError::AboveLimit {
            unit: LimitUnit::UnicodeScalars,
            ..
        })
    ));

    let byte_limit = ResourceLimit::try_new("tiny_bytes", LimitUnit::Bytes, 7).unwrap();
    byte_limit.check_str_bytes(text).unwrap();
    assert!(matches!(
        byte_limit.check_str_bytes("abcdefgh"),
        Err(LimitError::AboveLimit {
            unit: LimitUnit::Bytes,
            ..
        })
    ));
    assert_eq!(
        byte_limit.check_unicode_scalars("🦀🦀🦀🦀🦀🦀🦀🦀"),
        Err(LimitError::WrongLimitUnit {
            name: "tiny_bytes",
            expected: LimitUnit::UnicodeScalars,
            actual: LimitUnit::Bytes,
        })
    );
    assert_eq!(
        scalar_limit.check_str_bytes("abc"),
        Err(LimitError::WrongLimitUnit {
            name: "tiny_scalars",
            expected: LimitUnit::Bytes,
            actual: LimitUnit::UnicodeScalars,
        })
    );
}

#[test]
fn checked_arithmetic_reports_usize_and_u64_overflow() {
    assert_eq!(checked_add_usize("usize_add", 40, 2).unwrap(), 42);
    assert_eq!(checked_add_u64("u64_add", 40, 2).unwrap(), 42);
    assert_eq!(
        checked_add_usize("usize_add", usize::MAX, 1),
        Err(LimitError::ArithmeticOverflow { name: "usize_add" })
    );
    assert_eq!(
        checked_add_u64("u64_add", u64::MAX, 1),
        Err(LimitError::ArithmeticOverflow { name: "u64_add" })
    );
}

#[test]
fn invocation_deadline_reserves_cleanup_on_monotonic_clock() {
    let start = MonotonicMillis::from_millis(1_000);
    let deadline = InvocationDeadline::default_from_start(start).unwrap();

    assert_eq!(deadline.start(), start);
    assert_eq!(deadline.total().as_millis(), 3_000);
    assert_eq!(deadline.cleanup_reserve().as_millis(), 200);
    assert_eq!(deadline.expires_at(), MonotonicMillis::from_millis(4_000));
    assert_eq!(
        deadline.latest_work_time(),
        MonotonicMillis::from_millis(3_800)
    );

    assert_eq!(
        deadline
            .remaining_before_cleanup(MonotonicMillis::from_millis(3_799))
            .as_millis(),
        1
    );
    assert_eq!(
        deadline
            .remaining_before_cleanup(MonotonicMillis::from_millis(3_800))
            .as_millis(),
        0
    );
    assert!(deadline.is_in_cleanup_reserve(MonotonicMillis::from_millis(3_800)));
    assert!(!deadline.is_expired(MonotonicMillis::from_millis(3_999)));
    assert!(deadline.is_expired(MonotonicMillis::from_millis(4_000)));
}

#[test]
fn invalid_cleanup_reserve_and_monotonic_overflow_are_rejected() {
    let start = MonotonicMillis::from_millis(0);
    let total = DurationMillis::new("total", 200, 1_000).unwrap();
    let reserve_equal = DurationMillis::new("reserve", 200, 1_000).unwrap();
    assert_eq!(
        InvocationDeadline::new(start, total, reserve_equal),
        Err(LimitError::InvalidDeadlineReserve {
            total_ms: 200,
            cleanup_reserve_ms: 200
        })
    );

    let near_max = MonotonicMillis::from_millis(u64::MAX - 10);
    let total = DurationMillis::new("total", 20, 1_000).unwrap();
    let reserve = DurationMillis::new("reserve", 1, 1_000).unwrap();
    assert_eq!(
        InvocationDeadline::new(near_max, total, reserve),
        Err(LimitError::ArithmeticOverflow {
            name: "invocation_deadline_end"
        })
    );
    assert_eq!(
        InvocationDeadline::default_from_start(MonotonicMillis::from_millis(u64::MAX - 10)),
        Err(LimitError::ArithmeticOverflow {
            name: "invocation_deadline_end"
        })
    );

    let valid = InvocationDeadline::default_from_start(MonotonicMillis::from_millis(0)).unwrap();
    let zero_cleanup = valid.remaining_before_cleanup(MonotonicMillis::from_millis(2_800));
    assert_eq!(zero_cleanup.as_millis(), 0);
    assert_eq!(
        InvocationDeadline::new(
            MonotonicMillis::from_millis(0),
            DurationMillis::new("total", 1_000, 3_000).unwrap(),
            zero_cleanup,
        ),
        Err(LimitError::ZeroIsNotUnlimited {
            name: "cleanup_reserve"
        })
    );
}

#[test]
fn wall_clock_expiry_is_distinct_from_monotonic_deadline() {
    let wall_start = WallClockMillis::from_unix_millis(1_700_000_000_000);
    let ttl = DurationMillis::new("ttl", 60_000, 3_600_000).unwrap();
    let expiry = WallClockExpiry::new(wall_start, ttl).unwrap();

    assert_eq!(expiry.not_before(), wall_start);
    assert!(expiry.is_expired(WallClockMillis::from_unix_millis(1_699_999_999_999)));
    assert!(!expiry.is_expired(wall_start));
    assert_eq!(
        expiry.expires_at(),
        WallClockMillis::from_unix_millis(1_700_000_060_000)
    );
    assert!(!expiry.is_expired(WallClockMillis::from_unix_millis(1_700_000_059_999)));
    assert!(expiry.is_expired(WallClockMillis::from_unix_millis(1_700_000_060_000)));

    let monotonic_deadline =
        InvocationDeadline::default_from_start(MonotonicMillis::from_millis(0)).unwrap();
    assert_eq!(
        monotonic_deadline.expires_at(),
        MonotonicMillis::from_millis(3_000)
    );
}

#[test]
fn invalid_monotonic_samples_cannot_increase_remaining_budget() {
    let deadline =
        InvocationDeadline::default_from_start(MonotonicMillis::from_millis(1_000)).unwrap();
    for now in [0, 999, 4_000, u64::MAX] {
        let now = MonotonicMillis::from_millis(now);
        assert_eq!(deadline.remaining_before_cleanup(now).as_millis(), 0);
        assert_eq!(deadline.remaining_until_expiry(now).as_millis(), 0);
        assert!(deadline.ensure_can_start_work(now, "test").is_err());
    }
    let start = deadline.start();
    assert_eq!(deadline.remaining_before_cleanup(start).as_millis(), 2_800);
    assert_eq!(deadline.remaining_until_expiry(start).as_millis(), 3_000);
    assert!(deadline.ensure_can_start_work(start, "test").is_ok());
}

#[test]
fn wall_clock_addition_checks_the_result_across_the_full_signed_range() {
    let maximum = DurationMillis::new("ttl", u64::MAX, u64::MAX).unwrap();
    assert_eq!(
        WallClockMillis::from_unix_millis(i64::MIN)
            .checked_add(maximum, "test")
            .unwrap(),
        WallClockMillis::from_unix_millis(i64::MAX)
    );
    assert_eq!(
        WallClockMillis::from_unix_millis(0).checked_add(maximum, "test"),
        Err(LimitError::ArithmeticOverflow { name: "test" })
    );
    let one = DurationMillis::new("ttl", 1, 1).unwrap();
    assert_eq!(
        WallClockMillis::from_unix_millis(-1)
            .checked_add(one, "test")
            .unwrap(),
        WallClockMillis::from_unix_millis(0)
    );
    assert!(WallClockExpiry::new(WallClockMillis::from_unix_millis(i64::MAX), one).is_err());
}

#[test]
fn attempt_reservation_requires_active_logical_request_and_work_time() {
    let limits = AttemptLimits::default_invocation();
    let deadline =
        InvocationDeadline::default_from_start(MonotonicMillis::from_millis(1_000)).unwrap();

    let mut counters = AttemptCounters::new(limits);
    assert_eq!(
        counters
            .reserve_attempt(MonotonicMillis::from_millis(1_001), deadline)
            .unwrap_err(),
        LimitError::NoLogicalRequestForAttempt
    );
    assert_eq!(counters.logical_used(), 0);
    assert_eq!(counters.attempts_used(), 0);

    counters.begin_logical_request().unwrap();
    assert_eq!(
        counters
            .reserve_attempt(MonotonicMillis::from_millis(999), deadline)
            .unwrap_err(),
        LimitError::DeadlineBeforeStart {
            name: "http_attempt",
            now_ms: 999,
            start_ms: 1_000,
        }
    );
    assert_eq!(counters.logical_used(), 1);
    assert_eq!(counters.attempts_used(), 0);

    assert_eq!(
        counters
            .reserve_attempt(MonotonicMillis::from_millis(3_800), deadline)
            .unwrap_err(),
        LimitError::DeadlineInCleanupReserve {
            name: "http_attempt",
            now_ms: 3_800,
            latest_work_ms: 3_800,
            expires_ms: 4_000,
        }
    );
    assert_eq!(counters.logical_used(), 1);
    assert_eq!(counters.attempts_used(), 0);

    assert_eq!(
        counters
            .reserve_attempt(MonotonicMillis::from_millis(4_000), deadline)
            .unwrap_err(),
        LimitError::DeadlineExpired {
            name: "http_attempt",
            now_ms: 4_000,
            expires_ms: 4_000,
        }
    );
    assert_eq!(counters.logical_used(), 1);
    assert_eq!(counters.attempts_used(), 0);
}

#[test]
fn attempt_limits_and_receipts_preserve_default_budget_without_network_admission() {
    let limits = AttemptLimits::default_invocation();
    assert_eq!(limits.logical_requests(), 2);
    assert_eq!(limits.http_attempts(), 4);
    assert_eq!(
        AttemptLimits::new(0, 4),
        Err(LimitError::ZeroIsNotUnlimited {
            name: "logical_requests"
        })
    );
    assert_eq!(
        AttemptLimits::new(2, 0),
        Err(LimitError::ZeroIsNotUnlimited {
            name: "http_attempts"
        })
    );
    assert_eq!(
        AttemptLimits::new(3, 2),
        Err(LimitError::AttemptsBelowLogicalRequests {
            logical_requests: 3,
            http_attempts: 2
        })
    );

    let deadline =
        InvocationDeadline::default_from_start(MonotonicMillis::from_millis(10)).unwrap();
    let mut counters = AttemptCounters::new(limits);
    counters.begin_logical_request().unwrap();
    let receipt1 = counters
        .reserve_attempt(MonotonicMillis::from_millis(20), deadline)
        .unwrap();
    let receipt2 = counters
        .reserve_attempt(MonotonicMillis::from_millis(30), deadline)
        .unwrap();
    counters.begin_logical_request().unwrap();
    let receipt3 = counters
        .reserve_attempt(MonotonicMillis::from_millis(40), deadline)
        .unwrap();
    let receipt4 = counters
        .reserve_attempt(MonotonicMillis::from_millis(50), deadline)
        .unwrap();

    assert_eq!(receipt1.logical_limit(), 2);
    assert_eq!(receipt1.attempt_limit(), 4);
    assert_eq!(receipt1.logical_used_at_reservation(), 1);
    assert_eq!(receipt1.attempt_sequence(), 1);
    assert_eq!(receipt1.remaining_until_expiry_ms(), 2_990);
    assert_eq!(receipt1.remaining_before_cleanup_ms(), 2_790);
    assert_eq!(receipt2.attempt_sequence(), 2);
    assert_eq!(receipt3.logical_used_at_reservation(), 2);
    assert_eq!(receipt4.attempt_sequence(), 4);
    assert_eq!(counters.logical_used(), 2);
    assert_eq!(counters.attempts_used(), 4);
    assert_eq!(
        counters
            .reserve_attempt(MonotonicMillis::from_millis(60), deadline)
            .unwrap_err(),
        LimitError::AboveLimit {
            name: "http_attempts",
            observed: 5,
            limit: 4,
            unit: LimitUnit::Attempts,
        }
    );
    assert_eq!(counters.logical_used(), 2);
    assert_eq!(counters.attempts_used(), 4);
    assert_eq!(
        counters.begin_logical_request().unwrap_err(),
        LimitError::AboveLimit {
            name: "logical_requests",
            observed: 3,
            limit: 2,
            unit: LimitUnit::LogicalRequests,
        }
    );
    assert_eq!(counters.logical_used(), 2);
    assert_eq!(counters.attempts_used(), 4);
}

#[test]
fn live_batch_rejects_zero_runtime_from_duration_arithmetic() {
    let start = MonotonicMillis::from_millis(1_000);
    let zero = start.saturating_duration_since(start);
    assert_eq!(
        BatchBounds::live(start, zero, 1),
        Err(LimitError::ZeroIsNotUnlimited {
            name: "batch_runtime"
        })
    );

    let positive = DurationMillis::new("batch_runtime", 201, 600_000).unwrap();
    let batch = BatchBounds::live(start, positive, 1).unwrap();
    assert_eq!(batch.per_case_deadline_at(start).unwrap().total(), positive);
    assert_eq!(
        BatchBounds::live(start, positive, 0),
        Err(LimitError::MissingLiveBatchRequestCap)
    );
}

#[test]
fn batch_and_maintenance_bounds_are_separate_from_one_shot_invocation() {
    let batch = BatchBounds::replay_default(MonotonicMillis::from_millis(0)).unwrap();
    assert_eq!(batch.max_runtime().as_millis(), 600_000);
    let early_case = batch
        .per_case_deadline_at(MonotonicMillis::from_millis(1_000))
        .unwrap();
    assert_eq!(early_case.total().as_millis(), 3_000);
    assert_eq!(early_case.expires_at(), MonotonicMillis::from_millis(4_000));

    let late_case = batch
        .per_case_deadline_at(MonotonicMillis::from_millis(598_000))
        .unwrap();
    assert_eq!(late_case.total().as_millis(), 2_000);
    assert_eq!(
        late_case.expires_at(),
        MonotonicMillis::from_millis(600_000)
    );
    assert_eq!(
        batch
            .per_case_deadline_at(MonotonicMillis::from_millis(599_800))
            .unwrap_err(),
        LimitError::DeadlineInCleanupReserve {
            name: "batch_case",
            now_ms: 599_800,
            latest_work_ms: 599_800,
            expires_ms: 600_000,
        }
    );
    assert_eq!(
        batch
            .per_case_deadline_at(MonotonicMillis::from_millis(600_000))
            .unwrap_err(),
        LimitError::DeadlineExpired {
            name: "batch_case",
            now_ms: 600_000,
            expires_ms: 600_000,
        }
    );
    assert_eq!(
        batch.network_mode(),
        BatchNetworkMode::ReplayOnlyZeroNetwork
    );

    assert_eq!(
        BatchBounds::replay_default(MonotonicMillis::from_millis(u64::MAX - 10)),
        Err(LimitError::ArithmeticOverflow {
            name: "batch_deadline_end"
        })
    );
    let tiny_live = BatchBounds::live(
        MonotonicMillis::from_millis(0),
        DurationMillis::new("batch_runtime", 100, 600_000).unwrap(),
        1,
    )
    .unwrap();
    assert_eq!(
        tiny_live
            .per_case_deadline_at(MonotonicMillis::from_millis(0))
            .unwrap_err(),
        LimitError::DeadlineInCleanupReserve {
            name: "batch_case",
            now_ms: 0,
            latest_work_ms: 0,
            expires_ms: 100,
        }
    );

    let live = BatchBounds::live(
        MonotonicMillis::from_millis(0),
        DurationMillis::new("batch_runtime", 120_000, 600_000).unwrap(),
        10,
    )
    .unwrap();
    assert_eq!(
        live.network_mode(),
        BatchNetworkMode::LiveWithExplicitRequestCap {
            max_http_attempts: 10
        }
    );
    assert_eq!(
        BatchBounds::live(
            MonotonicMillis::from_millis(0),
            DurationMillis::new("batch_runtime", 120_000, 600_000).unwrap(),
            0,
        ),
        Err(LimitError::MissingLiveBatchRequestCap)
    );

    let maintenance = MaintenanceBounds::defaults();
    assert_eq!(maintenance.sqlite_busy_wait().as_millis(), 25);
    assert_eq!(maintenance.state_store().max(), 256 * MIB);
    assert_eq!(maintenance.cache_and_coordinator().max(), 64 * MIB);
}
