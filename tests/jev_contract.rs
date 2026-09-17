//! Integration and property tests for TypeSafe Jev distribution, confidence,
//! and usage validation before scoring.
//!
//! Satisfies contract boundary `p1_distribution_validation` (sr-roadmap-l1i.2.6)
//! mapped in `tests/contract_matrix.toml`.

use serde_json::{Value, json};
use skillranker::jev::CostReceipt;
use skillranker::jev::codec::{
    Answer, CodecError, MAX_CHOICE_OPTIONS, MAX_RESPONSE_BYTES, NoulCriteria, Question, Request,
    Response, SUM_TOLERANCE, Usage,
};
use skillranker::output::{CliExit, ErrorKind};

const CANARY: &str = "canary-secret-token-77441199";

fn assert_canary_not_leaked(text: &str) {
    assert!(
        !text.contains(CANARY),
        "security violation: private text leaked in diagnostic: {text}"
    );
}

fn expect_err<T>(result: Result<T, CodecError>) -> CodecError {
    match result {
        Err(err) => err,
        Ok(_) => panic!("expected CodecError rejection, got Ok"),
    }
}

fn sample_request() -> Request {
    Request::new(
        "synthetic-model".into(),
        json!({"task": "sample-ranking"}),
        [
            (
                "rank".into(),
                Question::choice(
                    json!("Rank candidates"),
                    [
                        ("skill_alpha".into(), "Alpha skill".into()),
                        ("skill_beta".into(), "Beta skill".into()),
                        ("__none__".into(), "None of the above".into()),
                    ],
                )
                .unwrap(),
            ),
            (
                "stuck".into(),
                Question::Noul {
                    instructions: json!("Is agent stuck?"),
                    criteria: Some(NoulCriteria {
                        yes: Some("Agent needs help".into()),
                        no: Some("Agent making progress".into()),
                    }),
                },
            ),
        ],
    )
    .expect("sample request must construct")
}

fn valid_response_json() -> Value {
    json!({
        "model": "synthetic-model-resolved",
        "answers": {
            "rank": {
                "type": "choice",
                "choice": "skill_alpha",
                "probabilities": {
                    "skill_alpha": 0.60,
                    "skill_beta": 0.30,
                    "__none__": 0.10
                },
                "confidence": 0.85
            },
            "stuck": {
                "type": "noul",
                "noul": 0.05
            }
        },
        "usage": {
            "input_tokens": 150,
            "output_tokens": 42
        }
    })
}

fn decode_val(req: &Request, val: &Value) -> Result<Response, CodecError> {
    req.decode_response(&serde_json::to_vec(val).unwrap())
}

// -----------------------------------------------------------------------------
// 1. Boundary: p1_distribution_validation
// Unit Property Test: tests/jev_contract.rs::distribution_sum_and_confidence
// -----------------------------------------------------------------------------

