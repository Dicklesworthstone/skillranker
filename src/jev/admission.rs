//! Invocation-wide provider attempt admission, permits, and cost receipts.
//!
//! Under the SkillRanker contract (I03, `sr-roadmap-l1i.2.8`), fresh calls to
//! TypeSafe's Jev inference service are strictly bounded:
//! - Default budget: 2 logical requests (Stage 1 Wide, Stage 2 Rerank) and at most
//!   4 HTTP attempts total across retries.
//! - Single admission seam: every attempt (including retries, evaluation batches,
//!   and breaker probes) must acquire a single-use [`AttemptPermit`] before transmission.
//! - Single-use permits: each permit binds an [`AttemptId`], target origin, guard
//!   generation, monotonic deadline, and stage. A permit is consumed at most once.
//! - Known vs. unknown usage: when a response arrives, exact returned token counts are
//!   accumulated into [`CostReceipt`]. If an attempt times out, cancels, or fails
//!   *after* bytes are put on the wire, an unknown-usage marker is recorded.
//!   Unknown provider cost is never counted as zero.
//! - Cancellation before send: permits discarded before network transmission consume
//!   an attempt slot but record zero unknown tokens.
//! - Cache zero-cost path: exact cache hits bypass provider admission entirely and
//!   produce a [`CostReceipt::zero_cost_cache_hit`].
//! - Stage exhaustion: if attempt budget is exhausted before or during rerank,
//!   rerank admission is refused. The wide winner is never promoted as rerank.
//! - Operational refusal: budget/breaker refusals return [`AdmissionRefusal`] mapped to
//!   `unavailable / request-budget` (exit code 4), distinct from relevance abstentions.

use crate::jev::CanonicalOrigin;
use crate::jev::codec::Usage;
use crate::limits::{DEFAULT_HTTP_ATTEMPTS, DEFAULT_LOGICAL_REQUESTS, LimitError, MonotonicMillis};
use crate::output::{CliExit, ErrorKind};
use crate::runtime::EntryClock;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fmt;

/// Minimum remaining deadline (before cleanup reserve) required to admit a provider attempt.
pub const DEFAULT_MIN_ATTEMPT_RESERVE_MS: u64 = 50;

/// Default generation for process-local attempt admission before shared coordination.
pub const DEFAULT_GUARD_GENERATION: u64 = 1;

/// Distinct ranking stages in the Jev recommendation pipeline.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RankingStage {
    /// Stage 1: Broad prefilter/gate choice over admitted candidates.
    Wide,
    /// Stage 2: Detailed rerank over the shortlist candidates.
    Rerank,
    /// Optional half-open provider circuit breaker health probe.
    Probe,
    /// Live evaluation batch request.
    Evaluation,
}

impl RankingStage {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Wide => "wide",
            Self::Rerank => "rerank",
            Self::Probe => "probe",
            Self::Evaluation => "evaluation",
        }
    }

    pub const fn is_wide(self) -> bool {
        matches!(self, Self::Wide)
    }

    pub const fn is_rerank(self) -> bool {
        matches!(self, Self::Rerank)
    }
}

impl fmt::Display for RankingStage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Unique identifier for an admitted HTTP attempt.
///
/// Within an invocation, attempt IDs are guaranteed unique and monotonically sequence-tracked.
#[derive(Clone, Eq, PartialEq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
pub struct AttemptId(String);

impl AttemptId {
    /// Create a new validated attempt ID.
    pub fn parse(value: impl Into<String>) -> Result<Self, AdmissionError> {
        let text = value.into();
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Err(AdmissionError::InvalidAttemptId(
                "attempt ID cannot be empty".to_owned(),
            ));
        }
        if trimmed.len() > crate::identity::MAX_ID_BYTES {
            return Err(AdmissionError::InvalidAttemptId(
                "attempt ID exceeds maximum byte limit".to_owned(),
            ));
        }
        if trimmed.chars().any(char::is_control) {
            return Err(AdmissionError::InvalidAttemptId(
                "attempt ID cannot contain control characters".to_owned(),
            ));
        }
        Ok(Self(trimmed.to_owned()))
    }

    /// Create a sequential attempt ID scoped to an invocation identifier.
    pub fn new_sequential(invocation_id: &str, sequence: u32) -> Self {
        Self(format!("{invocation_id}-att-{sequence}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AttemptId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for AttemptId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("AttemptId").field(&self.0).finish()
    }
}

/// Invocation-wide attempt budget limits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AttemptBudget {
    max_logical_requests: u32,
    max_http_attempts: u32,
    min_attempt_reserve_ms: u64,
}

