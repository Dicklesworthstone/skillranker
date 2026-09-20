//! Explicit, sequential retries sharing the invocation's clock and allowance.
//!
//! This boundary owns no background work or persistent cooldown. Every actual
//! attempt rechecks caller authorization and the single-attempt client's policy.

use super::CanonicalOrigin;
use super::OriginScopedCredential;
use super::admission::{
    AdmissionError, AdmissionRefusal, AttemptAdmission, AttemptBudget, AttemptFailure,
    AttemptPermit, AttemptProvenance, CostReceipt, RankingStage, SentAttempt,
};
use super::client::{JevTransport, TransportError, TransportErrorKind};
use super::codec::{Request, Response, Usage};
use crate::privacy::NetworkConsent;
use crate::runtime::EntryClock;
use asupersync::Cx;
use std::fmt;
use std::time::{Duration, Instant, SystemTime};

/// No raw provider header text survives parsing.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum RetryAfter {
    #[default]
    Absent,
    Delay(Duration),
    /// Invalid, duplicated or oversized metadata cannot authorize an early retry.
    Invalid,
}

impl RetryAfter {
    pub(crate) fn from_headers(headers: &[(String, String)], received: SystemTime) -> Self {
        let mut values = headers
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case("retry-after"));
        match (values.next(), values.next()) {
            (None, _) => Self::Absent,
            (Some((_, value)), None) => Self::parse(value, received),
            _ => Self::Invalid,
        }
    }

    /// Delta-seconds or HTTP-date, converted once at receipt into a monotonic
    /// wait. Wall-clock changes during the subsequent wait cannot shorten it.
    pub fn parse(value: &str, received: SystemTime) -> Self {
        if value.len() > 128 {
            return Self::Invalid;
        }
        let value = value.trim_matches([' ', '\t']);
        if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
            return Self::Delay(
                value
                    .parse::<u64>()
                    .map(Duration::from_secs)
                    .unwrap_or(Duration::MAX),
            );
        }
        httpdate::parse_http_date(value)
            .map(|at| Self::Delay(at.duration_since(received).unwrap_or(Duration::ZERO)))
            .unwrap_or(Self::Invalid)
    }
}

/// Only these closed failures are transient. TLS/authentication, malformed
/// protocol/answers, input errors, cancellation and deadline exhaustion stop.
pub const fn retryable(kind: TransportErrorKind) -> bool {
    matches!(
        kind,
        TransportErrorKind::Dns
            | TransportErrorKind::Connect
            | TransportErrorKind::TransientIo
            | TransportErrorKind::HttpStatus(429 | 500 | 502 | 503 | 504 | 529)
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RetryStop {
    NotTransient,
    InvalidRetryAfter,
    DoesNotFitDeadline,
}

/// Pure scheduling decision: callers/tests supply the remaining monotonic
/// budget and entropy. Never truncate a provider delay to fit the deadline.
pub fn retry_delay(
    error: &TransportError,
    failures: u32,
    remaining: Duration,
    minimum_attempt: Duration,
    entropy: u64,
) -> Result<Duration, RetryStop> {
    if !retryable(error.kind) {
        return Err(RetryStop::NotTransient);
    }
    let provider = match error.retry_after {
        RetryAfter::Absent => Duration::ZERO,
        RetryAfter::Delay(delay) => delay,
        RetryAfter::Invalid => return Err(RetryStop::InvalidRetryAfter),
    };
    let backoff = Duration::from_millis(100_u64 << failures.saturating_sub(1).min(3));
    let delay = provider
        .max(backoff)
        .checked_add(Duration::from_millis(entropy % 51))
        .ok_or(RetryStop::DoesNotFitDeadline)?;
    if delay
        .checked_add(minimum_attempt)
        .is_none_or(|needed| needed >= remaining)
    {
        return Err(RetryStop::DoesNotFitDeadline);
    }
    Ok(delay)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RetryErrorKind {
    Transport(TransportErrorKind),
    Admission(AdmissionRefusal),
    Accounting,
    PolicyChanged,
    RetryStopped(RetryStop),
    ModelPairMismatch,
}

pub struct RetryError {
    pub kind: RetryErrorKind,
    /// Cumulative, including a successful wide call when rerank fails.
    pub receipt: CostReceipt,
    pub last_transport: Option<TransportError>,
}

impl fmt::Debug for RetryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Admission errors can contain local invocation IDs; keep diagnostics
        // free of those, as well as raw model IDs, credentials and bodies.
        let kind = match self.kind {
            RetryErrorKind::Transport(_) => "transport",
            RetryErrorKind::Admission(_) => "admission",
            RetryErrorKind::Accounting => "accounting",
            RetryErrorKind::PolicyChanged => "policy-changed",
            RetryErrorKind::RetryStopped(_) => "retry-stopped",
            RetryErrorKind::ModelPairMismatch => "model-pair-mismatch",
        };
        f.debug_struct("RetryError")
            .field("kind", &kind)
            .field("receipt", &self.receipt)
            .finish()
    }
}
impl fmt::Display for RetryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Jev stage failed: {self:?}")
    }
}
impl std::error::Error for RetryError {}