#[test]
fn distribution_sum_and_confidence() {
    let req = sample_request();

    // Exact sum = 1.0 with valid confidence
    let base = valid_response_json();
    let resp = decode_val(&req, &base).expect("exact sum 1.0 must decode");
    let rank = match &resp.answers["rank"] {
        Answer::Choice(c) => c,
        _ => panic!("expected choice answer"),
    };
    assert_eq!(rank.choice(), "skill_alpha");
    assert_eq!(rank.confidence(), 0.85);
    assert!((rank.raw_sum() - 1.0).abs() < 1e-9);
    assert_eq!(rank.raw_probabilities()["skill_alpha"], 0.60);
    assert_eq!(rank.raw_probabilities()["skill_beta"], 0.30);
    assert_eq!(rank.raw_probabilities()["__none__"], 0.10);

    // Normalized probabilities match raw when sum is exactly 1.0
    let norm = rank.normalized_probabilities();
    assert!((norm["skill_alpha"] - 0.60).abs() < 1e-9);
    assert!((norm["skill_beta"] - 0.30).abs() < 1e-9);
    assert!((norm["__none__"] - 0.10).abs() < 1e-9);

    // Sum within tolerance 1e-4 (positive drift: sum = 1.00009)
    let mut val_drift_up = valid_response_json();
    val_drift_up["answers"]["rank"]["probabilities"]["skill_alpha"] = json!(0.60009);
    let resp_up = decode_val(&req, &val_drift_up).expect("drift within 1e-4 must be accepted");
    if let Answer::Choice(c) = &resp_up.answers["rank"] {
        assert_eq!(c.raw_probabilities()["skill_alpha"], 0.60009);
        assert!((c.raw_sum() - 1.00009).abs() < 1e-9);
        // Normalized probability divides by raw_sum
        let norm_alpha = c.normalized_probability("skill_alpha").unwrap();
        assert!((norm_alpha - (0.60009 / 1.00009)).abs() < 1e-9);
    } else {
        panic!("expected choice answer");
    }

    // Sum within tolerance 1e-4 (negative drift: sum = 0.99991)
    let mut val_drift_down = valid_response_json();
    val_drift_down["answers"]["rank"]["probabilities"]["skill_alpha"] = json!(0.59991);
    let resp_down = decode_val(&req, &val_drift_down).expect("drift within 1e-4 must be accepted");
    if let Answer::Choice(c) = &resp_down.answers["rank"] {
        assert_eq!(c.raw_probabilities()["skill_alpha"], 0.59991);
        assert!((c.raw_sum() - 0.99991).abs() < 1e-9);
        let norm_alpha = c.normalized_probability("skill_alpha").unwrap();
        assert!((norm_alpha - (0.59991 / 0.99991)).abs() < 1e-9);
    } else {
        panic!("expected choice answer");
    }

    // Boundary edge: exactly 1.0 + SUM_TOLERANCE (1.00010)
    let mut val_tol_up = valid_response_json();
    val_tol_up["answers"]["rank"]["probabilities"]["skill_alpha"] = json!(0.60010);
    assert!(decode_val(&req, &val_tol_up).is_ok());

    // Boundary edge: exactly 1.0 - SUM_TOLERANCE (0.99990)
    let mut val_tol_down = valid_response_json();
    val_tol_down["answers"]["rank"]["probabilities"]["skill_alpha"] = json!(0.59990);
    assert!(decode_val(&req, &val_tol_down).is_ok());

    // Beyond tolerance: 1.0 + 1.01e-4 (1.00011) must be rejected
    let mut val_excess_up = valid_response_json();
    val_excess_up["answers"]["rank"]["probabilities"]["skill_alpha"] = json!(0.60011);
    let err_excess_up = expect_err(decode_val(&req, &val_excess_up));
    assert_eq!(err_excess_up, CodecError::InvalidDistribution);
    assert_eq!(err_excess_up.kind(), ErrorKind::InvalidProviderResponse);
    assert_eq!(err_excess_up.exit_code(), CliExit::ProviderContract);

    // Beyond tolerance: 1.0 - 1.01e-4 (0.99989) must be rejected
    let mut val_excess_down = valid_response_json();
    val_excess_down["answers"]["rank"]["probabilities"]["skill_alpha"] = json!(0.59989);
    let err_excess_down = expect_err(decode_val(&req, &val_excess_down));
    assert_eq!(err_excess_down, CodecError::InvalidDistribution);

    // Zero sum (all zero) must be rejected
    let mut val_zero_sum = valid_response_json();
    val_zero_sum["answers"]["rank"]["probabilities"] = json!({
        "skill_alpha": 0.0,
        "skill_beta": 0.0,
        "__none__": 0.0
    });
    val_zero_sum["answers"]["rank"]["choice"] = json!("skill_alpha");
    assert_eq!(
        expect_err(decode_val(&req, &val_zero_sum)),
        CodecError::InvalidDistribution
    );

    // Negative sum must be rejected
    let mut val_neg_sum = valid_response_json();
    val_neg_sum["answers"]["rank"]["probabilities"]["skill_alpha"] = json!(-0.5);
    assert_eq!(
        expect_err(decode_val(&req, &val_neg_sum)),
        CodecError::InvalidProbability
    );

    // Confidence bounds: 0.0 and 1.0 are valid extremes
    let mut val_conf_zero = valid_response_json();
    val_conf_zero["answers"]["rank"]["confidence"] = json!(0.0);
    assert!(decode_val(&req, &val_conf_zero).is_ok());

    let mut val_conf_one = valid_response_json();
    val_conf_one["answers"]["rank"]["confidence"] = json!(1.0);
    assert!(decode_val(&req, &val_conf_one).is_ok());

    // Confidence out of range
    let mut val_conf_neg = valid_response_json();
    val_conf_neg["answers"]["rank"]["confidence"] = json!(-0.01);
    assert_eq!(
        expect_err(decode_val(&req, &val_conf_neg)),
        CodecError::InvalidProbability
    );

    let mut val_conf_high = valid_response_json();
    val_conf_high["answers"]["rank"]["confidence"] = json!(1.01);
    assert_eq!(
        expect_err(decode_val(&req, &val_conf_high)),
        CodecError::InvalidProbability
    );
}

