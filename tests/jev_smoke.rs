#![cfg(unix)]
//! Bounded live Jev contract spike and consented provider smoke test.
//!
//! Exercises `p1_jev_contract_smoke` (sr-roadmap-l1i.2.10), mapped in
//! `tests/contract_matrix.toml`. A small live success does not qualify provider capacity.

use asupersync::Cx;
use serde_json::json;
use skillranker::config::{ConfigSources, ResolvedConfig};
use skillranker::jev::admission::{AttemptAdmission, AttemptBudget, RankingStage};
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
use std::process::Command;

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

// Consent is checked before consulting credentials. Never load a repository .env.
fn live_api_key(
    consent: Option<OsString>,
    read_key: impl FnOnce() -> Option<OsString>,
) -> Result<String, &'static str> {
    if !matches!(
        consent.as_deref().and_then(|v| v.to_str()),
        Some("1" | "true")
    ) {
        return Err("explicit SKILLRANKER_LIVE_CONSENT=1 is required");
    }
    let key = read_key().ok_or("export TYPESAFE_API_KEY explicitly before the live test")?;
    let key = key
        .to_str()
        .ok_or("TYPESAFE_API_KEY must be valid Unicode")?;
    if key.trim().is_empty() {
        return Err("TYPESAFE_API_KEY must be nonempty");
    }
    Ok(key.trim().to_owned())
}

#[test]
fn live_consent_precedes_credential_lookup() {
    use std::os::unix::ffi::OsStringExt;
    for consent in [
        None,
        Some(OsString::from("0")),
        Some(OsString::from("false")),
        Some(OsString::from("TRUE")),
        Some(OsString::from(" 1")),
        Some(OsString::from_vec(vec![0xff])),
    ] {
        assert!(live_api_key(consent, || panic!("credential lookup before consent")).is_err());
    }
    for consent in ["1", "true"] {
        assert_eq!(
            live_api_key(Some(consent.into()), || Some("synthetic-test-key".into())),
            Ok("synthetic-test-key".to_owned())
        );
    }
    for key in [None, Some(" ".into()), Some(OsString::from_vec(vec![0xff]))] {
        assert!(live_api_key(Some("1".into()), || key).is_err());
    }
}