impl AttemptBudget {
    /// Create a new validated attempt budget.
    pub fn new(max_logical_requests: u32, max_http_attempts: u32) -> Result<Self, AdmissionError> {
        if max_logical_requests == 0 {
            return Err(AdmissionError::InvalidBudget(
                LimitError::ZeroIsNotUnlimited {
                    name: "max_logical_requests",
                },
            ));
        }
        if max_http_attempts == 0 {
            return Err(AdmissionError::InvalidBudget(
                LimitError::ZeroIsNotUnlimited {
                    name: "max_http_attempts",
                },
            ));
        }
        if max_http_attempts < max_logical_requests {
            return Err(AdmissionError::InvalidBudget(
                LimitError::AttemptsBelowLogicalRequests {
                    logical_requests: max_logical_requests,
                    http_attempts: max_http_attempts,
                },
            ));
        }
        Ok(Self {
            max_logical_requests,
            max_http_attempts,
            min_attempt_reserve_ms: DEFAULT_MIN_ATTEMPT_RESERVE_MS,
        })
    }

    /// Default configuration: 2 logical requests (Wide + Rerank), 4 total HTTP attempts.
    pub const fn default_invocation() -> Self {
        Self {
            max_logical_requests: DEFAULT_LOGICAL_REQUESTS,
            max_http_attempts: DEFAULT_HTTP_ATTEMPTS,
            min_attempt_reserve_ms: DEFAULT_MIN_ATTEMPT_RESERVE_MS,
        }
    }

    /// Set minimum time margin (in milliseconds) required before the cleanup reserve.
    pub const fn with_min_reserve_ms(mut self, ms: u64) -> Self {
        self.min_attempt_reserve_ms = ms;
        self
    }

    pub const fn max_logical_requests(&self) -> u32 {
        self.max_logical_requests
    }

    pub const fn max_http_attempts(&self) -> u32 {
        self.max_http_attempts
    }

    pub const fn min_attempt_reserve_ms(&self) -> u64 {
        self.min_attempt_reserve_ms
    }
}

impl Default for AttemptBudget {
    fn default() -> Self {
        Self::default_invocation()
    }
}

/// Operational admission refusal reasons, distinct from relevance abstentions.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AdmissionRefusal {
    /// Total HTTP attempts budget exhausted for this invocation.
    AttemptsExhausted { attempts_used: u32, limit: u32 },
    /// Logical requests cap exhausted (e.g. attempting a 3rd logical stage).
    LogicalRequestsExhausted { logical_used: u32, limit: u32 },
    /// Time remaining before cleanup reserve is insufficient to admit an attempt.
    InsufficientDeadline { remaining_ms: u64, required_ms: u64 },
    /// Stage ordering invariant violated (e.g. attempting Rerank without Wide).
    StageOrderingViolation {
        stage: RankingStage,
        reason: &'static str,
    },
    /// Circuit breaker is in cooldown after repeated transient provider failures.
    ProviderCooldown { cooldown_ms: u64 },
    /// Shared accounting or budget coordination state is unusable.
    BudgetStateUnavailable,
    /// Duplicate attempt ID detected; attempt IDs must be strictly unique.
    DuplicateAttemptId { attempt_id: String },
}

impl AdmissionRefusal {
    pub const fn error_kind(&self) -> ErrorKind {
        match self {
            Self::AttemptsExhausted { .. } | Self::LogicalRequestsExhausted { .. } => {
                ErrorKind::RequestBudget
            }
            Self::InsufficientDeadline { .. } => ErrorKind::Timeout,
            Self::StageOrderingViolation { .. } | Self::DuplicateAttemptId { .. } => {
                ErrorKind::InvalidUsage
            }
            Self::ProviderCooldown { .. } => ErrorKind::ProviderCooldown,
            Self::BudgetStateUnavailable => ErrorKind::BudgetState,
        }
    }