/// Identities are evidence from this response, never inferred immutable pins.
#[derive(Clone)]
pub struct ModelObservation {
    pub requested: String,
    pub returned: String,
    pub started_at: SystemTime,
    pub received_at: SystemTime,
    pub started_elapsed_ms: u64,
    pub received_elapsed_ms: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelPairStatus {
    NotPaired,
    /// Matching returned aliases do not prove an atomic model snapshot.
    MatchingUnpinnedIdentifiers,
}

pub struct StageResponse {
    pub response: Response,
    pub model: ModelObservation,
    pub pair_status: ModelPairStatus,
    pub attempts: u32,
    pub receipt: CostReceipt,
}

/// One invocation, one client/origin, one clock, one attempt allowance across
/// stages. This type does not create a durable guard or evaluation batch
/// allowance; those require a separate admission integration. An in-memory
/// transport without an origin is scoped to the production origin for
/// allowance bookkeeping only; it never receives a credential.
pub struct RetrySession<'a> {
    client: &'a dyn JevTransport,
    origin: CanonicalOrigin,
    credential: Option<&'a OriginScopedCredential>,
    clock: EntryClock,
    admission: AttemptAdmission,
    wide: Option<ModelObservation>,
    last_model: Option<ModelObservation>,
}

impl<'a> RetrySession<'a> {
    pub fn new(
        client: &'a dyn JevTransport,
        credential: Option<&'a OriginScopedCredential>,
        clock: EntryClock,
        budget: AttemptBudget,
        invocation_id: impl Into<String>,
    ) -> Result<Self, AdmissionError> {
        Ok(Self {
            client,
            origin: client
                .origin()
                .cloned()
                .unwrap_or_else(CanonicalOrigin::production),
            // A transport without a real origin never receives a credential,
            // whatever the caller passed.
            credential: client.origin().and(credential),
            clock,
            admission: AttemptAdmission::new(budget, clock, invocation_id)?,
            wide: None,
            last_model: None,
        })
    }

    pub fn receipt(&self) -> CostReceipt {
        *self.admission.receipt()
    }

    /// Every attempt this session admitted, with what became of it. The receipt's
    /// invocation-wide counters cannot say which attempt incurred a given spend,
    /// which is what attributing cost to a recorded event needs.
    pub fn attempts(&self) -> impl Iterator<Item = &AttemptProvenance> {
        self.admission.attempts()
    }

    pub fn wide_model(&self) -> Option<&ModelObservation> {
        self.wide.as_ref()
    }

    /// Retain the received identity/times even when a pair is rejected.
    pub fn last_model(&self) -> Option<&ModelObservation> {
        self.last_model.as_ref()
    }

    fn error(&self, kind: RetryErrorKind, last_transport: Option<TransportError>) -> RetryError {
        RetryError {
            kind,
            receipt: self.receipt(),
            last_transport,
        }
    }

    /// Retry a single logical stage. `authorize` runs immediately before EVERY
    /// admission, including retries. The owner must refresh its trusted policy
    /// projection there; a changed policy or unavailable required guard returns
    /// `Err(())`. No request text can grant authorization.
    pub async fn send_stage(
        &mut self,
        stage: RankingStage,
        request: &Request,
        cx: &Cx,
        mut authorize: impl FnMut() -> Result<NetworkConsent, ()>,
    ) -> Result<StageResponse, RetryError> {
        if stage == RankingStage::Rerank
            && self
                .wide
                .as_ref()
                .is_some_and(|wide| wide.requested != request.model())
        {
            return Err(self.error(RetryErrorKind::ModelPairMismatch, None));
        }
        let initial_attempts = self.receipt().admitted_attempts;
        let started_at = SystemTime::now();
        let started_elapsed_ms = self.clock.now().as_millis();
        let mut last_transport = None;
        loop {
            check_work(cx, &self.clock)
                .map_err(|kind| self.error(RetryErrorKind::Transport(kind), last_transport))?;
            let consent = authorize()
                .map_err(|()| self.error(RetryErrorKind::PolicyChanged, last_transport))?;
            let permit = self
                .admission
                .admit(stage, &self.origin)
                .map_err(|kind| self.error(RetryErrorKind::Admission(kind), last_transport))?;
            let mut flight = AttemptFlight {
                admission: &mut self.admission,
                permit: Some(permit),
                sent: None,
            };
            let result = {
                let mut on_start = || flight.start();
                self.client
                    .send_accounted(
                        request,
                        self.credential,
                        consent,
                        cx,
                        &self.clock,
                        &mut on_start,
                    )
                    .await
            };
            let accounting = flight.finish(
                result.as_ref().ok().map(|response| response.usage),
                result.as_ref().err(),
            );
            drop(flight);
            accounting.map_err(|_| self.error(RetryErrorKind::Accounting, last_transport))?;
            match result {
                Ok(response) => {
                    let model = ModelObservation {
                        requested: response.requested_model.clone(),
                        returned: response.returned_model.clone(),
                        started_at,
                        received_at: SystemTime::now(),
                        started_elapsed_ms,
                        received_elapsed_ms: self.clock.now().as_millis(),
                    };
                    self.last_model = Some(model.clone());
                    let pair_status = if stage == RankingStage::Rerank {
                        let Some(wide) = &self.wide else {
                            return Err(self.error(RetryErrorKind::ModelPairMismatch, None));
                        };
                        // Conservative for both aliases and immutable revisions:
                        // changed identifiers invalidate; equality is not a pin.
                        if wide.requested != model.requested || wide.returned != model.returned {
                            return Err(self.error(RetryErrorKind::ModelPairMismatch, None));
                        }
                        ModelPairStatus::MatchingUnpinnedIdentifiers
                    } else {
                        ModelPairStatus::NotPaired
                    };
                    if stage == RankingStage::Wide {
                        self.wide = Some(model.clone());
                    }
                    return Ok(StageResponse {
                        response,
                        model,
                        pair_status,
                        attempts: self.receipt().admitted_attempts - initial_attempts,
                        receipt: self.receipt(),
                    });
                }
                Err(error) => {
                    last_transport = Some(error);
                    if !retryable(error.kind) {
                        return Err(
                            self.error(RetryErrorKind::Transport(error.kind), last_transport)
                        );
                    }
                    if self.admission.remaining_http_attempts() == 0 {
                        return Err(self.error(
                            RetryErrorKind::Admission(AdmissionRefusal::AttemptsExhausted {
                                attempts_used: self.receipt().admitted_attempts,
                                limit: self.admission.budget().max_http_attempts(),
                            }),
                            last_transport,
                        ));
                    }
                    let delay = retry_delay(
                        &error,
                        self.receipt().admitted_attempts - initial_attempts,
                        Duration::from_millis(self.clock.remaining_before_cleanup().as_millis()),
                        Duration::from_millis(self.admission.budget().min_attempt_reserve_ms()),
                        cx.random_u64(),
                    )
                    .map_err(|stop| {
                        self.error(RetryErrorKind::RetryStopped(stop), last_transport)
                    })?;
                    wait_before_retry(cx, &self.clock, delay)
                        .await
                        .map_err(|kind| {
                            self.error(RetryErrorKind::Transport(kind), last_transport)
                        })?;
                }
            }
        }
    }
}

