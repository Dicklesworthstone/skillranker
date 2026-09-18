//! TypeSafe Jev client, endpoint canonicalization, request/response codecs,
//! and admission policies.
//!
//! TypeSafe.ai's Jev is the essential ranking engine for SkillRanker.
//! This module provides pure base-origin canonicalization, origin-scoped credential
//! routing, redirect prohibition, and request/response validation.

pub mod admission;
pub mod client;
pub mod codec;
pub mod rerank;
pub mod endpoint;
pub mod retry;
pub mod wide;

pub use admission::{
    AdmissionError, AdmissionRefusal, AttemptAdmission, AttemptBudget, AttemptId, AttemptPermit,
    CostReceipt, DEFAULT_GUARD_GENERATION, DEFAULT_MIN_ATTEMPT_RESERVE_MS, DiscardedAttempt,
    RankingStage, SentAttempt,
};
pub use codec::{
    Answer, ChoiceAnswer, CodecError, MAX_CHOICE_OPTIONS, MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES,
    NoulCriteria, Question, Request, Response, SUM_TOLERANCE, Usage,
};
pub use endpoint::{
    AMBIENT_PROXY_VARS, CanonicalOrigin, CredentialRoutingError, DEFAULT_TYPESAFE_ENDPOINT,
    EndpointConfig, EndpointError, OriginScopedCredential, ProxyPolicy, RedirectError,
    RedirectPolicy, SKILLRANKER_USER_AGENT, SYSTEMONE_PATH, Scheme, TargetUrl,
    sanitize_url_for_diagnostics,
};