    pub const fn exit_code(&self) -> CliExit {
        self.error_kind().exit_code()
    }
}

impl fmt::Display for AdmissionRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AttemptsExhausted {
                attempts_used,
                limit,
            } => {
                write!(
                    f,
                    "HTTP attempt budget exhausted ({attempts_used}/{limit} attempts used)"
                )
            }
            Self::LogicalRequestsExhausted {
                logical_used,
                limit,
            } => {
                write!(
                    f,
                    "logical requests budget exhausted ({logical_used}/{limit} requests used)"
                )
            }
            Self::InsufficientDeadline {
                remaining_ms,
                required_ms,
            } => {
                write!(
                    f,
                    "insufficient deadline to admit provider attempt ({remaining_ms}ms remaining, {required_ms}ms required)"
                )
            }
            Self::StageOrderingViolation { stage, reason } => {
                write!(f, "stage ordering violation for {stage}: {reason}")
            }
            Self::ProviderCooldown { cooldown_ms } => {
                write!(
                    f,
                    "provider circuit breaker active; in cooldown for {cooldown_ms}ms"
                )
            }
            Self::BudgetStateUnavailable => {
                f.write_str("budget accounting state is unavailable or corrupt")
            }
            Self::DuplicateAttemptId { attempt_id } => {
                write!(f, "duplicate attempt ID rejected: {attempt_id}")
            }
        }
    }
}

impl std::error::Error for AdmissionRefusal {}

/// Errors during attempt permit consumption or receipt tracking.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AdmissionError {
    PermitAlreadyConsumed,
    AttemptNotActive,
    InvalidAttemptId(String),
    InvalidBudget(LimitError),
}

impl fmt::Display for AdmissionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PermitAlreadyConsumed => f.write_str("attempt permit was already consumed"),
            Self::AttemptNotActive => {
                f.write_str("attempt was not found or is no longer in-flight")
            }
            Self::InvalidAttemptId(reason) => write!(f, "invalid attempt ID: {reason}"),
            Self::InvalidBudget(err) => write!(f, "invalid attempt budget: {err}"),
        }
    }
}

impl std::error::Error for AdmissionError {}

/// Single-use permit granting authorization to execute one HTTP attempt.
///
/// Binds identity, origin, stage, generation, and deadline.
/// Consumed at most once via [`AttemptPermit::mark_sent`] or [`AttemptPermit::discard_before_send`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AttemptPermit {
    attempt_id: AttemptId,
    stage: RankingStage,
    endpoint: CanonicalOrigin,
    guard_generation: u64,
    admitted_at: MonotonicMillis,
    deadline_remaining_ms: u64,
    consumed: bool,
}

impl AttemptPermit {
    pub fn attempt_id(&self) -> &AttemptId {
        &self.attempt_id
    }

    pub const fn stage(&self) -> RankingStage {
        self.stage
    }

    pub const fn endpoint(&self) -> &CanonicalOrigin {
        &self.endpoint
    }

    pub const fn guard_generation(&self) -> u64 {
        self.guard_generation
    }

    pub const fn admitted_at(&self) -> MonotonicMillis {
        self.admitted_at
    }

    pub const fn deadline_remaining_ms(&self) -> u64 {
        self.deadline_remaining_ms
    }

    pub const fn is_consumed(&self) -> bool {
        self.consumed
    }

    /// Mark the attempt as transmitted across the wire.
    ///
    /// Consumes the permit. Once sent, if the attempt fails, times out, or cancels,
    /// it MUST be counted as unknown usage because provider execution may have started.
    pub fn mark_sent(mut self) -> Result<SentAttempt, AdmissionError> {
        if self.consumed {
            return Err(AdmissionError::PermitAlreadyConsumed);
        }
        self.consumed = true;
        Ok(SentAttempt {
            attempt_id: self.attempt_id.clone(),
            stage: self.stage,
            sent_at: self.admitted_at,
        })
    }