// -----------------------------------------------------------------------------
// 2. Assertion ID: exact_argmax
// -----------------------------------------------------------------------------

#[test]
fn exact_argmax_validation_and_deterministic_tie_breaking() {
    let req = sample_request();

    // 1. Single argmax winner: choice MUST match the highest probability option
    let mut val = valid_response_json();
    val["answers"]["rank"]["probabilities"] = json!({
        "skill_alpha": 0.70,
        "skill_beta": 0.20,
        "__none__": 0.10
    });

    // Valid: choice is skill_alpha (0.70)
    val["answers"]["rank"]["choice"] = json!("skill_alpha");
    let resp = decode_val(&req, &val).expect("choice matching argmax is valid");
    if let Answer::Choice(c) = &resp.answers["rank"] {
        assert_eq!(c.choice(), "skill_alpha");
        assert_eq!(c.argmax_options(), vec!["skill_alpha"]);
        assert!(!c.has_tie());
        assert_eq!(c.deterministic_choice(), "skill_alpha");
    }

    // Invalid: provider returned skill_beta (0.20) when alpha is 0.70
    val["answers"]["rank"]["choice"] = json!("skill_beta");
    let err_not_argmax = expect_err(decode_val(&req, &val));
    assert_eq!(err_not_argmax, CodecError::InvalidChoice);
    assert_eq!(err_not_argmax.kind(), ErrorKind::InvalidProviderResponse);

    // Invalid: provider returned __none__ (0.10)
    val["answers"]["rank"]["choice"] = json!("__none__");
    assert_eq!(
        expect_err(decode_val(&req, &val)),
        CodecError::InvalidChoice
    );

    // Invalid: provider returned foreign option not in request
    val["answers"]["rank"]["choice"] = json!("nonexistent_skill");
    assert_eq!(
        expect_err(decode_val(&req, &val)),
        CodecError::InvalidChoice
    );

    // 2. Exact ties: multiple options share the maximum probability
    let mut val_tie = valid_response_json();
    val_tie["answers"]["rank"]["probabilities"] = json!({
        "skill_alpha": 0.45,
        "skill_beta": 0.45,
        "__none__": 0.10
    });

    // Provider choosing skill_alpha is valid (it is in the argmax set)
    val_tie["answers"]["rank"]["choice"] = json!("skill_alpha");
    let resp_tie_a = decode_val(&req, &val_tie).expect("tied choice alpha is valid");
    if let Answer::Choice(c) = &resp_tie_a.answers["rank"] {
        assert_eq!(c.choice(), "skill_alpha");
        assert_eq!(c.argmax_options(), vec!["skill_alpha", "skill_beta"]);
        assert!(c.has_tie());
        // Deterministic choice breaks tie by stable ID (lexicographical first: skill_alpha < skill_beta)
        assert_eq!(c.deterministic_choice(), "skill_alpha");
    }

    // Provider choosing skill_beta is ALSO valid (it is also in the argmax set)
    val_tie["answers"]["rank"]["choice"] = json!("skill_beta");
    let resp_tie_b = decode_val(&req, &val_tie).expect("tied choice beta is valid");
    if let Answer::Choice(c) = &resp_tie_b.answers["rank"] {
        assert_eq!(c.choice(), "skill_beta");
        assert_eq!(c.argmax_options(), vec!["skill_alpha", "skill_beta"]);
        assert!(c.has_tie());
        // Deterministic choice still stably resolves to skill_alpha
        assert_eq!(c.deterministic_choice(), "skill_alpha");
    }

    // But provider choosing __none__ (0.10) during alpha/beta tie is REJECTED
    val_tie["answers"]["rank"]["choice"] = json!("__none__");
    assert_eq!(
        expect_err(decode_val(&req, &val_tie)),
        CodecError::InvalidChoice
    );

    // 3. Three-way tie: alpha, beta, __none__ each 0.3333333333333333 (sum = 1.0)
    let p3 = 1.0 / 3.0;
    let mut val_3way = valid_response_json();
    val_3way["answers"]["rank"]["probabilities"] = json!({
        "skill_alpha": p3,
        "skill_beta": p3,
        "__none__": p3
    });
    val_3way["answers"]["rank"]["choice"] = json!("__none__");
    let resp_3way = decode_val(&req, &val_3way).expect("3-way tie is valid");
    if let Answer::Choice(c) = &resp_3way.answers["rank"] {
        assert!(c.has_tie());
        assert_eq!(c.argmax_options().len(), 3);
        // Lexicographical ordering: "__none__" < "skill_alpha" < "skill_beta"
        assert_eq!(c.deterministic_choice(), "__none__");
    }
}

