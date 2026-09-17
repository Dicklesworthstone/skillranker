//! Resource limits, checked counters, and clock boundary types.
//!
//! This module is intentionally pure and runtime-free. It defines the constants
//! and validation helpers that later input, transport, replay, batch, and
//! maintenance code consume; it does not perform I/O, spawn tasks, or admit real
//! provider requests.

use core::fmt;

pub const KIB: usize = 1024;
pub const MIB: usize = 1024 * KIB;

pub const DEFAULT_INVOCATION_DEADLINE_MS: u64 = 3_000;
pub const DEFAULT_OUTPUT_CLEANUP_RESERVE_MS: u64 = 200;
pub const DEFAULT_LOGICAL_REQUESTS: u32 = 2;
pub const DEFAULT_HTTP_ATTEMPTS: u32 = 4;
pub const DEFAULT_EVAL_BATCH_RUNTIME_MS: u64 = 600_000;
pub const DEFAULT_SQLITE_BUSY_WAIT_MS: u64 = 25;
pub const DEFAULT_WATCH_MIN_INTERVAL_MS: u64 = 5_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LimitUnit {
    Bytes,
    Records,
    Items,
    Depth,
    UnicodeScalars,
    Milliseconds,
    Attempts,
    LogicalRequests,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LimitError {
    ZeroIsNotUnlimited {
        name: &'static str,
    },
    AboveLimit {
        name: &'static str,
        observed: u128,
        limit: u128,
        unit: LimitUnit,
    },
    ArithmeticOverflow {
        name: &'static str,
    },
    InvalidDeadlineReserve {
        total_ms: u64,
        cleanup_reserve_ms: u64,
    },
    AttemptsBelowLogicalRequests {
        logical_requests: u32,
        http_attempts: u32,
    },
    MissingLiveBatchRequestCap,
    WrongLimitUnit {
        name: &'static str,
        expected: LimitUnit,
        actual: LimitUnit,
    },
    NoLogicalRequestForAttempt,
    DeadlineBeforeStart {
        name: &'static str,
        now_ms: u64,
        start_ms: u64,
    },
    DeadlineInCleanupReserve {
        name: &'static str,
        now_ms: u64,
        latest_work_ms: u64,
        expires_ms: u64,
    },
    DeadlineExpired {
        name: &'static str,
        now_ms: u64,
        expires_ms: u64,
    },
}

impl fmt::Display for LimitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroIsNotUnlimited { name } => {
                write!(f, "{name} must be positive; zero is not unlimited")
            }
            Self::AboveLimit {
                name,
                observed,
                limit,
                unit,
            } => {
                write!(
                    f,
                    "{name} observed {observed} exceeds limit {limit} {unit:?}"
                )
            }
            Self::ArithmeticOverflow { name } => write!(f, "{name} arithmetic overflow"),
            Self::InvalidDeadlineReserve {
                total_ms,
                cleanup_reserve_ms,
            } => write!(
                f,
                "cleanup reserve {cleanup_reserve_ms}ms must be smaller than total deadline {total_ms}ms",
            ),
            Self::AttemptsBelowLogicalRequests {
                logical_requests,
                http_attempts,
            } => write!(
                f,
                "http attempt limit {http_attempts} cannot be below logical request limit {logical_requests}",
            ),
            Self::MissingLiveBatchRequestCap => {
                f.write_str("live batch evaluation requires an explicit HTTP request cap")
            }
            Self::WrongLimitUnit {
                name,
                expected,
                actual,
            } => write!(
                f,
                "{name} uses {actual:?} limit where {expected:?} was required",
            ),
            Self::NoLogicalRequestForAttempt => {
                f.write_str("an HTTP attempt cannot be reserved before a logical request starts")
            }
            Self::DeadlineBeforeStart {
                name,
                now_ms,
                start_ms,
            } => write!(
                f,
                "{name} time {now_ms}ms is before deadline start {start_ms}ms",
            ),
            Self::DeadlineInCleanupReserve {
                name,
                now_ms,
                latest_work_ms,
                expires_ms,
            } => write!(
                f,
                "{name} time {now_ms}ms is in cleanup reserve [{latest_work_ms}ms, {expires_ms}ms)",
            ),
            Self::DeadlineExpired {
                name,
                now_ms,
                expires_ms,
            } => write!(
                f,
                "{name} time {now_ms}ms is at or after expiry {expires_ms}ms"
            ),
        }
    }
}