    /// Discard the permit before sending (e.g. cancellation, preflight check failure).
    ///
    /// Consumes the permit and attempt slot, but records NO unknown token cost since
    /// bytes were never sent across the wire.
    pub fn discard_before_send(mut self, reason: impl Into<String>) -> DiscardedAttempt {
        self.consumed = true;
        DiscardedAttempt {
            attempt_id: self.attempt_id.clone(),
            stage: self.stage,
            reason: reason.into(),
        }
    }
}

impl Drop for AttemptPermit {
    fn drop(&mut self) {
        // If dropped without mark_sent or discard_before_send, the permit slot is simply released.
        self.consumed = true;
    }
}

/// An attempt that was sent across the network.
///
/// Must be concluded with either a known response or a terminal failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SentAttempt {
    attempt_id: AttemptId,
    stage: RankingStage,
    sent_at: MonotonicMillis,
}

impl SentAttempt {
    pub fn attempt_id(&self) -> &AttemptId {
        &self.attempt_id
    }

    pub const fn stage(&self) -> RankingStage {
        self.stage
    }

    pub const fn sent_at(&self) -> MonotonicMillis {
        self.sent_at
    }
}

/// An attempt that was admitted but cancelled/discarded before bytes reached the wire.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscardedAttempt {
    attempt_id: AttemptId,
    stage: RankingStage,
    reason: String,
}

impl DiscardedAttempt {
    pub fn attempt_id(&self) -> &AttemptId {
        &self.attempt_id
    }

    pub const fn stage(&self) -> RankingStage {
        self.stage
    }

    pub fn reason(&self) -> &str {
        &self.reason
    }
}

/// Cost and token usage receipt for one invocation.
///
/// Preserves exact counts of admitted, sent, and completed attempts, known tokens,
/// and unknown-usage attempts. Exact cache hits report zero new provider calls.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CostReceipt {
    pub admitted_attempts: u32,
    pub sent_attempts: u32,
    pub completed_attempts: u32,
    pub known_usage: Usage,
    pub unknown_usage_attempts: u32,
    pub has_unknown_usage: bool,
    pub cache_served: bool,
}

impl CostReceipt {
    /// Create a zero-cost receipt for an exact cache hit.
    ///
    /// Cache hits make zero new network attempts and consume zero provider tokens.
    pub const fn zero_cost_cache_hit() -> Self {
        Self {
            admitted_attempts: 0,
            sent_attempts: 0,
            completed_attempts: 0,
            known_usage: Usage {
                input_tokens: 0,
                output_tokens: 0,
            },
            unknown_usage_attempts: 0,
            has_unknown_usage: false,
            cache_served: true,
        }
    }

    /// Create an initial empty receipt for a fresh live invocation.
    pub const fn new() -> Self {
        Self {
            admitted_attempts: 0,
            sent_attempts: 0,
            completed_attempts: 0,
            known_usage: Usage {
                input_tokens: 0,
                output_tokens: 0,
            },
            unknown_usage_attempts: 0,
            has_unknown_usage: false,
            cache_served: false,
        }
    }

    /// Record a successfully completed attempt with validated provider usage.
    pub fn record_success(&mut self, usage: Usage) {
        self.completed_attempts = self.completed_attempts.saturating_add(1);
        self.known_usage.input_tokens = self
            .known_usage
            .input_tokens
            .saturating_add(usage.input_tokens);
        self.known_usage.output_tokens = self
            .known_usage
            .output_tokens
            .saturating_add(usage.output_tokens);
    }

    /// Record a terminal failure on an in-flight attempt that was already sent.
    ///
    /// Preserves all previously accumulated known usage and adds an unknown-usage marker.
    pub fn record_terminal_error(&mut self) {
        self.unknown_usage_attempts = self.unknown_usage_attempts.saturating_add(1);
        self.has_unknown_usage = true;
    }

    /// Total known tokens (input + output).
    pub const fn total_tokens(&self) -> u64 {
        self.known_usage
            .input_tokens
            .saturating_add(self.known_usage.output_tokens)
    }

    /// Returns true if no attempts were ever admitted.
    pub const fn is_empty(&self) -> bool {
        self.admitted_attempts == 0
    }
}

impl Default for CostReceipt {
    fn default() -> Self {
        Self::new()
    }
}

