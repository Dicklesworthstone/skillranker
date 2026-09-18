#![cfg(unix)]
//! Bounded live Jev contract spike and consented provider smoke test.
//!
//! Satisfies contract boundary `p1_jev_contract_smoke` (sr-roadmap-l1i.2.10)
//! mapped in `tests/contract_matrix.toml`.

use asupersync::Cx;
use serde_json::json;
use skillranker::config::{ConfigSources, ResolvedConfig};
use skillranker::jev::client::{JevClient, TransportError, TransportErrorKind};
use skillranker::jev::codec::{
    Answer, CodecError, MAX_CHOICE_OPTIONS, MAX_REQUEST_BYTES, Question, Request, Response,
    SUM_TOLERANCE,
};
use skillranker::jev::{CanonicalOrigin, EndpointConfig, OriginScopedCredential};
use skillranker::limits::DurationMillis;
use skillranker::privacy::{ConsentSource, NetworkBlock, NetworkConsent, ProviderAdmissionRefusal};
use skillranker::runtime::{EntryClock, ProcessInvocation};
use std::ffi::OsString;
use std::path::Path;

const DEFAULT_MODEL: &str = "jev-latest";

struct TestContext {
    clock: EntryClock,
    invocation: ProcessInvocation,
    cx: Cx,
}

impl TestContext {
    fn new(total_ms: u64, cleanup_ms: u64) -> Self {
        let clock = EntryClock::capture_with(
            DurationMillis::new("test-total", total_ms, 30_000).unwrap(),
            DurationMillis::new("test-cleanup", cleanup_ms, 30_000).unwrap(),
        )
        .unwrap();
        let invocation = ProcessInvocation::from_clock(clock).unwrap();
        let cx = invocation.request_cx().unwrap();
        Self {
            clock,
            invocation,
            cx,
        }
    }

    fn send(
        &self,
        client: &JevClient,
        request: &Request,
        key: Option<&OriginScopedCredential>,
        consent: NetworkConsent,
    ) -> Result<Response, TransportError> {
        self.invocation.runtime().block_on(client.send(
            request,
            key,
            consent,
            &self.cx,
            &self.clock,
        ))
    }

    fn finish(self) {
        assert!(
            self.invocation.shutdown(),
            "ProcessInvocation runtime must shut down cleanly within deadline"
        );
    }
}

fn resolve_credential(origin: &CanonicalOrigin, token: &str) -> OriginScopedCredential {
    let config = ResolvedConfig::resolve(
        ConfigSources {
            environment: vec![(OsString::from("TYPESAFE_API_KEY"), OsString::from(token))],
            ..Default::default()
        },
        1,
    )
    .expect("valid config with token");
    OriginScopedCredential::bind(config.credential().unwrap().clone(), origin)
        .expect("origin scoped credential bind")
}

fn load_api_key_from_env_or_file() -> Option<String> {
    if let Ok(key) = std::env::var("TYPESAFE_API_KEY")
        && !key.trim().is_empty()
    {
        return Some(key.trim().to_string());
    }

    let env_path = Path::new(env!("CARGO_MANIFEST_DIR")).join(".env");
    if env_path.is_file()
        && let Ok(content) = std::fs::read_to_string(&env_path)
    {
        for line in content.lines() {
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix("TYPESAFE_API_KEY=") {
                let key = rest.trim().trim_matches('"').trim_matches('\'');
                if !key.is_empty() {
                    return Some(key.to_string());
                }
            }
        }
    }

    None
}

fn synthetic_contract_request() -> Request {
    Request::new(
        DEFAULT_MODEL.to_string(),
        json!({
            "task": "bounded_contract_spike",
            "context": "Synthetic qualification probe. An apple is a fruit; a carrot is a vegetable. No real session data."
        }),
        [
            (
                "food_class".to_string(),
                Question::choice(
                    json!("Which candidate is described as a fruit?"),
                    [
                        (
                            "apple".to_string(),
                            "An apple, sweet edible fruit produced by an apple tree".to_string(),
                        ),
                        (
                            "carrot".to_string(),
                            "A carrot, root vegetable usually orange in color".to_string(),
                        ),
                        (
                            "__none__".to_string(),
                            "Neither listed candidate fits the description".to_string(),
                        ),
                    ],
                )
                .expect("valid choice question"),
            ),
            (
                "fruit_health".to_string(),
                Question::Noul {
                    instructions: json!("Is an apple considered a healthy food?"),
                    criteria: None,
                },
            ),
        ],
    )
    .expect("valid synthetic request")
}

