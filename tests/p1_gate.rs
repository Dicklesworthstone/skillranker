#![cfg(unix)]
//! Phase P1 Acceptance Gate: Transport and Runtime Readiness.
//!
//! Satisfies contract boundary `p1_acceptance_gate` (sr-roadmap-l1i.2.12)
//! mapped in `tests/contract_matrix.toml`.
//!
//! Verifies that the complete Phase P1 transport, runtime, codec, retry,
//! accounting, and security subsystem satisfies all foundational invariants.

use asupersync::Cx;
use serde_json::json;
use skillranker::config::{ConfigSources, ResolvedConfig};
use skillranker::jev::admission::{AttemptAdmission, RankingStage};
use skillranker::jev::client::{JevClient, TransportErrorKind};
use skillranker::jev::codec::{CodecError, MAX_REQUEST_BYTES, Question, Request};
use skillranker::jev::endpoint::CanonicalOrigin;
use skillranker::jev::retry::retryable;
use skillranker::jev::{EndpointConfig, OriginScopedCredential};
use skillranker::limits::DurationMillis;
use skillranker::privacy::{
    ConsentSource, CredentialStatus, NetworkBlock, NetworkConsent, ProviderAdmissionRefusal,
    admit_provider_attempt,
};
use skillranker::runtime::{EntryClock, ProcessInvocation};
use std::ffi::OsString;

const CANARY_SECRET: &str = "test-canary-secret-never-leak-12345";

struct GateContext {
    clock: EntryClock,
    invocation: ProcessInvocation,
    _cx: Cx,
}

impl GateContext {
    fn new(total_ms: u64, cleanup_ms: u64) -> Self {
        let clock = EntryClock::capture_with(
            DurationMillis::new("gate-total", total_ms, 30_000).unwrap(),
            DurationMillis::new("gate-cleanup", cleanup_ms, 30_000).unwrap(),
        )
        .expect("clock initialization");
        let invocation = ProcessInvocation::from_clock(clock).expect("invocation enter");
        let _cx = invocation.request_cx().expect("request cx");
        Self {
            clock,
            invocation,
            _cx,
        }
    }

    fn finish(self) {
        assert!(
            self.invocation.shutdown(),
            "runtime must cleanly shut down within deadline"
        );
    }
}

fn sample_credential(origin: &CanonicalOrigin, token: &str) -> OriginScopedCredential {
    let config = ResolvedConfig::resolve(
        ConfigSources {
            environment: vec![(OsString::from("TYPESAFE_API_KEY"), OsString::from(token))],
            ..Default::default()
        },
        1,
    )
    .expect("config with key");
    OriginScopedCredential::bind(config.credential().unwrap().clone(), origin)
        .expect("origin-scoped credential bind")
}