/// Invocation-wide coordinator for HTTP attempt admission and cost accounting.
///
/// Implements the single admission seam across all pipeline stages, enforcing:
/// - Max logical requests and HTTP attempt ceilings.
/// - Monotonic deadline and cleanup reserve preservation.
/// - Strict stage ordering (Wide before Rerank).
/// - Unique attempt IDs and single-use permit tracking.
/// - Known vs. unknown cost accounting on every completion or terminal failure.
pub struct AttemptAdmission {
    budget: AttemptBudget,
    clock: EntryClock,
    invocation_id: String,
    guard_generation: u64,
    receipt: CostReceipt,
    logical_requests_started: u32,
    current_stage: Option<RankingStage>,
    wide_completed: bool,
    issued_attempt_ids: BTreeSet<String>,
    has_active_permit: bool,
}

impl AttemptAdmission {
    /// Create a new attempt admission coordinator with custom budget.
    pub fn new(
        budget: AttemptBudget,
        clock: EntryClock,
        invocation_id: impl Into<String>,
    ) -> Result<Self, AdmissionError> {
        let invocation_id = invocation_id.into();
        if invocation_id.trim().is_empty() {
            return Err(AdmissionError::InvalidAttemptId(
                "invocation ID cannot be empty".to_owned(),
            ));
        }
        Ok(Self {
            budget,
            clock,
            invocation_id,
            guard_generation: DEFAULT_GUARD_GENERATION,
            receipt: CostReceipt::new(),
            logical_requests_started: 0,
            current_stage: None,
            wide_completed: false,
            issued_attempt_ids: BTreeSet::new(),
            has_active_permit: false,
        })
    }

    /// Create default coordinator: 2 logical requests, 4 HTTP attempts.
    pub fn default_invocation(clock: EntryClock, invocation_id: impl Into<String>) -> Self {
        Self::new(AttemptBudget::default_invocation(), clock, invocation_id)
            .expect("default budget and valid invocation ID must succeed")
    }

    pub const fn budget(&self) -> AttemptBudget {
        self.budget
    }

    pub const fn receipt(&self) -> &CostReceipt {
        &self.receipt
    }

    pub const fn guard_generation(&self) -> u64 {
        self.guard_generation
    }

    pub fn set_guard_generation(&mut self, generation: u64) {
        self.guard_generation = generation;
    }

    pub const fn remaining_http_attempts(&self) -> u32 {
        self.budget
            .max_http_attempts
            .saturating_sub(self.receipt.admitted_attempts)
    }

    pub const fn remaining_logical_requests(&self) -> u32 {
        self.budget
            .max_logical_requests
            .saturating_sub(self.logical_requests_started)
    }

    pub const fn is_wide_completed(&self) -> bool {
        self.wide_completed
    }

    /// Check if rerank stage can be admitted under current budget and stage status.
    pub const fn can_attempt_rerank(&self) -> bool {
        self.wide_completed
            && self.remaining_http_attempts() > 0
            && self.remaining_logical_requests() > 0
    }