#[test]
fn budgeted_live_contract_smoke() {
    let maybe_key = load_api_key_from_env_or_file();
    let live_consent_granted = std::env::var("SKILLRANKER_LIVE_CONSENT")
        .map(|v| v == "1" || v == "true")
        .unwrap_or(true);

    let endpoint_config = EndpointConfig::production();
    let origin = endpoint_config.origin().clone();
    assert!(origin.is_secure(), "TypeSafe endpoint must be HTTPS");

    let run = TestContext::new(10000, 500);

    if let (Some(ref api_key), true) = (maybe_key, live_consent_granted) {
        let cred = resolve_credential(&origin, api_key);
        let client = JevClient::new(endpoint_config).expect("client with public trust roots");

        let request = synthetic_contract_request();
        let consent = NetworkConsent::Authorized(ConsentSource::AllowNetworkFlag);

        let started = std::time::Instant::now();
        let response_result = run.send(&client, &request, Some(&cred), consent);
        let elapsed = started.elapsed();

        let response = match response_result {
            Ok(resp) => resp,
            Err(err) => {
                // If live service is unreachable from remote build worker or offline,
                // do not panic without clear diagnostic; verify it failed safely.
                panic!("live Jev contract spike failed: {:?}", err.kind);
            }
        };

        // Validate response properties according to P1 contract
        assert_eq!(response.requested_model, DEFAULT_MODEL);
        assert!(!response.returned_model.is_empty());

        let answers = &response.answers;
        assert_eq!(answers.len(), 2, "must receive exactly 2 answers");

        // Validate Choice answer
        match answers.get("food_class") {
            Some(Answer::Choice(choice)) => {
                assert!(
                    choice.choice() == "apple"
                        || choice.choice() == "carrot"
                        || choice.choice() == "__none__",
                    "choice must be one of the specified options"
                );
                assert!(
                    (0.0..=1.0).contains(&choice.confidence()),
                    "confidence must be in [0, 1]"
                );
                let probs = choice.raw_probabilities();
                assert_eq!(probs.len(), 3);
                assert!(probs.contains_key("apple"));
                assert!(probs.contains_key("carrot"));
                assert!(probs.contains_key("__none__"));

                for &val in probs.values() {
                    assert!(
                        (0.0..=1.0).contains(&val),
                        "probabilities must be in [0, 1]"
                    );
                }
                assert!(
                    (choice.raw_sum() - 1.0).abs() <= SUM_TOLERANCE,
                    "probabilities sum ({}) must be within tolerance of 1.0",
                    choice.raw_sum()
                );
            }
            _ => panic!("expected Choice answer for food_class"),
        }

        // Validate Noul answer
        match answers.get("fruit_health") {
            Some(Answer::Noul(val)) => {
                assert!((0.0..=1.0).contains(val), "noul value must be in [0, 1]");
            }
            _ => panic!("expected Noul answer for fruit_health"),
        }

        // Validate Usage
        assert!(
            response.usage.input_tokens > 0,
            "input tokens must be positive"
        );
        assert!(
            response.usage.output_tokens > 0,
            "output tokens must be positive"
        );
        assert!(
            response.usage.total_tokens() > 0,
            "total tokens must be positive"
        );

        eprintln!(
            "[LIVE CONTRACT SPIKE PASSED] model={}, returned={}, input_tokens={}, output_tokens={}, elapsed_ms={}",
            response.requested_model,
            response.returned_model,
            response.usage.input_tokens,
            response.usage.output_tokens,
            elapsed.as_millis()
        );
    } else {
        // Missing credentials or consent: verify that admission fails safely closed
        let client = JevClient::new(endpoint_config).expect("client with public trust roots");
        let request = synthetic_contract_request();

        // 1. With absent consent
        let err_no_consent = match run.send(&client, &request, None, NetworkConsent::NotAuthorized)
        {
            Err(e) => e,
            Ok(_) => panic!("must be refused when consent is absent"),
        };
        assert_eq!(
            err_no_consent.kind,
            TransportErrorKind::Admission(ProviderAdmissionRefusal::NetworkNotAuthorized)
        );
        assert!(!err_no_consent.http_attempt_started);

        // 2. With absent credential
        let err_no_key = match run.send(
            &client,
            &request,
            None,
            NetworkConsent::Authorized(ConsentSource::AllowNetworkFlag),
        ) {
            Err(e) => e,
            Ok(_) => panic!("must be refused when credential is absent"),
        };
        assert_eq!(
            err_no_key.kind,
            TransportErrorKind::Admission(ProviderAdmissionRefusal::MissingCredential)
        );
        assert!(!err_no_key.http_attempt_started);

        eprintln!(
            "[LIVE SPIKE GATE VERIFIED CLOSED] No TYPESAFE_API_KEY or consent; admission gates confirmed fail-closed."
        );
    }

    run.finish();
}

#[test]
fn request_size_and_limits_bounded() {
    let large_text = "x".repeat(MAX_REQUEST_BYTES + 1024);
    let oversized = Request::new(
        DEFAULT_MODEL.to_string(),
        json!({ "context": large_text }),
        [(
            "probe".to_string(),
            Question::Noul {
                instructions: json!("Is this too large?"),
                criteria: None,
            },
        )],
    );
    match oversized {
        Err(CodecError::TooLarge) => {}
        Err(other) => panic!("expected CodecError::TooLarge, got {:?}", other),
        Ok(_) => panic!("oversized request must be rejected"),
    }
}

#[test]
fn choice_options_limit_and_none_sentinel() {
    let mut options = Vec::new();
    for i in 0..MAX_CHOICE_OPTIONS {
        options.push((format!("opt_{i}"), format!("Option {i} description")));
    }
    let valid_choice = Question::choice(json!("Valid question"), options.clone());
    assert!(valid_choice.is_ok());

    options.push(("overflow".to_string(), "Overflow option".to_string()));
    let overflow_choice = Question::choice(json!("Overflow question"), options);
    match overflow_choice {
        Err(CodecError::InvalidRequest) => {}
        Err(other) => panic!("expected CodecError::InvalidRequest, got {:?}", other),
        Ok(_) => panic!("overflow choice must be rejected"),
    }
}

#[test]
fn unauthorized_attempt_refused_without_network() {
    let endpoint = EndpointConfig::production();
    let client = JevClient::new(endpoint).unwrap();
    let request = synthetic_contract_request();
    let run = TestContext::new(3000, 500);

    let err = match run.send(
        &client,
        &request,
        None,
        NetworkConsent::Blocked(NetworkBlock::Offline),
    ) {
        Err(e) => e,
        Ok(_) => panic!("offline flag must block attempt before network"),
    };

    assert_eq!(
        err.kind,
        TransportErrorKind::Admission(ProviderAdmissionRefusal::Offline)
    );
    assert!(!err.http_attempt_started);

    run.finish();
}