/// Synchronous finalization also protects cancellation by dropping the future:
/// once entered, unresolved work is unknown cost, never free or left active.
struct AttemptFlight<'a> {
    admission: &'a mut AttemptAdmission,
    permit: Option<AttemptPermit>,
    sent: Option<SentAttempt>,
}
impl AttemptFlight<'_> {
    fn start(&mut self) -> Result<(), TransportError> {
        let error = || TransportError {
            kind: TransportErrorKind::Protocol,
            http_attempt_started: false,
            retry_after: RetryAfter::Absent,
        };
        let sent = self
            .permit
            .take()
            .ok_or_else(error)?
            .mark_sent()
            .map_err(|_| error())?;
        self.admission.record_sent(&sent).map_err(|_| error())?;
        self.sent = Some(sent);
        Ok(())
    }
    fn finish(
        &mut self,
        usage: Option<Usage>,
        error: Option<&TransportError>,
    ) -> Result<(), AdmissionError> {
        if let Some(sent) = self.sent.take() {
            match usage {
                Some(usage) => self.admission.record_response(&sent, usage),
                None => self
                    .admission
                    .record_terminal_failure(&sent, attempt_failure(error)),
            }
        } else if let Some(permit) = self.permit.take() {
            self.admission
                .record_discard(&permit.discard_before_send("local refusal before HTTP"))
        } else {
            Ok(())
        }
    }
}
impl Drop for AttemptFlight<'_> {
    fn drop(&mut self) {
        let _ = self.finish(None, None);
    }
}

/// An attempt finalized without an observed outcome, as when cancellation unwinds
/// through the drop guard, has a disposition that is simply unknown.
fn attempt_failure(error: Option<&TransportError>) -> AttemptFailure {
    error.map_or(
        AttemptFailure::Indeterminate("settlement-unobserved"),
        AttemptFailure::from_transport,
    )
}

fn check_work(cx: &Cx, clock: &EntryClock) -> Result<(), TransportErrorKind> {
    if cx.is_cancel_requested() {
        return Err(TransportErrorKind::Cancelled);
    }
    clock
        .admit_new_work()
        .map(|_| ())
        .map_err(|_| TransportErrorKind::Deadline)
}

async fn wait_before_retry(
    cx: &Cx,
    clock: &EntryClock,
    delay: Duration,
) -> Result<(), TransportErrorKind> {
    // Preserve sub-millisecond precision. Adding to the entry clock's floored
    // millisecond reading could otherwise retry up to 1ms too early. This
    // monotonic wake time does not replace or extend the entry deadline.
    let target = Instant::now()
        .checked_add(delay)
        .ok_or(TransportErrorKind::Deadline)?;
    loop {
        check_work(cx, clock)?;
        let left = target.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(());
        }
        asupersync::time::sleep(
            asupersync::time::wall_now(),
            left.min(Duration::from_millis(10)),
        )
        .await;
    }
}