// -----------------------------------------------------------------------------
// 3. Assertion ID: unknown_usage_preserved
// -----------------------------------------------------------------------------

#[test]
fn unknown_usage_preserved_across_errors() {
    let req = sample_request();

    // 1. Usage validation: non-negative integers (u64)
    let mut val_negative_tokens = valid_response_json();
    val_negative_tokens["usage"]["input_tokens"] = json!(-5);
    assert_eq!(
        expect_err(decode_val(&req, &val_negative_tokens)),
        CodecError::InvalidAnswer
    );

    let mut val_float_tokens = valid_response_json();
    val_float_tokens["usage"]["output_tokens"] = json!(12.5);
    assert_eq!(
        expect_err(decode_val(&req, &val_float_tokens)),
        CodecError::InvalidAnswer
    );

    let mut val_string_tokens = valid_response_json();
    val_string_tokens["usage"]["input_tokens"] = json!("100");
    assert_eq!(
        expect_err(decode_val(&req, &val_string_tokens)),
        CodecError::InvalidAnswer
    );

    let mut val_null_tokens = valid_response_json();
    val_null_tokens["usage"]["output_tokens"] = Value::Null;
    assert_eq!(
        expect_err(decode_val(&req, &val_null_tokens)),
        CodecError::InvalidAnswer
    );

    // Valid usage integers are preserved
    let mut val_valid = valid_response_json();
    val_valid["usage"]["input_tokens"] = json!(12345);
    val_valid["usage"]["output_tokens"] = json!(6789);
    let resp = decode_val(&req, &val_valid).unwrap();
    assert_eq!(resp.usage.input_tokens, 12345);
    assert_eq!(resp.usage.output_tokens, 6789);
    assert_eq!(resp.usage.total_tokens(), 12345 + 6789);

    // 2. Integration with CostReceipt: unknown usage preserved on terminal failure
    let mut receipt = CostReceipt::new();
    assert_eq!(receipt.unknown_usage_attempts, 0);
    assert!(!receipt.has_unknown_usage);

    // First attempt succeeds: records known tokens
    receipt.record_success(Usage {
        input_tokens: 150,
        output_tokens: 42,
    });
    assert_eq!(receipt.completed_attempts, 1);
    assert_eq!(receipt.known_usage.input_tokens, 150);
    assert_eq!(receipt.known_usage.output_tokens, 42);
    assert_eq!(receipt.unknown_usage_attempts, 0);
    assert!(!receipt.has_unknown_usage);

    // Second attempt fails with terminal error (e.g. malformed response after send):
    // Previously accumulated known usage is PRESERVED, unknown_usage_attempts is incremented
    receipt.record_terminal_error();
    assert_eq!(receipt.completed_attempts, 1);
    assert_eq!(receipt.known_usage.input_tokens, 150);
    assert_eq!(receipt.known_usage.output_tokens, 42);
    assert_eq!(receipt.unknown_usage_attempts, 1);
    assert!(receipt.has_unknown_usage);

    // Unknown provider cost is NEVER counted as zero
    let receipt_json = serde_json::to_value(&receipt).unwrap();
    assert_eq!(receipt_json["unknown_usage_attempts"], 1);
    assert_eq!(receipt_json["has_unknown_usage"], true);
    assert_eq!(receipt_json["known_usage"]["input_tokens"], 150);
    assert_eq!(receipt_json["known_usage"]["output_tokens"], 42);
}