impl std::error::Error for LimitError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResourceLimit {
    pub name: &'static str,
    pub unit: LimitUnit,
    max: usize,
}

impl ResourceLimit {
    const fn new_const(name: &'static str, unit: LimitUnit, max: usize) -> Self {
        // All public constants below use non-zero values; fallible construction
        // exists for user configuration where zero must not mean unlimited.
        Self { name, unit, max }
    }

    pub fn try_new(name: &'static str, unit: LimitUnit, max: usize) -> Result<Self, LimitError> {
        if max == 0 {
            return Err(LimitError::ZeroIsNotUnlimited { name });
        }
        Ok(Self { name, unit, max })
    }

    pub const fn max(self) -> usize {
        self.max
    }

    pub fn cap_plus_one(self) -> Result<usize, LimitError> {
        self.max
            .checked_add(1)
            .ok_or(LimitError::ArithmeticOverflow { name: self.name })
    }

    pub fn check(self, observed: usize) -> Result<(), LimitError> {
        if observed <= self.max {
            Ok(())
        } else {
            Err(LimitError::AboveLimit {
                name: self.name,
                observed: observed as u128,
                limit: self.max as u128,
                unit: self.unit,
            })
        }
    }

    pub fn check_bytes(self, bytes: &[u8]) -> Result<(), LimitError> {
        self.require_unit(LimitUnit::Bytes)?;
        self.check(bytes.len())
    }

    pub fn check_str_bytes(self, text: &str) -> Result<(), LimitError> {
        self.require_unit(LimitUnit::Bytes)?;
        self.check(text.len())
    }

    pub fn check_unicode_scalars(self, text: &str) -> Result<(), LimitError> {
        self.require_unit(LimitUnit::UnicodeScalars)?;
        match self.cap_plus_one() {
            Ok(stop_after) => {
                let counted = text.chars().take(stop_after).count();
                self.check(counted)
            }
            // A str cannot contain more Unicode scalar values than its byte
            // length, and no allocation can exceed usize::MAX bytes. Avoid an
            // unbounded scan for a theoretical max-sized scalar limit.
            Err(LimitError::ArithmeticOverflow { .. }) => Ok(()),
            Err(err) => Err(err),
        }
    }