#[test]
fn explicitly_selected_live_test_cannot_pass_without_consent_or_key() {
    for (name, consent_name) in [
        ("budgeted_live_contract_smoke", "SKILLRANKER_LIVE_CONSENT"),
        (
            "budgeted_live_capacity_shapes",
            "SKILLRANKER_CAPACITY_CONSENT",
        ),
    ] {
        for (consent, key, diagnostic) in [
            (
                None,
                Some("synthetic-credential-canary"),
                "explicit SKILLRANKER_LIVE_CONSENT=1 is required",
            ),
            (
                Some("false"),
                Some("synthetic-credential-canary"),
                "explicit SKILLRANKER_LIVE_CONSENT=1 is required",
            ),
            (
                Some("1"),
                None,
                "export TYPESAFE_API_KEY explicitly before the live test",
            ),
        ] {
            let mut child = Command::new(std::env::current_exe().unwrap());
            child
                .env_clear()
                .args(["--ignored", "--exact", name, "--nocapture"]);
            if let Some(consent) = consent {
                child.env(consent_name, consent);
            }
            if let Some(key) = key {
                child.env("TYPESAFE_API_KEY", key);
            }
            let output = child
                .output()
                .expect("launch the actual smoke test executable");
            assert!(
                !output.status.success(),
                "an unexecuted live probe must not pass"
            );
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                stderr.contains(if name == "budgeted_live_capacity_shapes" {
                    "capacity probe requires SKILLRANKER_CAPACITY_CONSENT=1"
                } else {
                    diagnostic
                }),
                "missing static prerequisite diagnostic"
            );
            assert!(!stderr.contains("synthetic-credential-canary"));
            assert!(
                !String::from_utf8_lossy(&output.stdout).contains("synthetic-credential-canary")
            );
        }
    }
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
#[ignore = "live paid request: explicit consent and exported TYPESAFE_API_KEY required"]
fn budgeted_live_contract_smoke() {
    let api_key = live_api_key(std::env::var_os("SKILLRANKER_LIVE_CONSENT"), || {
        std::env::var_os("TYPESAFE_API_KEY")
    })
    .unwrap_or_else(|message| panic!("live Jev prerequisites: {message}"));

    let endpoint_config = EndpointConfig::production();
    let origin = endpoint_config.origin().clone();
    assert!(origin.is_secure(), "TypeSafe endpoint must be HTTPS");

    let run = TestContext::new(10000, 500);
    let cred = resolve_credential(&origin, &api_key);
    let client = JevClient::new(endpoint_config).expect("client with public trust roots");

    let request = synthetic_contract_request();
    let consent = NetworkConsent::Authorized(ConsentSource::AllowNetworkFlag);

    let mut admission = AttemptAdmission::new(
        AttemptBudget::new(1, 1).unwrap(),
        run.clock,
        "synthetic-live-smoke",
    )
    .unwrap();
    let permit = admission.admit(RankingStage::Probe, &origin).unwrap();
    let started = std::time::Instant::now();
    let response_result = run.send(&client, &request, Some(&cred), consent);
    let elapsed = started.elapsed();

    let response = match response_result {
        Ok(resp) => {
            let sent = permit.mark_sent().unwrap();
            admission.record_sent(&sent).unwrap();
            admission.record_response(&sent, resp.usage).unwrap();
            resp
        }
        Err(err) => {
            if err.http_attempt_started {
                let sent = permit.mark_sent().unwrap();
                admission.record_sent(&sent).unwrap();
                admission
                    .record_terminal_failure(&sent, "live probe failed")
                    .unwrap();
            } else {
                admission
                    .record_discard(&permit.discard_before_send("local refusal"))
                    .unwrap();
            }
            run.finish();
            panic!(
                "live Jev contract spike failed: {:?}; http_attempt_started={}; usage_unknown={}",
                err.kind, err.http_attempt_started, err.http_attempt_started
            );
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

    // Hash provider-controlled identity rather than printing arbitrary response text.
    // One direct send, no retry loop; JevClient also disables transport retries.
    let receipt = json!({
        "schema_version": 1,
        "kind": "synthetic-live-jev-smoke",
        "requested_model": DEFAULT_MODEL,
        "request_bytes": request.to_json().unwrap().len(),
        "request_blake3": blake3::hash(&request.to_json().unwrap()).to_hex().to_string(),
        "returned_model_blake3": blake3::hash(response.returned_model.as_bytes()).to_hex().to_string(),
        "http_attempts": admission.receipt().sent_attempts,
        "admitted_attempts": admission.receipt().admitted_attempts,
        "questions": 2,
        "choice_options": 3,
        "input_tokens": response.usage.input_tokens,
        "output_tokens": response.usage.output_tokens,
        "elapsed_ms": elapsed.as_millis(),
        "provider_capacity_qualified": false
    });

    run.finish();
    eprintln!("{receipt}");
}

#[test]
fn admission_gates_fail_closed_without_consent_or_credential() {
    let endpoint_config = EndpointConfig::production();
    let client = JevClient::new(endpoint_config).expect("client with public trust roots");
    let request = synthetic_contract_request();
    let run = TestContext::new(3000, 500);

    // 1. With absent consent
    let err_no_consent = match run.send(&client, &request, None, NetworkConsent::NotAuthorized) {
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

/// Production builders over an authorized synthetic roster, never local skills.
fn capacity_requests() -> [Request; 2] {
    use skillranker::authorized_read::{AuthorizedRoot, AuthorizedRoots};
    use skillranker::context::RenderedContextPayload;
    use skillranker::identity::{LogicalSkillKey, SourceId};
    use skillranker::output::ContextQuality;
    use skillranker::privacy::ContextProfile;
    use skillranker::roster::resolution::{BindingSpec, ResolvedRoster, SkillEntry};
    use skillranker::roster::{InvocationName, InvocationRestrictions, Visibility};
    use std::os::unix::fs::DirBuilderExt;
    use std::path::Path;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    let root = std::env::temp_dir().join(format!(
        "sr-capacity-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&root)
        .unwrap();
    let roots = AuthorizedRoots::single(AuthorizedRoot::open_absolute(&root).unwrap());
    let run = TestContext::new(30_000, 500);
    let entries = (0..254)
        .map(|i| {
            let name = format!("synthetic{i:03}");
            let filename = format!("{name}.md");
            let description: String =
                format!("Synthetic parser technique {i}. Validate nested lists and round trips. ")
                    .repeat(30)
                    .chars()
                    .take(1000)
                    .collect();
            let body: String =
                "Synthetic procedure: inspect parsing boundaries and test malformed nested lists. "
                    .repeat(20)
                    .chars()
                    .take(700)
                    .collect();
            std::fs::write(
                root.join(&filename),
                format!("---\nname: {name}\ndescription: {description}\n---\n{body}\n"),
            )
            .unwrap();
            SkillEntry::from_read(
                BindingSpec {
                    source: SourceId::new("synthetic-capacity").unwrap(),
                    logical_key: LogicalSkillKey::new(name.as_str()).unwrap(),
                    invocation: InvocationName::new(name.as_str()).unwrap(),
                    priority: Some(1),
                    visibility: Visibility::Verified {
                        contract_version: "synthetic-only-v1".into(),
                    },
                    restrictions: InvocationRestrictions {
                        agent_invocable: true,
                        user_invocable: true,
                    },
                },
                roots
                    .read_bounded(
                        0,
                        Path::new(&filename),
                        skillranker::limits::SKILL_FILE_BYTES,
                    )
                    .unwrap(),
            )
            .unwrap()
        })
        .collect();
    let roster = ResolvedRoster::resolve(entries, false, &run.cx, &run.clock).unwrap();
    let ids: Vec<_> = roster.advisory().map(|s| s.binding.id.clone()).collect();
    assert_eq!(ids.len(), 254);
    let state = RenderedContextPayload {
        schema_version: 1,
        context_profile: ContextProfile::Standard,
        harness: "synthetic".into(),
        context_quality: ContextQuality::Complete,
        project_signals: Default::default(),
        session_state: Default::default(),
        recent_messages: Vec::new(),
        latest_user_request:
            "Synthetic task: repair a parser for nested lists, preserving round trips. "
                .repeat(200)
                .chars()
                .take(12_000)
                .collect(),
    };
    let wide = skillranker::jev::wide::build(&roster, &ids, &state, DEFAULT_MODEL, true).unwrap();
    let rerank =
        skillranker::jev::rerank::build(&roster, &ids[..32], &state, DEFAULT_MODEL).unwrap();
    let requests = [wide.request().clone(), rerank.request().clone()];
    run.finish();
    requests
}

#[test]
fn capacity_shapes_use_production_builders_and_stay_bounded() {
    let requests = capacity_requests();
    let independently_located = capacity_requests();
    for (a, b) in requests.iter().zip(&independently_located) {
        assert_eq!(
            a.to_json().unwrap(),
            b.to_json().unwrap(),
            "local fixture paths must not affect provider bytes"
        );
    }
    for (request, questions, options) in [(&requests[0], 6, 255), (&requests[1], 33, 33)] {
        assert_eq!(request.questions().len(), questions);
        let bytes = request.to_json().unwrap();
        assert!(bytes.len() <= MAX_REQUEST_BYTES);
        assert!(
            bytes.len() > 40_000,
            "probe must exercise substantial request size"
        );
        let max_options = request
            .questions()
            .values()
            .filter_map(|q| match q {
                Question::Choice { criteria, .. } => Some(criteria.len()),
                _ => None,
            })
            .max()
            .unwrap();
        assert_eq!(max_options, options);
    }
}

#[test]
#[ignore = "two live paid requests: explicit capacity consent and exported key required"]
fn budgeted_live_capacity_shapes() {
    let api_key = live_api_key(std::env::var_os("SKILLRANKER_CAPACITY_CONSENT"), || {
        std::env::var_os("TYPESAFE_API_KEY")
    }).unwrap_or_else(|_| panic!("capacity probe requires SKILLRANKER_CAPACITY_CONSENT=1 and an exported TYPESAFE_API_KEY"));
    let requests = capacity_requests();
    let endpoint = EndpointConfig::production();
    let origin = endpoint.origin().clone();
    let key = resolve_credential(&origin, &api_key);
    let client = JevClient::new(endpoint).unwrap();
    let run = TestContext::new(30_000, 500);
    let mut admission = AttemptAdmission::new(
        AttemptBudget::new(2, 2).unwrap(),
        run.clock,
        "synthetic-capacity",
    )
    .unwrap();
    for (request, stage) in requests
        .iter()
        .zip([RankingStage::Wide, RankingStage::Rerank])
    {
        let bytes = request.to_json().unwrap();
        eprintln!(
            "{}",
            json!({"kind":"synthetic-capacity-attempt", "stage":stage.as_str(), "request_bytes":bytes.len(), "request_blake3":blake3::hash(&bytes).to_hex().to_string(), "questions":request.questions().len(), "requested_model":DEFAULT_MODEL})
        );
        let permit = admission.admit(stage, &origin).unwrap();
        let started = std::time::Instant::now();
        let result = run.send(
            &client,
            request,
            Some(&key),
            NetworkConsent::Authorized(ConsentSource::AllowNetworkFlag),
        );
        let response = match result {
            Ok(response) => {
                let sent = permit.mark_sent().unwrap();
                admission.record_sent(&sent).unwrap();
                admission.record_response(&sent, response.usage).unwrap();
                response
            }
            Err(error) => {
                if error.http_attempt_started {
                    let sent = permit.mark_sent().unwrap();
                    admission.record_sent(&sent).unwrap();
                    admission
                        .record_terminal_failure(&sent, "capacity probe failed")
                        .unwrap();
                } else {
                    admission
                        .record_discard(&permit.discard_before_send("local refusal"))
                        .unwrap();
                }
                run.finish();
                panic!(
                    "capacity stage {stage} failed: {:?}; http_attempt_started={}; usage_unknown={}",
                    error.kind, error.http_attempt_started, error.http_attempt_started
                );
            }
        };
        assert_eq!(response.answers.len(), request.questions().len());
        let bytes = request.to_json().unwrap();
        eprintln!(
            "{}",
            json!({"kind":"synthetic-capacity-stage", "stage":stage.as_str(), "request_bytes":bytes.len(), "request_blake3":blake3::hash(&bytes).to_hex().to_string(), "questions":request.questions().len(), "returned_model_blake3":blake3::hash(response.returned_model.as_bytes()).to_hex().to_string(), "input_tokens":response.usage.input_tokens, "output_tokens":response.usage.output_tokens, "elapsed_ms":started.elapsed().as_millis()})
        );
    }
    assert_eq!(admission.receipt().sent_attempts, 2);
    run.finish();
}