// -----------------------------------------------------------------------------
// 4. E2E Case: nonfinite-rejected
// -----------------------------------------------------------------------------

#[test]
fn e2e_case_nonfinite_rejected() {
    let req = sample_request();

    // NaN in probability
    let nan_json = r#"{
        "model": "synthetic-model",
        "answers": {
            "rank": {
                "type": "choice",
                "choice": "skill_alpha",
                "probabilities": {
                    "skill_alpha": NaN,
                    "skill_beta": 0.5,
                    "__none__": 0.5
                },
                "confidence": 0.8
            },
            "stuck": {"type": "noul", "noul": 0.1}
        },
        "usage": {"input_tokens": 10, "output_tokens": 10}
    }"#;
    let err_nan = expect_err(req.decode_response(nan_json.as_bytes()));
    assert_eq!(err_nan, CodecError::InvalidJson);

    // Infinity in probability
    let inf_json = r#"{
        "model": "synthetic-model",
        "answers": {
            "rank": {
                "type": "choice",
                "choice": "skill_alpha",
                "probabilities": {
                    "skill_alpha": Infinity,
                    "skill_beta": 0.0,
                    "__none__": 0.0
                },
                "confidence": 0.8
            },
            "stuck": {"type": "noul", "noul": 0.1}
        },
        "usage": {"input_tokens": 10, "output_tokens": 10}
    }"#;
    let err_inf = expect_err(req.decode_response(inf_json.as_bytes()));
    assert_eq!(err_inf, CodecError::InvalidJson);

    // Non-finite in Noul
    let mut val_noul_neg = valid_response_json();
    val_noul_neg["answers"]["stuck"]["noul"] = json!(-0.01);
    assert_eq!(
        expect_err(decode_val(&req, &val_noul_neg)),
        CodecError::InvalidProbability
    );

    let mut val_noul_high = valid_response_json();
    val_noul_high["answers"]["stuck"]["noul"] = json!(1.01);
    assert_eq!(
        expect_err(decode_val(&req, &val_noul_high)),
        CodecError::InvalidProbability
    );

    // Missing answer for requested question
    let mut val_missing_q = valid_response_json();
    val_missing_q["answers"]
        .as_object_mut()
        .unwrap()
        .remove("stuck");
    assert_eq!(
        expect_err(decode_val(&req, &val_missing_q)),
        CodecError::QuestionMismatch
    );

    // Extra foreign question not requested
    let mut val_foreign_q = valid_response_json();
    val_foreign_q["answers"]["foreign_question"] = json!({"type": "noul", "noul": 0.5});
    assert_eq!(
        expect_err(decode_val(&req, &val_foreign_q)),
        CodecError::QuestionMismatch
    );

    // Answer type mismatch: requested choice, returned noul
    let mut val_type_mismatch = valid_response_json();
    val_type_mismatch["answers"]["rank"] = json!({"type": "noul", "noul": 0.5});
    assert_eq!(
        expect_err(decode_val(&req, &val_type_mismatch)),
        CodecError::QuestionMismatch
    );

    // Missing option in choice distribution
    let mut val_missing_opt = valid_response_json();
    val_missing_opt["answers"]["rank"]["probabilities"]
        .as_object_mut()
        .unwrap()
        .remove("__none__");
    assert_eq!(
        expect_err(decode_val(&req, &val_missing_opt)),
        CodecError::OptionMismatch
    );

    // Foreign option in choice distribution
    let mut val_foreign_opt = valid_response_json();
    val_foreign_opt["answers"]["rank"]["probabilities"]["intruder"] = json!(0.0);
    assert_eq!(
        expect_err(decode_val(&req, &val_foreign_opt)),
        CodecError::OptionMismatch
    );
}