    fn require_unit(self, expected: LimitUnit) -> Result<(), LimitError> {
        if self.unit == expected {
            Ok(())
        } else {
            Err(LimitError::WrongLimitUnit {
                name: self.name,
                expected,
                actual: self.unit,
            })
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BoundedU64 {
    pub name: &'static str,
    pub unit: LimitUnit,
    value: u64,
}

impl BoundedU64 {
    pub fn new(
        name: &'static str,
        unit: LimitUnit,
        value: u64,
        max: u64,
    ) -> Result<Self, LimitError> {
        if value == 0 {
            return Err(LimitError::ZeroIsNotUnlimited { name });
        }
        if value > max {
            return Err(LimitError::AboveLimit {
                name,
                observed: value as u128,
                limit: max as u128,
                unit,
            });
        }
        Ok(Self { name, unit, value })
    }

    pub const fn get(self) -> u64 {
        self.value
    }
}

pub const HOOK_STDIN_BYTES: ResourceLimit =
    ResourceLimit::new_const("hook_stdin", LimitUnit::Bytes, MIB);
pub const NORMALIZED_CONTEXT_JSON_BYTES: ResourceLimit =
    ResourceLimit::new_const("normalized_context_json", LimitUnit::Bytes, MIB);
pub const NORMALIZED_CONTEXT_DEPTH: ResourceLimit =
    ResourceLimit::new_const("normalized_context_depth", LimitUnit::Depth, 64);
pub const EXPLICIT_ROSTER_JSON_BYTES: ResourceLimit =
    ResourceLimit::new_const("explicit_roster_json", LimitUnit::Bytes, 32 * MIB);
pub const EXPLICIT_ROSTER_RECORDS: ResourceLimit =
    ResourceLimit::new_const("explicit_roster_records", LimitUnit::Records, 10_000);
pub const EXPLICIT_ROSTER_DEPTH: ResourceLimit =
    ResourceLimit::new_const("explicit_roster_depth", LimitUnit::Depth, 64);
pub const CONFIG_FILE_BYTES: ResourceLimit =
    ResourceLimit::new_const("config_file", LimitUnit::Bytes, 256 * KIB);
pub const CONFIG_FILE_DEPTH: ResourceLimit =
    ResourceLimit::new_const("config_file_depth", LimitUnit::Depth, 32);
pub const YAML_DOCUMENT_DEPTH: ResourceLimit =
    ResourceLimit::new_const("yaml_document_depth", LimitUnit::Depth, 64);
pub const YAML_ALIASES: ResourceLimit =
    ResourceLimit::new_const("yaml_aliases", LimitUnit::Items, 128);
pub const SKILL_FILE_BYTES: ResourceLimit =
    ResourceLimit::new_const("skill_file", LimitUnit::Bytes, 256 * KIB);
pub const SKILL_FRONTMATTER_BYTES: ResourceLimit =
    ResourceLimit::new_const("skill_frontmatter", LimitUnit::Bytes, 16 * KIB);
pub const DISCOVERY_FILES: ResourceLimit =
    ResourceLimit::new_const("discovery_files", LimitUnit::Records, 10_000);
pub const DISCOVERY_PARSED_BYTES: ResourceLimit =
    ResourceLimit::new_const("discovery_parsed", LimitUnit::Bytes, 32 * MIB);
pub const REPLAY_CASE_BYTES: ResourceLimit =
    ResourceLimit::new_const("replay_case", LimitUnit::Bytes, 16 * MIB);
pub const REPLAY_CASE_DEPTH: ResourceLimit =
    ResourceLimit::new_const("replay_case_depth", LimitUnit::Depth, 64);
pub const REPLAY_POLICY_BYTES: ResourceLimit =
    ResourceLimit::new_const("replay_policy", LimitUnit::Bytes, 64 * KIB);
pub const REPLAY_POLICY_DEPTH: ResourceLimit =
    ResourceLimit::new_const("replay_policy_depth", LimitUnit::Depth, 32);
pub const EVALUATION_DATASET_BYTES: ResourceLimit =
    ResourceLimit::new_const("evaluation_dataset", LimitUnit::Bytes, 256 * MIB);
pub const EVALUATION_CASE_RECORDS: ResourceLimit =
    ResourceLimit::new_const("evaluation_case_records", LimitUnit::Records, 10_000);
pub const EVALUATION_DATASET_DEPTH: ResourceLimit =
    ResourceLimit::new_const("evaluation_dataset_depth", LimitUnit::Depth, 64);
pub const NATIVE_TRANSCRIPT_TAIL_BYTES: ResourceLimit =
    ResourceLimit::new_const("native_transcript_tail", LimitUnit::Bytes, 2 * MIB);
pub const NATIVE_TRANSCRIPT_TAIL_RECORDS: ResourceLimit =
    ResourceLimit::new_const("native_transcript_tail_records", LimitUnit::Records, 2_000);
pub const ONE_TRANSCRIPT_RECORD_BYTES: ResourceLimit =
    ResourceLimit::new_const("one_transcript_record", LimitUnit::Bytes, 256 * KIB);
pub const CASS_STDOUT_BYTES: ResourceLimit =
    ResourceLimit::new_const("cass_stdout", LimitUnit::Bytes, 8 * MIB);
pub const RECENT_NORMALIZED_MESSAGES: ResourceLimit =
    ResourceLimit::new_const("recent_normalized_messages", LimitUnit::Items, 12);
pub const RENDERED_CONTEXT_SCALARS: ResourceLimit =
    ResourceLimit::new_const("rendered_context", LimitUnit::UnicodeScalars, 12_000);
pub const MAX_EXPLICIT_REQUESTS: ResourceLimit =
    ResourceLimit::new_const("explicit_requests", LimitUnit::Items, 32);
pub const WIDE_EXCERPT_SCALARS: ResourceLimit =
    ResourceLimit::new_const("wide_excerpt", LimitUnit::UnicodeScalars, 160);
pub const RERANK_DESCRIPTION_SCALARS: ResourceLimit =
    ResourceLimit::new_const("rerank_description", LimitUnit::UnicodeScalars, 1_000);
pub const RERANK_BODY_EXCERPT_SCALARS: ResourceLimit =
    ResourceLimit::new_const("rerank_body_excerpt", LimitUnit::UnicodeScalars, 700);
pub const SERIALIZED_REQUEST_BYTES: ResourceLimit =
    ResourceLimit::new_const("serialized_request", LimitUnit::Bytes, 96 * KIB);
pub const DECODED_RESPONSE_BYTES: ResourceLimit =
    ResourceLimit::new_const("decoded_response", LimitUnit::Bytes, 2 * MIB);
pub const OBSERVATION_DELTA_BYTES: ResourceLimit =
    ResourceLimit::new_const("observation_delta", LimitUnit::Bytes, 8 * MIB);
pub const QUILL_QUERY_TERMS: ResourceLimit =
    ResourceLimit::new_const("quill_query_terms", LimitUnit::Items, 128);
pub const QUILL_QUERY_SCALARS: ResourceLimit =
    ResourceLimit::new_const("quill_query", LimitUnit::UnicodeScalars, 4_096);
pub const HOOK_ADDITIONAL_CONTEXT_SCALARS: ResourceLimit =
    ResourceLimit::new_const("hook_additional_context", LimitUnit::UnicodeScalars, 1_024);
pub const STATE_STORE_BYTES: ResourceLimit =
    ResourceLimit::new_const("ledger_sidecars_and_backups", LimitUnit::Bytes, 256 * MIB);
pub const CACHE_COORDINATOR_BYTES: ResourceLimit =
    ResourceLimit::new_const("cache_and_coordinator_state", LimitUnit::Bytes, 64 * MIB);

pub const ALL_DEFAULT_LIMITS: &[ResourceLimit] = &[
    HOOK_STDIN_BYTES,
    NORMALIZED_CONTEXT_JSON_BYTES,
    NORMALIZED_CONTEXT_DEPTH,
    EXPLICIT_ROSTER_JSON_BYTES,
    EXPLICIT_ROSTER_RECORDS,
    EXPLICIT_ROSTER_DEPTH,
    CONFIG_FILE_BYTES,
    CONFIG_FILE_DEPTH,
    YAML_DOCUMENT_DEPTH,
    YAML_ALIASES,
    SKILL_FILE_BYTES,
    SKILL_FRONTMATTER_BYTES,
    DISCOVERY_FILES,
    DISCOVERY_PARSED_BYTES,
    REPLAY_CASE_BYTES,
    REPLAY_CASE_DEPTH,
    REPLAY_POLICY_BYTES,
    REPLAY_POLICY_DEPTH,
    EVALUATION_DATASET_BYTES,
    EVALUATION_CASE_RECORDS,
    EVALUATION_DATASET_DEPTH,
    NATIVE_TRANSCRIPT_TAIL_BYTES,
    NATIVE_TRANSCRIPT_TAIL_RECORDS,
    ONE_TRANSCRIPT_RECORD_BYTES,
    CASS_STDOUT_BYTES,
    RECENT_NORMALIZED_MESSAGES,
    RENDERED_CONTEXT_SCALARS,
    MAX_EXPLICIT_REQUESTS,
    WIDE_EXCERPT_SCALARS,
    RERANK_DESCRIPTION_SCALARS,
    RERANK_BODY_EXCERPT_SCALARS,
    SERIALIZED_REQUEST_BYTES,
    DECODED_RESPONSE_BYTES,
    OBSERVATION_DELTA_BYTES,
    QUILL_QUERY_TERMS,
    QUILL_QUERY_SCALARS,
    HOOK_ADDITIONAL_CONTEXT_SCALARS,
    STATE_STORE_BYTES,
    CACHE_COORDINATOR_BYTES,
];

pub fn checked_add_usize(name: &'static str, lhs: usize, rhs: usize) -> Result<usize, LimitError> {
    lhs.checked_add(rhs)
        .ok_or(LimitError::ArithmeticOverflow { name })
}

pub fn checked_add_u64(name: &'static str, lhs: u64, rhs: u64) -> Result<u64, LimitError> {
    lhs.checked_add(rhs)
        .ok_or(LimitError::ArithmeticOverflow { name })
}

pub fn unicode_scalar_count(text: &str) -> usize {
    text.chars().count()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct MonotonicMillis(u64);

impl MonotonicMillis {
    pub const fn from_millis(ms: u64) -> Self {
        Self(ms)
    }
    pub const fn as_millis(self) -> u64 {
        self.0
    }

    pub fn checked_add(
        self,
        duration: DurationMillis,
        name: &'static str,
    ) -> Result<Self, LimitError> {
        checked_add_u64(name, self.0, duration.as_millis()).map(Self)
    }

    pub fn saturating_duration_since(self, earlier: Self) -> DurationMillis {
        DurationMillis(self.0.saturating_sub(earlier.0))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct WallClockMillis(i64);

impl WallClockMillis {
    pub const fn from_unix_millis(ms: i64) -> Self {
        Self(ms)
    }
    pub const fn as_unix_millis(self) -> i64 {
        self.0
    }

    pub fn checked_add(
        self,
        duration: DurationMillis,
        name: &'static str,
    ) -> Result<Self, LimitError> {
        let rhs = i64::try_from(duration.as_millis())
            .map_err(|_| LimitError::ArithmeticOverflow { name })?;
        self.0
            .checked_add(rhs)
            .map(Self)
            .ok_or(LimitError::ArithmeticOverflow { name })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct DurationMillis(u64);

impl DurationMillis {
    pub fn new(name: &'static str, ms: u64, max_ms: u64) -> Result<Self, LimitError> {
        BoundedU64::new(name, LimitUnit::Milliseconds, ms, max_ms)
            .map(|bounded| Self(bounded.get()))
    }

    const fn from_trusted_nonzero_millis(ms: u64) -> Self {
        assert!(ms > 0, "trusted duration constants must be nonzero");
        Self(ms)
    }
    pub const fn as_millis(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvocationDeadline {
    start: MonotonicMillis,
    total: DurationMillis,
    cleanup_reserve: DurationMillis,
}

impl InvocationDeadline {
    pub fn new(
        start: MonotonicMillis,
        total: DurationMillis,
        cleanup_reserve: DurationMillis,
    ) -> Result<Self, LimitError> {
        if total.as_millis() == 0 {
            return Err(LimitError::ZeroIsNotUnlimited {
                name: "invocation_deadline_total",
            });
        }
        if cleanup_reserve.as_millis() == 0 {
            return Err(LimitError::ZeroIsNotUnlimited {
                name: "cleanup_reserve",
            });
        }
        if cleanup_reserve.as_millis() >= total.as_millis() {
            return Err(LimitError::InvalidDeadlineReserve {
                total_ms: total.as_millis(),
                cleanup_reserve_ms: cleanup_reserve.as_millis(),
            });
        }
        // Prove end-time arithmetic at construction.
        start.checked_add(total, "invocation_deadline_end")?;
        Ok(Self {
            start,
            total,
            cleanup_reserve,
        })
    }

    pub fn default_from_start(start: MonotonicMillis) -> Result<Self, LimitError> {
        Self::new(
            start,
            DurationMillis::from_trusted_nonzero_millis(DEFAULT_INVOCATION_DEADLINE_MS),
            DurationMillis::from_trusted_nonzero_millis(DEFAULT_OUTPUT_CLEANUP_RESERVE_MS),
        )
    }

    pub const fn start(self) -> MonotonicMillis {
        self.start
    }
    pub const fn total(self) -> DurationMillis {
        self.total
    }
    pub const fn cleanup_reserve(self) -> DurationMillis {
        self.cleanup_reserve
    }

    pub fn expires_at(self) -> MonotonicMillis {
        self.start
            .checked_add(self.total, "invocation_deadline_end")
            .expect("validated deadline end")
    }

    pub fn latest_work_time(self) -> MonotonicMillis {
        MonotonicMillis(self.expires_at().as_millis() - self.cleanup_reserve.as_millis())
    }

    pub fn remaining_before_cleanup(self, now: MonotonicMillis) -> DurationMillis {
        self.latest_work_time().saturating_duration_since(now)
    }

    pub fn remaining_until_expiry(self, now: MonotonicMillis) -> DurationMillis {
        self.expires_at().saturating_duration_since(now)
    }

    pub fn is_in_cleanup_reserve(self, now: MonotonicMillis) -> bool {
        now >= self.latest_work_time() && now < self.expires_at()
    }

    pub fn is_expired(self, now: MonotonicMillis) -> bool {
        now >= self.expires_at()
    }

    pub fn ensure_can_start_work(
        self,
        now: MonotonicMillis,
        name: &'static str,
    ) -> Result<(), LimitError> {
        if now < self.start {
            return Err(LimitError::DeadlineBeforeStart {
                name,
                now_ms: now.as_millis(),
                start_ms: self.start.as_millis(),
            });
        }
        let expires_at = self.expires_at();
        if now >= expires_at {
            return Err(LimitError::DeadlineExpired {
                name,
                now_ms: now.as_millis(),
                expires_ms: expires_at.as_millis(),
            });
        }
        let latest_work = self.latest_work_time();
        if now >= latest_work {
            return Err(LimitError::DeadlineInCleanupReserve {
                name,
                now_ms: now.as_millis(),
                latest_work_ms: latest_work.as_millis(),
                expires_ms: expires_at.as_millis(),
            });
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WallClockExpiry {
    not_before: WallClockMillis,
    expires_at: WallClockMillis,
}

impl WallClockExpiry {
    pub fn new(not_before: WallClockMillis, ttl: DurationMillis) -> Result<Self, LimitError> {
        let expires_at = not_before.checked_add(ttl, "wall_clock_expiry")?;
        Ok(Self {
            not_before,
            expires_at,
        })
    }

    pub const fn not_before(self) -> WallClockMillis {
        self.not_before
    }
    pub const fn expires_at(self) -> WallClockMillis {
        self.expires_at
    }
    pub fn is_expired(self, now: WallClockMillis) -> bool {
        now >= self.expires_at
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AttemptLimits {
    logical_requests: u32,
    http_attempts: u32,
}

impl AttemptLimits {
    pub fn new(logical_requests: u32, http_attempts: u32) -> Result<Self, LimitError> {
        if logical_requests == 0 {
            return Err(LimitError::ZeroIsNotUnlimited {
                name: "logical_requests",
            });
        }
        if http_attempts == 0 {
            return Err(LimitError::ZeroIsNotUnlimited {
                name: "http_attempts",
            });
        }
        if http_attempts < logical_requests {
            return Err(LimitError::AttemptsBelowLogicalRequests {
                logical_requests,
                http_attempts,
            });
        }
        Ok(Self {
            logical_requests,
            http_attempts,
        })
    }

    pub const fn default_invocation() -> Self {
        Self {
            logical_requests: DEFAULT_LOGICAL_REQUESTS,
            http_attempts: DEFAULT_HTTP_ATTEMPTS,
        }
    }

    pub const fn logical_requests(self) -> u32 {
        self.logical_requests
    }
    pub const fn http_attempts(self) -> u32 {
        self.http_attempts
    }
}

/// Pure in-memory accounting snapshot for one invocation boundary.
///
/// This type is deliberately not `Copy` or `Clone`: later network admission code
/// must keep one mutable state value. Transitions mutate only after all checks
/// pass so denied admission preserves already spent usage for diagnostics. The
/// snapshot itself is not a provider-call capability and does not perform debit,
/// persistence, or network I/O.
#[derive(Debug, Eq, PartialEq)]
pub struct AttemptCounters {
    limits: AttemptLimits,
    logical_used: u32,
    attempts_used: u32,
}

impl AttemptCounters {
    pub const fn new(limits: AttemptLimits) -> Self {
        Self {
            limits,
            logical_used: 0,
            attempts_used: 0,
        }
    }

    pub const fn limits(&self) -> AttemptLimits {
        self.limits
    }
    pub const fn logical_used(&self) -> u32 {
        self.logical_used
    }
    pub const fn attempts_used(&self) -> u32 {
        self.attempts_used
    }

    pub fn begin_logical_request(&mut self) -> Result<(), LimitError> {
        if self.logical_used >= self.limits.logical_requests {
            return Err(LimitError::AboveLimit {
                name: "logical_requests",
                observed: self.logical_used as u128 + 1,
                limit: self.limits.logical_requests as u128,
                unit: LimitUnit::LogicalRequests,
            });
        }
        self.logical_used += 1;
        Ok(())
    }

    pub fn reserve_attempt(
        &mut self,
        now: MonotonicMillis,
        deadline: InvocationDeadline,
    ) -> Result<AttemptReceipt, LimitError> {
        if self.logical_used == 0 {
            return Err(LimitError::NoLogicalRequestForAttempt);
        }
        deadline.ensure_can_start_work(now, "http_attempt")?;
        if self.attempts_used >= self.limits.http_attempts {
            return Err(LimitError::AboveLimit {
                name: "http_attempts",
                observed: self.attempts_used as u128 + 1,
                limit: self.limits.http_attempts as u128,
                unit: LimitUnit::Attempts,
            });
        }
        let next_attempt_sequence = self.attempts_used + 1;
        let receipt = AttemptReceipt {
            logical_limit: self.limits.logical_requests,
            attempt_limit: self.limits.http_attempts,
            logical_used_at_reservation: self.logical_used,
            attempt_sequence: next_attempt_sequence,
            reserved_at: now,
            remaining_before_cleanup_ms: deadline.remaining_before_cleanup(now).as_millis(),
            remaining_until_expiry_ms: deadline.remaining_until_expiry(now).as_millis(),
        };
        self.attempts_used = next_attempt_sequence;
        Ok(receipt)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AttemptReceipt {
    logical_limit: u32,
    attempt_limit: u32,
    logical_used_at_reservation: u32,
    attempt_sequence: u32,
    reserved_at: MonotonicMillis,
    remaining_before_cleanup_ms: u64,
    remaining_until_expiry_ms: u64,
}

impl AttemptReceipt {
    pub const fn logical_limit(self) -> u32 {
        self.logical_limit
    }
    pub const fn attempt_limit(self) -> u32 {
        self.attempt_limit
    }
    pub const fn logical_used_at_reservation(self) -> u32 {
        self.logical_used_at_reservation
    }
    pub const fn attempt_sequence(self) -> u32 {
        self.attempt_sequence
    }
    pub const fn reserved_at(self) -> MonotonicMillis {
        self.reserved_at
    }
    pub const fn remaining_before_cleanup_ms(self) -> u64 {
        self.remaining_before_cleanup_ms
    }
    pub const fn remaining_until_expiry_ms(self) -> u64 {
        self.remaining_until_expiry_ms
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BatchNetworkMode {
    ReplayOnlyZeroNetwork,
    LiveWithExplicitRequestCap { max_http_attempts: u32 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BatchBounds {
    start: MonotonicMillis,
    max_runtime: DurationMillis,
    network_mode: BatchNetworkMode,
}

impl BatchBounds {
    pub fn replay_default(start: MonotonicMillis) -> Result<Self, LimitError> {
        let bounds = Self {
            start,
            max_runtime: DurationMillis::from_trusted_nonzero_millis(DEFAULT_EVAL_BATCH_RUNTIME_MS),
            network_mode: BatchNetworkMode::ReplayOnlyZeroNetwork,
        };
        bounds.expires_at()?;
        Ok(bounds)
    }

    pub fn live(
        start: MonotonicMillis,
        max_runtime: DurationMillis,
        max_http_attempts: u32,
    ) -> Result<Self, LimitError> {
        if max_http_attempts == 0 {
            return Err(LimitError::MissingLiveBatchRequestCap);
        }
        let bounds = Self {
            start,
            max_runtime,
            network_mode: BatchNetworkMode::LiveWithExplicitRequestCap { max_http_attempts },
        };
        bounds.expires_at()?;
        Ok(bounds)
    }

    pub const fn start(self) -> MonotonicMillis {
        self.start
    }
    pub const fn max_runtime(self) -> DurationMillis {
        self.max_runtime
    }
    pub const fn network_mode(self) -> BatchNetworkMode {
        self.network_mode
    }

    pub fn expires_at(self) -> Result<MonotonicMillis, LimitError> {
        self.start
            .checked_add(self.max_runtime, "batch_deadline_end")
    }

    pub fn per_case_deadline_at(
        self,
        case_start: MonotonicMillis,
    ) -> Result<InvocationDeadline, LimitError> {
        if case_start < self.start {
            return Err(LimitError::DeadlineBeforeStart {
                name: "batch_case",
                now_ms: case_start.as_millis(),
                start_ms: self.start.as_millis(),
            });
        }
        let batch_end = self.expires_at()?;
        if case_start >= batch_end {
            return Err(LimitError::DeadlineExpired {
                name: "batch_case",
                now_ms: case_start.as_millis(),
                expires_ms: batch_end.as_millis(),
            });
        }
        let remaining_ms = batch_end.as_millis() - case_start.as_millis();
        if remaining_ms <= DEFAULT_OUTPUT_CLEANUP_RESERVE_MS {
            return Err(LimitError::DeadlineInCleanupReserve {
                name: "batch_case",
                now_ms: case_start.as_millis(),
                latest_work_ms: batch_end
                    .as_millis()
                    .saturating_sub(DEFAULT_OUTPUT_CLEANUP_RESERVE_MS)
                    .max(self.start.as_millis()),
                expires_ms: batch_end.as_millis(),
            });
        }
        let total_ms = remaining_ms.min(DEFAULT_INVOCATION_DEADLINE_MS);
        InvocationDeadline::new(
            case_start,
            DurationMillis::new("batch_case_total", total_ms, DEFAULT_INVOCATION_DEADLINE_MS)?,
            DurationMillis::from_trusted_nonzero_millis(DEFAULT_OUTPUT_CLEANUP_RESERVE_MS),
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MaintenanceBounds {
    sqlite_busy_wait: DurationMillis,
    state_store: ResourceLimit,
    cache_and_coordinator: ResourceLimit,
}

impl MaintenanceBounds {
    pub const fn defaults() -> Self {
        Self {
            sqlite_busy_wait: DurationMillis::from_trusted_nonzero_millis(
                DEFAULT_SQLITE_BUSY_WAIT_MS,
            ),
            state_store: STATE_STORE_BYTES,
            cache_and_coordinator: CACHE_COORDINATOR_BYTES,
        }
    }

    pub const fn sqlite_busy_wait(self) -> DurationMillis {
        self.sqlite_busy_wait
    }
    pub const fn state_store(self) -> ResourceLimit {
        self.state_store
    }
    pub const fn cache_and_coordinator(self) -> ResourceLimit {
        self.cache_and_coordinator
    }
}