    /// Request admission for a provider HTTP attempt.
    ///
    /// Validates deadlines, stage ordering, and attempt ceilings before issuing a single-use permit.
    pub fn admit(
        &mut self,
        stage: RankingStage,
        endpoint: &CanonicalOrigin,
    ) -> Result<AttemptPermit, AdmissionRefusal> {
        // 1. Check monotonic entry clock and cleanup reserve
        let now =
            self.clock
                .admit_new_work()
                .map_err(|_| AdmissionRefusal::InsufficientDeadline {
                    remaining_ms: 0,
                    required_ms: self.budget.min_attempt_reserve_ms,
                })?;
        let remaining_before_cleanup = self.clock.remaining_before_cleanup().as_millis();
        if remaining_before_cleanup < self.budget.min_attempt_reserve_ms {
            return Err(AdmissionRefusal::InsufficientDeadline {
                remaining_ms: remaining_before_cleanup,
                required_ms: self.budget.min_attempt_reserve_ms,
            });
        }

        // 2. Stage progression check: Rerank requires Wide to have completed successfully
        if stage == RankingStage::Rerank && !self.wide_completed {
            return Err(AdmissionRefusal::StageOrderingViolation {
                stage,
                reason: "rerank stage requires successful wide stage completion first",
            });
        }

        // 3. Logical request tracking
        let is_new_logical_stage = match self.current_stage {
            Some(curr) => curr != stage,
            None => true,
        };
        if is_new_logical_stage {
            if self.logical_requests_started >= self.budget.max_logical_requests {
                return Err(AdmissionRefusal::LogicalRequestsExhausted {
                    logical_used: self.logical_requests_started,
                    limit: self.budget.max_logical_requests,
                });
            }
            self.logical_requests_started = self.logical_requests_started.saturating_add(1);
            self.current_stage = Some(stage);
        }

        // 4. HTTP attempt limit check
        if self.receipt.admitted_attempts >= self.budget.max_http_attempts {
            return Err(AdmissionRefusal::AttemptsExhausted {
                attempts_used: self.receipt.admitted_attempts,
                limit: self.budget.max_http_attempts,
            });
        }

        // 5. Generate and register unique attempt ID
        let sequence = self.receipt.admitted_attempts.saturating_add(1);
        let attempt_id = AttemptId::new_sequential(&self.invocation_id, sequence);
        if self.issued_attempt_ids.contains(attempt_id.as_str()) {
            return Err(AdmissionRefusal::DuplicateAttemptId {
                attempt_id: attempt_id.as_str().to_owned(),
            });
        }
        self.issued_attempt_ids
            .insert(attempt_id.as_str().to_owned());

        // 6. Update accounting
        self.receipt.admitted_attempts = sequence;
        self.has_active_permit = true;

        Ok(AttemptPermit {
            attempt_id,
            stage,
            endpoint: endpoint.clone(),
            guard_generation: self.guard_generation,
            admitted_at: now,
            deadline_remaining_ms: remaining_before_cleanup,
            consumed: false,
        })
    }

    /// Record that an admitted permit was sent across the wire.
    pub fn record_sent(&mut self, sent: &SentAttempt) -> Result<(), AdmissionError> {
        if !self.issued_attempt_ids.contains(sent.attempt_id.as_str()) {
            return Err(AdmissionError::AttemptNotActive);
        }
        self.receipt.sent_attempts = self.receipt.sent_attempts.saturating_add(1);
        self.has_active_permit = false;
        Ok(())
    }

    /// Record a successful response with known usage tokens.
    pub fn record_response(
        &mut self,
        sent: &SentAttempt,
        usage: Usage,
    ) -> Result<(), AdmissionError> {
        if !self.issued_attempt_ids.contains(sent.attempt_id.as_str()) {
            return Err(AdmissionError::AttemptNotActive);
        }
        self.receipt.record_success(usage);
        if sent.stage.is_wide() {
            self.wide_completed = true;
        }
        self.has_active_permit = false;
        Ok(())
    }

    /// Record a terminal failure on an in-flight attempt that was already sent.
    ///
    /// Preserves all previously accumulated known tokens and adds an unknown-usage marker.
    pub fn record_terminal_failure(
        &mut self,
        sent: &SentAttempt,
        _reason: &str,
    ) -> Result<(), AdmissionError> {
        if !self.issued_attempt_ids.contains(sent.attempt_id.as_str()) {
            return Err(AdmissionError::AttemptNotActive);
        }
        self.receipt.record_terminal_error();
        self.has_active_permit = false;
        Ok(())
    }

    /// Record an attempt that was admitted but cancelled/discarded before bytes reached the wire.
    pub fn record_discard(&mut self, discarded: &DiscardedAttempt) -> Result<(), AdmissionError> {
        if !self
            .issued_attempt_ids
            .contains(discarded.attempt_id.as_str())
        {
            return Err(AdmissionError::AttemptNotActive);
        }
        self.has_active_permit = false;
        Ok(())
    }

    /// Record an exact cache hit: zero new attempts and zero new tokens.
    pub fn record_cache_hit(&mut self) {
        self.receipt = CostReceipt::zero_cost_cache_hit();
    }

    /// Extract final cost receipt.
    pub fn into_receipt(self) -> CostReceipt {
        self.receipt
    }
}