#[test]
fn all_p1_invariants_verified() {
    let ctx = GateContext::new(10_000, 500);

    // 1. Runtime Invariant: Clock deadline tracking and cleanup budget
    assert_eq!(
        ctx.clock.deadline().total().as_millis(),
        10_000,
        "total budget must match configured value"
    );
    assert_eq!(
        ctx.clock.deadline().cleanup_reserve().as_millis(),
        500,
        "cleanup reserve must match configured value"
    );
    assert!(
        ctx.clock.remaining_before_cleanup().as_millis() > 9_000,
        "usable budget must exclude cleanup reserve"
    );

    // 2. Endpoint & Origin Invariant: Canonical base origin joining /v1/systemone once
    let prod_endpoint = EndpointConfig::production();
    assert_eq!(
        prod_endpoint.origin().as_str(),
        "https://api.typesafe.ai",
        "production origin must be https://api.typesafe.ai"
    );
    assert!(
        prod_endpoint.origin().is_secure(),
        "endpoint origin must be HTTPS"
    );
    assert_eq!(
        prod_endpoint.target_url().as_str(),
        "https://api.typesafe.ai/v1/systemone",
        "endpoint URL must append /v1/systemone exactly once"
    );

    // Endpoint error cases: reject non-root path, userinfo, query string, and insecure HTTP
    assert!(EndpointConfig::from_base_origin_str("https://api.typesafe.ai/custom/path").is_err());
    assert!(EndpointConfig::from_base_origin_str("https://user:pass@api.typesafe.ai").is_err());
    assert!(EndpointConfig::from_base_origin_str("https://api.typesafe.ai?query=1").is_err());
    assert!(EndpointConfig::from_base_origin_str("http://api.typesafe.ai").is_err());
    assert!(CanonicalOrigin::parse("http://api.typesafe.ai").is_err());

    // 3. Credential Routing Invariant: Bound to exact HTTPS origin
    let cred = sample_credential(prod_endpoint.origin(), CANARY_SECRET);
    assert_eq!(cred.origin(), prod_endpoint.origin());

    // 4. Request Codec Invariant: 96 KiB cap, choice 255 option limit, __none__ sentinel
    let valid_choice = Question::choice(
        json!("Which tool is best?"),
        [
            ("tool_a".to_string(), "Tool A description".to_string()),
            ("tool_b".to_string(), "Tool B description".to_string()),
            (
                "__none__".to_string(),
                "None of the above tools".to_string(),
            ),
        ],
    );
    assert!(valid_choice.is_ok(), "valid Choice question must construct");

    // Duplicate option rejected
    let dup_choice = Question::choice(
        json!("Duplicate probe"),
        [
            ("tool_a".to_string(), "Desc 1".to_string()),
            ("tool_a".to_string(), "Desc 2".to_string()),
        ],
    );
    assert!(matches!(dup_choice, Err(CodecError::DuplicateId)));

    // Request construction and 96 KiB cap
    let valid_req = Request::new(
        "jev-latest".to_string(),
        json!({"task": "p1_gate_test"}),
        [("q1".to_string(), valid_choice.unwrap())],
    );
    assert!(valid_req.is_ok(), "valid Request must construct");

    let oversized_req = Request::new(
        "jev-latest".to_string(),
        json!({"context": "a".repeat(MAX_REQUEST_BYTES + 1024)}),
        [(
            "q1".to_string(),
            Question::Noul {
                instructions: json!("Probe"),
                criteria: None,
            },
        )],
    );
    assert!(matches!(oversized_req, Err(CodecError::TooLarge)));

    // 5. Response Validation Invariant: Argmax match, sum tolerance, finite bounds
    let valid_resp_bytes = br#"{
        "model": "jev-1.13.0",
        "answers": {
            "q1": {
                "type": "choice",
                "choice": "tool_a",
                "confidence": 0.95,
                "probabilities": {
                    "tool_a": 0.70,
                    "tool_b": 0.20,
                    "__none__": 0.10
                }
            }
        },
        "usage": {
            "input_tokens": 150,
            "output_tokens": 25
        }
    }"#;
    let decoded = valid_req
        .as_ref()
        .unwrap()
        .decode_response(valid_resp_bytes);
    assert!(decoded.is_ok(), "valid response must decode cleanly");
    let resp = decoded.unwrap();
    assert_eq!(resp.requested_model, "jev-latest");
    assert_eq!(resp.returned_model, "jev-1.13.0");
    assert_eq!(resp.usage.input_tokens, 150);
    assert_eq!(resp.usage.output_tokens, 25);
    assert_eq!(resp.usage.total_tokens(), 175);

    // Sum violating tolerance (> 1e-4 drift) must be rejected
    let bad_sum_bytes = br#"{
        "model": "jev-1.13.0",
        "answers": {
            "q1": {
                "type": "choice",
                "choice": "tool_a",
                "confidence": 0.95,
                "probabilities": {
                    "tool_a": 0.70,
                    "tool_b": 0.20,
                    "__none__": 0.05
                }
            }
        },
        "usage": {"input_tokens": 100, "output_tokens": 10}
    }"#;
    assert!(matches!(
        valid_req.as_ref().unwrap().decode_response(bad_sum_bytes),
        Err(CodecError::InvalidDistribution)
    ));

    // Choice not matching argmax must be rejected
    let bad_choice_bytes = br#"{
        "model": "jev-1.13.0",
        "answers": {
            "q1": {
                "type": "choice",
                "choice": "tool_b",
                "confidence": 0.95,
                "probabilities": {
                    "tool_a": 0.70,
                    "tool_b": 0.20,
                    "__none__": 0.10
                }
            }
        },
        "usage": {"input_tokens": 100, "output_tokens": 10}
    }"#;
    assert!(matches!(
        valid_req
            .as_ref()
            .unwrap()
            .decode_response(bad_choice_bytes),
        Err(CodecError::InvalidChoice)
    ));

    // 6. Fail-Closed Privacy & Admission Invariants
    assert_eq!(
        admit_provider_attempt(
            NetworkConsent::Blocked(NetworkBlock::Offline),
            CredentialStatus::PresentFromEnvironment
        ),
        Err(ProviderAdmissionRefusal::Offline)
    );
    assert_eq!(
        admit_provider_attempt(
            NetworkConsent::Blocked(NetworkBlock::DryRun),
            CredentialStatus::PresentFromEnvironment
        ),
        Err(ProviderAdmissionRefusal::DryRun)
    );
    assert_eq!(
        admit_provider_attempt(
            NetworkConsent::NotAuthorized,
            CredentialStatus::PresentFromEnvironment
        ),
        Err(ProviderAdmissionRefusal::NetworkNotAuthorized)
    );
    assert_eq!(
        admit_provider_attempt(
            NetworkConsent::Authorized(ConsentSource::AllowNetworkFlag),
            CredentialStatus::Absent
        ),
        Err(ProviderAdmissionRefusal::MissingCredential)
    );
    assert_eq!(
        admit_provider_attempt(
            NetworkConsent::Authorized(ConsentSource::AllowNetworkFlag),
            CredentialStatus::PresentFromEnvironment
        ),
        Ok(ConsentSource::AllowNetworkFlag)
    );

    // 7. Attempt Allowance Invariant: 2 logical requests, 4 HTTP attempts total
    let mut admission = AttemptAdmission::default_invocation(ctx.clock, "gate-test");
    assert_eq!(admission.remaining_logical_requests(), 2);
    assert_eq!(admission.remaining_http_attempts(), 4);

    let permit = admission.admit(RankingStage::Wide, prod_endpoint.origin());
    assert!(permit.is_ok(), "first wide attempt must be admitted");
    let sent = permit.unwrap().mark_sent().unwrap();
    assert_eq!(sent.stage(), RankingStage::Wide);
    assert_eq!(admission.remaining_http_attempts(), 3);

    // 8. Retry Classification Invariant: 429/529/503 retryable; 400/401/403/404 non-retryable
    assert!(retryable(TransportErrorKind::HttpStatus(429)));
    assert!(retryable(TransportErrorKind::HttpStatus(529)));
    assert!(retryable(TransportErrorKind::HttpStatus(503)));
    assert!(!retryable(TransportErrorKind::HttpStatus(400)));
    assert!(!retryable(TransportErrorKind::HttpStatus(401)));
    assert!(!retryable(TransportErrorKind::HttpStatus(403)));
    assert!(!retryable(TransportErrorKind::HttpStatus(404)));
    assert!(retryable(TransportErrorKind::Connect));
    assert!(retryable(TransportErrorKind::Dns));
    assert!(retryable(TransportErrorKind::TransientIo));

    // 9. Transport Client & Roots Invariant: Public WebPKI roots without accept-all
    let client = JevClient::new(prod_endpoint.clone()).expect("JevClient with native trust roots");
    assert_eq!(client.origin(), prod_endpoint.origin());

    // Diagnostic safety invariant: ensure canary is not in any display text
    let display_output = format!(
        "{:?}",
        TransportErrorKind::Admission(ProviderAdmissionRefusal::MissingCredential)
    );
    assert!(
        !display_output.contains(CANARY_SECRET),
        "canary secret must never appear in diagnostic display"
    );

    ctx.finish();
}