// -----------------------------------------------------------------------------
// 5. E2E Case: distribution-sum-tolerance
// -----------------------------------------------------------------------------

#[test]
fn e2e_case_distribution_sum_tolerance() {
    // Test with maximum option capacity (254 skills + __none__ = 255 options)
    let option_count = MAX_CHOICE_OPTIONS;
    let mut options = Vec::new();
    for i in 0..option_count - 1 {
        options.push((format!("skill_{i:03}"), format!("Skill {i} description")));
    }
    options.push(("__none__".into(), "None sentinel".into()));

    let large_req = Request::new(
        "synthetic-model".into(),
        json!("state"),
        [(
            "rank".into(),
            Question::choice(json!("select"), options.clone()).unwrap(),
        )],
    )
    .expect("large request must construct");

    // Distribute probability evenly across all 255 options
    let p_even = 1.0 / (option_count as f64);
    let mut probs = serde_json::Map::new();
    for (id, _) in &options {
        probs.insert(id.clone(), json!(p_even));
    }

    let mut large_resp = json!({
        "model": "synthetic-model",
        "answers": {
            "rank": {
                "type": "choice",
                "choice": "skill_000",
                "probabilities": Value::Object(probs),
                "confidence": 0.9
            }
        },
        "usage": {"input_tokens": 500, "output_tokens": 100}
    });

    let resp = decode_val(&large_req, &large_resp).expect("255-option distribution must decode");
    if let Answer::Choice(c) = &resp.answers["rank"] {
        assert_eq!(c.raw_probabilities().len(), 255);
        assert!((c.raw_sum() - 1.0).abs() < SUM_TOLERANCE);
        assert!(c.has_tie());
        assert_eq!(c.argmax_options().len(), 255);
    }

    // Add small drift within tolerance to option 0
    large_resp["answers"]["rank"]["probabilities"]["skill_000"] = json!(p_even + 0.00005);
    assert!(decode_val(&large_req, &large_resp).is_ok());

    // Add drift exceeding tolerance (0.00015)
    large_resp["answers"]["rank"]["probabilities"]["skill_000"] = json!(p_even + 0.00015);
    assert_eq!(
        expect_err(decode_val(&large_req, &large_resp)),
        CodecError::InvalidDistribution
    );
}

// -----------------------------------------------------------------------------
// 6. Limits and Safety: 2 MiB Response Cap and Depth Limit
// -----------------------------------------------------------------------------

#[test]
fn response_cap_and_depth_limits() {
    let req = sample_request();

    // Response <= 2 MiB accepted (with whitespace padding)
    let valid_bytes = serde_json::to_vec(&valid_response_json()).unwrap();
    let mut padded_bytes = valid_bytes.clone();
    padded_bytes.resize(MAX_RESPONSE_BYTES, b' ');
    assert!(req.decode_response(&padded_bytes).is_ok());

    // Response > 2 MiB rejected with CodecError::TooLarge
    padded_bytes.push(b' ');
    let err_too_large = expect_err(req.decode_response(&padded_bytes));
    assert_eq!(err_too_large, CodecError::TooLarge);
    assert_eq!(err_too_large.kind(), ErrorKind::OversizedInput);

    // Deeply nested additive metadata (> 64 depth) rejected
    let nested_64 = (0..64).fold(json!("leaf"), |acc, _| json!([acc]));
    let mut val_deep = valid_response_json();
    val_deep["metadata"] = nested_64;
    let err_depth = expect_err(decode_val(&req, &val_deep));
    assert_eq!(err_depth, CodecError::InvalidJson);

    // Depth <= 63 accepted as unknown additive metadata
    let nested_63 = (0..63).fold(json!("leaf"), |acc, _| json!([acc]));
    let mut val_ok_depth = valid_response_json();
    val_ok_depth["metadata"] = nested_63;
    assert!(decode_val(&req, &val_ok_depth).is_ok());
}

// -----------------------------------------------------------------------------
// 7. Duplicate Key Rejection in Response JSON
// -----------------------------------------------------------------------------

#[test]
fn duplicate_keys_in_response_strictly_rejected() {
    let req = sample_request();

    // Duplicate answer key
    let dup_answer = r#"{
        "model": "synthetic-model",
        "answers": {
            "rank": {"type": "choice", "choice": "skill_alpha", "probabilities": {"skill_alpha": 0.6, "skill_beta": 0.3, "__none__": 0.1}, "confidence": 0.8},
            "rank": {"type": "noul", "noul": 0.5},
            "stuck": {"type": "noul", "noul": 0.05}
        },
        "usage": {"input_tokens": 10, "output_tokens": 10}
    }"#;
    assert_eq!(
        expect_err(req.decode_response(dup_answer.as_bytes())),
        CodecError::InvalidJson
    );

    // Duplicate probability key
    let dup_prob = r#"{
        "model": "synthetic-model",
        "answers": {
            "rank": {
                "type": "choice",
                "choice": "skill_alpha",
                "probabilities": {
                    "skill_alpha": 0.6,
                    "skill_alpha": 0.7,
                    "skill_beta": 0.3,
                    "__none__": 0.1
                },
                "confidence": 0.8
            },
            "stuck": {"type": "noul", "noul": 0.05}
        },
        "usage": {"input_tokens": 10, "output_tokens": 10}
    }"#;
    assert_eq!(
        expect_err(req.decode_response(dup_prob.as_bytes())),
        CodecError::InvalidJson
    );

    // Duplicate key inside unknown metadata
    let dup_meta = r#"{
        "model": "synthetic-model",
        "metadata": {"extra": 1, "extra": 2},
        "answers": {
            "rank": {"type": "choice", "choice": "skill_alpha", "probabilities": {"skill_alpha": 0.6, "skill_beta": 0.3, "__none__": 0.1}, "confidence": 0.8},
            "stuck": {"type": "noul", "noul": 0.05}
        },
        "usage": {"input_tokens": 10, "output_tokens": 10}
    }"#;
    assert_eq!(
        expect_err(req.decode_response(dup_meta.as_bytes())),
        CodecError::InvalidJson
    );
}

// -----------------------------------------------------------------------------
// 8. Error Sanitization: Canary Secrets Never Leaked in Diagnostics
// -----------------------------------------------------------------------------

#[test]
fn error_bodies_sanitized_without_raw_payload_leaks() {
    let req = sample_request();

    // Adversarial malformed response containing canary secret
    let adversarial_body = format!(
        r#"{{"error": {{"message": "Rate limited with secret token {CANARY}", "code": 429}}}}"#
    );
    let err = expect_err(req.decode_response(adversarial_body.as_bytes()));
    let err_str = format!("{err}");
    let err_debug = format!("{err:?}");

    assert_canary_not_leaked(&err_str);
    assert_canary_not_leaked(&err_debug);
    assert_eq!(err.kind(), ErrorKind::InvalidProviderResponse);
    assert_eq!(err.exit_code(), CliExit::ProviderContract);

    // Raw HTML error response containing canary
    let html_error = format!("<html><body>502 Bad Gateway: secret={CANARY}</body></html>");
    let err_html = expect_err(req.decode_response(html_error.as_bytes()));
    assert_canary_not_leaked(&format!("{err_html}"));
    assert_canary_not_leaked(&format!("{err_html:?}"));
    assert_eq!(err_html.kind(), ErrorKind::InvalidProviderResponse);

    // Adversarial foreign choice containing canary
    let mut val_canary_choice = valid_response_json();
    val_canary_choice["answers"]["rank"]["choice"] = json!(CANARY);
    let err_choice = expect_err(decode_val(&req, &val_canary_choice));
    assert_canary_not_leaked(&format!("{err_choice}"));
    assert_canary_not_leaked(&format!("{err_choice:?}"));
}
