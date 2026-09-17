//! Deterministic synthetic protocol fixtures plus one recorded live provider
//! capture (see tests/fixtures/jev-recorded-provenance.v1.json).

use serde_json::{Value, json};
use skillranker::jev::codec::{
    Answer, ChoiceAnswer, CodecError, MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES, NoulCriteria,
    Question, Request, Response, Usage,
};

fn error<T>(result: Result<T, CodecError>, expected: CodecError) {
    match result {
        Err(actual) => assert_eq!(actual, expected),
        Ok(_) => panic!("expected codec rejection: {expected:?}"),
    }
}

fn request_value() -> Value {
    json!({
        "model": "synthetic-alias",
        "state": {"task": "synthetic ranking example"},
        "questions": {"rank": {
            "type": "choice", "instructions": "Select one",
            "criteria": {"a": "First", "b": "Second"}
        }}
    })
}

fn request() -> Request {
    Request::from_json(&serde_json::to_vec(&request_value()).unwrap()).unwrap()
}

fn response_value() -> Value {
    json!({
        "model": "synthetic-resolved-model",
        "answers": {"rank": {
            "type": "choice", "choice": "a",
            "probabilities": {"a": 0.75, "b": 0.25}, "confidence": 0.8
        }},
        "usage": {"input_tokens": 12, "output_tokens": 3}
    })
}

fn decode(request: &Request, value: &Value) -> Result<Response, CodecError> {
    request.decode_response(&serde_json::to_vec(value).unwrap())
}

fn choice(response: &Response) -> &ChoiceAnswer {
    match &response.answers["rank"] {
        Answer::Choice(answer) => answer,
        Answer::Noul(_) => panic!("rank must remain Choice"),
    }
}

fn noul() -> Question {
    Question::Noul {
        instructions: json!("Does the synthetic example need help?"),
        criteria: None,
    }
}

fn nested(arrays: usize) -> Value {
    (0..arrays).fold(json!("synthetic"), |value, _| json!([value]))
}

#[test]
fn minimal_noul_request_and_decode_response_succeed() {
    let original = Request::new(
        "synthetic-alias".into(),
        json!([{"task": ["synthetic", "例"]}]),
        [
            (
                "rank".into(),
                Question::choice(
                    json!({"instructions": ["Select one", {"detail": "例"}]}),
                    [("a".into(), "First".into()), ("b".into(), "Second".into())],
                )
                .unwrap(),
            ),
            (
                "stuck".into(),
                Question::Noul {
                    instructions: json!(["Need help?"]),
                    criteria: Some(NoulCriteria {
                        yes: Some("Help needed".into()),
                        no: Some("No help needed".into()),
                    }),
                },
            ),
        ],
    )
    .unwrap();
    let encoded = original.to_json().unwrap();
    let wire: Value = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(wire["state"], json!([{"task": ["synthetic", "例"]}]));
    assert_eq!(
        wire["questions"]["stuck"]["criteria"],
        json!({
            "true": "Help needed", "false": "No help needed"
        })
    );
    let restored = Request::from_json(&encoded).unwrap();
    assert_eq!(restored.to_json().unwrap(), encoded);
    let mut value = response_value();
    value["answers"]["stuck"] = json!({"type": "noul", "noul": 0.2});
    let response = decode(&restored, &value).unwrap();
    assert_eq!(response.requested_model, "synthetic-alias");
    assert_eq!(response.returned_model, "synthetic-resolved-model");
    assert_eq!(choice(&response).choice(), "a");
    assert!(matches!(response.answers["stuck"], Answer::Noul(value) if value == 0.2));
    assert_eq!(
        response.usage,
        Usage {
            input_tokens: 12,
            output_tokens: 3
        }
    );
}

#[test]
fn stuck_is_optional_but_exactly_required_when_requested() {
    let without = request();
    assert_eq!(
        choice(&decode(&without, &response_value()).unwrap()).choice(),
        "a"
    );
    let mut with_stuck = response_value();
    with_stuck["answers"]["stuck"] = json!({"type": "noul", "noul": 0.0});
    error(decode(&without, &with_stuck), CodecError::QuestionMismatch);

    let mut questions = without.questions().clone();
    questions.insert("stuck".into(), noul());
    let with = Request::new(without.model().into(), json!("synthetic"), questions).unwrap();
    let encoded: Value = serde_json::from_slice(&with.to_json().unwrap()).unwrap();
    assert!(encoded["questions"]["stuck"].get("criteria").is_none());
    error(
        decode(&with, &response_value()),
        CodecError::QuestionMismatch,
    );
    assert!(matches!(
        decode(&with, &with_stuck).unwrap().answers["stuck"],
        Answer::Noul(0.0)
    ));
}

#[test]
fn choice_preserves_raw_drift_and_exposes_separate_normalization() {
    let request = request();
    for (a, b) in [(0.75, 0.25), (0.6, 0.40005), (0.6, 0.39995)] {
        let mut value = response_value();
        value["answers"]["rank"]["probabilities"] = json!({"a": a, "b": b});
        let response = decode(&request, &value).unwrap();
        let answer = choice(&response);
        assert_eq!(answer.raw_probabilities()["a"], a);
        assert_eq!(answer.raw_probabilities()["b"], b);
        assert!((answer.normalized_probability("a").unwrap() - a / (a + b)).abs() < 1e-15);
        assert!((answer.normalized_probability("b").unwrap() - b / (a + b)).abs() < 1e-15);
        assert_eq!(answer.normalized_probability("foreign"), None);
        assert_eq!(answer.confidence(), 0.8);
    }
    for (a, b) in [(0.0, 0.0), (0.6, 0.40011), (0.6, 0.39989)] {
        let mut value = response_value();
        value["answers"]["rank"]["probabilities"] = json!({"a": a, "b": b});
        error(decode(&request, &value), CodecError::InvalidDistribution);
    }
}

#[test]
fn duplicate_request_keys_are_rejected_before_known_or_unknown_fields_collapse() {
    let base = r#"{"model":"synthetic","state":{"item":1},"questions":{"rank":{"type":"choice","instructions":{"text":"synthetic"},"criteria":{"a":"First","b":"Second"}}}}"#;
    for (before, after) in [
        (
            r#""model":"synthetic""#,
            r#""model":"synthetic","model":"other""#,
        ),
        (r#""item":1"#, r#""item":1,"item":2"#),
        (
            r#""text":"synthetic""#,
            r#""text":"synthetic","text":"other""#,
        ),
        (r#""a":"First""#, r#""a":"First","\u0061":"Second""#),
        (
            r#""rank":{"type""#,
            r#""rank":{"type":"noul","instructions":"other"},"rank":{"type""#,
        ),
        (
            r#""model":"synthetic""#,
            r#""metadata":{"nested":[{"x":1,"x":2}]},"model":"synthetic""#,
        ),
    ] {
        error(
            Request::from_json(base.replace(before, after).as_bytes()),
            CodecError::InvalidJson,
        );
    }
    assert_eq!(
        Request::from_json(base.as_bytes()).unwrap().model(),
        "synthetic"
    );
    let duplicate_options = [("a".into(), "First".into()), ("a".into(), "Second".into())];
    error(
        Question::choice(json!("synthetic"), duplicate_options),
        CodecError::DuplicateId,
    );
    error(
        Request::new(
            "synthetic".into(),
            json!("state"),
            [("q".into(), noul()), ("q".into(), noul())],
        ),
        CodecError::DuplicateId,
    );
}

#[test]
fn duplicate_response_keys_are_rejected_even_inside_additive_metadata() {
    let request = request();
    let base = r#"{"model":"synthetic","answers":{"rank":{"type":"choice","choice":"a","probabilities":{"a":0.75,"b":0.25},"confidence":0.8}},"usage":{"input_tokens":12,"output_tokens":3}}"#;
    for (before, after) in [
        (
            r#""model":"synthetic""#,
            r#""model":"synthetic","model":"other""#,
        ),
        (
            r#""rank":{"type""#,
            r#""rank":{"type":"noul","noul":0.5},"rank":{"type""#,
        ),
        (r#""a":0.75"#, r#""a":0.75,"\u0061":0.75"#),
        (
            r#""confidence":0.8"#,
            r#""confidence":0.8,"confidence":0.7"#,
        ),
        (
            r#""input_tokens":12"#,
            r#""input_tokens":12,"input_tokens":0"#,
        ),
        (
            r#""confidence":0.8"#,
            r#""metadata":{"nested":[{"x":1,"x":2}]},"confidence":0.8"#,
        ),
        (
            r#""model":"synthetic""#,
            r#""metadata":{"x":1,"x":2},"model":"synthetic""#,
        ),
    ] {
        error(
            request.decode_response(base.replace(before, after).as_bytes()),
            CodecError::InvalidJson,
        );
    }
    assert_eq!(
        choice(&request.decode_response(base.as_bytes()).unwrap()).choice(),
        "a"
    );
}

#[test]
fn question_option_and_answer_type_maps_must_match_the_request() {
    let request = request();
    for answers in [
        json!({}),
        json!({"foreign": response_value()["answers"]["rank"]}),
        json!({"rank": response_value()["answers"]["rank"], "foreign": {"type": "noul", "noul": 0.5}}),
        json!({"rank": {"type": "noul", "noul": 0.5}}),
    ] {
        let mut value = response_value();
        value["answers"] = answers;
        error(decode(&request, &value), CodecError::QuestionMismatch);
    }
    for probabilities in [
        json!({}),
        json!({"a": 1.0}),
        json!({"a": 0.75, "foreign": 0.25}),
        json!({"a": 0.75, "b": 0.25, "foreign": 0.0}),
    ] {
        let mut value = response_value();
        value["answers"]["rank"]["probabilities"] = probabilities;
        error(decode(&request, &value), CodecError::OptionMismatch);
    }
    for field in ["type", "choice", "probabilities", "confidence"] {
        let mut value = response_value();
        value["answers"]["rank"]
            .as_object_mut()
            .unwrap()
            .remove(field);
        error(decode(&request, &value), CodecError::InvalidAnswer);
    }
    for kind in ["score", "unknown", "Choice"] {
        let mut value = response_value();
        value["answers"]["rank"]["type"] = json!(kind);
        error(decode(&request, &value), CodecError::InvalidAnswer);
    }
    let noul_request = Request::new(
        "synthetic".into(),
        json!("state"),
        [("rank".into(), noul())],
    )
    .unwrap();
    error(
        decode(&noul_request, &response_value()),
        CodecError::QuestionMismatch,
    );
    assert_eq!(
        choice(&decode(&request, &response_value()).unwrap()).raw_probabilities()["b"],
        0.25
    );
}

#[test]
fn malformed_request_maps_and_deferred_score_are_not_accepted() {
    for field in ["model", "state", "questions"] {
        let mut value = request_value();
        value.as_object_mut().unwrap().remove(field);
        error(
            Request::from_json(&serde_json::to_vec(&value).unwrap()),
            CodecError::InvalidRequest,
        );
    }
    for questions in [
        json!({}),
        json!([]),
        json!({"rank": {"instructions": "synthetic", "criteria": {"a": "A"}}}),
        json!({"rank": {"type": "score", "instructions": "synthetic", "criteria": ["low", "high"]}}),
        json!({"rank": {"type": "choice", "instructions": "synthetic"}}),
        json!({"rank": {"type": "choice", "instructions": "synthetic", "criteria": {"a": 1}}}),
    ] {
        let mut value = request_value();
        value["questions"] = questions;
        error(
            Request::from_json(&serde_json::to_vec(&value).unwrap()),
            CodecError::InvalidRequest,
        );
    }
    assert_eq!(
        request()
            .questions()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["rank"]
    );
}

#[test]
fn usage_requires_both_nonnegative_integral_u64_counts() {
    let request = request();
    let mut value = response_value();
    value.as_object_mut().unwrap().remove("usage");
    error(decode(&request, &value), CodecError::InvalidAnswer);
    for field in ["input_tokens", "output_tokens"] {
        let mut value = response_value();
        value["usage"].as_object_mut().unwrap().remove(field);
        error(decode(&request, &value), CodecError::InvalidAnswer);
        for invalid in [json!(-1), json!(0.5), json!("1"), Value::Null] {
            value = response_value();
            value["usage"][field] = invalid;
            error(decode(&request, &value), CodecError::InvalidAnswer);
        }
        value = response_value();
        value["usage"][field] = json!("OVERFLOW");
        let bytes = serde_json::to_string(&value)
            .unwrap()
            .replace("\"OVERFLOW\"", "18446744073709551616");
        error(
            request.decode_response(bytes.as_bytes()),
            CodecError::InvalidAnswer,
        );
    }
    for (input, output) in [(0, u64::MAX), (u64::MAX, 0)] {
        let mut value = response_value();
        value["usage"] = json!({"input_tokens": input, "output_tokens": output});
        assert_eq!(
            decode(&request, &value).unwrap().usage,
            Usage {
                input_tokens: input,
                output_tokens: output
            }
        );
    }
}

#[test]
fn additive_metadata_does_not_replace_required_protocol_fields() {
    let mut request_wire = request_value();
    request_wire["metadata"] = json!({"nested": [{"new": true}]});
    request_wire["questions"]["rank"]["metadata"] = json!({"future": "synthetic"});
    let request = Request::from_json(&serde_json::to_vec(&request_wire).unwrap()).unwrap();
    let mut value = response_value();
    value["metadata"] = json!({"future": [1, 2, 3]});
    value["answers"]["rank"]["metadata"] = json!({"new": "synthetic"});
    value["usage"]["metadata"] = json!({"future": 7});
    let response = decode(&request, &value).unwrap();
    assert_eq!(choice(&response).choice(), "a");
    assert_eq!(response.usage.input_tokens, 12);
    value["usage"]
        .as_object_mut()
        .unwrap()
        .remove("input_tokens");
    value["usage"]["metadata"]["input_tokens"] = json!(12);
    error(decode(&request, &value), CodecError::InvalidAnswer);
}

#[test]
fn probabilities_and_confidence_accept_endpoints_but_reject_out_of_range_values() {
    let request = request();
    for (a, b, selected, confidence) in [(1.0, 0.0, "a", 0.0), (0.0, 1.0, "b", 1.0)] {
        let mut value = response_value();
        value["answers"]["rank"]["probabilities"] = json!({"a": a, "b": b});
        value["answers"]["rank"]["choice"] = json!(selected);
        value["answers"]["rank"]["confidence"] = json!(confidence);
        let response = decode(&request, &value).unwrap();
        assert_eq!(choice(&response).choice(), selected);
        assert_eq!(
            choice(&response).normalized_probability(selected),
            Some(1.0)
        );
        assert_eq!(choice(&response).confidence(), confidence);
    }
    for invalid in [-f64::EPSILON, 1.0 + f64::EPSILON] {
        for pointer in ["/answers/rank/confidence", "/answers/rank/probabilities/a"] {
            let mut value = response_value();
            *value.pointer_mut(pointer).unwrap() = json!(invalid);
            error(decode(&request, &value), CodecError::InvalidProbability);
        }
    }
    let request = Request::new(
        "synthetic".into(),
        json!("state"),
        [("rank".into(), noul())],
    )
    .unwrap();
    for probability in [0.0, -0.0, 1.0] {
        let mut value = response_value();
        value["answers"]["rank"] = json!({"type": "noul", "noul": probability});
        assert!(
            matches!(decode(&request, &value).unwrap().answers["rank"], Answer::Noul(actual) if actual == probability)
        );
    }
    for probability in [-f64::EPSILON, 1.0 + f64::EPSILON] {
        let mut value = response_value();
        value["answers"]["rank"] = json!({"type": "noul", "noul": probability});
        error(decode(&request, &value), CodecError::InvalidProbability);
    }
    for token in ["NaN", "Infinity", "-Infinity", "1e309"] {
        let raw = format!(
            r#"{{"model":"synthetic","answers":{{"rank":{{"type":"noul","noul":{token}}}}},"usage":{{"input_tokens":0,"output_tokens":0}}}}"#
        );
        error(
            request.decode_response(raw.as_bytes()),
            CodecError::InvalidJson,
        );
    }
}

#[test]
fn chosen_option_must_be_an_argmax_and_exact_ties_are_allowed() {
    let request = request();
    for selected in ["b", "foreign"] {
        let mut value = response_value();
        value["answers"]["rank"]["choice"] = json!(selected);
        error(decode(&request, &value), CodecError::InvalidChoice);
    }
    for selected in ["a", "b"] {
        let mut value = response_value();
        value["answers"]["rank"]["probabilities"] = json!({"a": 0.5, "b": 0.5});
        value["answers"]["rank"]["choice"] = json!(selected);
        assert_eq!(
            choice(&decode(&request, &value).unwrap()).choice(),
            selected
        );
    }
    let mut value = response_value();
    value["answers"]["rank"]["probabilities"] = json!({"a": 0.499999999999, "b": 0.500000000001});
    error(decode(&request, &value), CodecError::InvalidChoice);
    value["answers"]["rank"]["choice"] = json!("b");
    assert_eq!(choice(&decode(&request, &value).unwrap()).choice(), "b");
}

#[test]
fn option_limit_includes_the_explicit_none_sentinel() {
    for count in [0, 1, 254, 255, 256] {
        let options: Vec<_> = (0..count)
            .map(|index| {
                let id = if index == 0 {
                    "__none__".into()
                } else {
                    format!("skill_{index}")
                };
                (id, "synthetic option".into())
            })
            .collect();
        let question = Question::choice(json!("Select synthetic option"), options);
        if count == 0 || count == 256 {
            error(question, CodecError::InvalidRequest);
            continue;
        }
        let request = Request::new(
            "synthetic".into(),
            json!("state"),
            [("rank".into(), question.unwrap())],
        )
        .unwrap();
        let probabilities: serde_json::Map<String, Value> = (0..count)
            .map(|index| {
                if index == 0 {
                    ("__none__".into(), json!(1.0))
                } else {
                    (format!("skill_{index}"), json!(0.0))
                }
            })
            .collect();
        let mut value = response_value();
        value["answers"]["rank"]["choice"] = json!("__none__");
        value["answers"]["rank"]["probabilities"] = Value::Object(probabilities);
        let response = decode(&request, &value).unwrap();
        assert_eq!(choice(&response).choice(), "__none__");
        assert_eq!(
            choice(&response).normalized_probability("__none__"),
            Some(1.0)
        );
        assert_eq!(choice(&response).raw_probabilities().len(), count);
        value["answers"]["rank"]["probabilities"]
            .as_object_mut()
            .unwrap()
            .remove("__none__");
        error(decode(&request, &value), CodecError::OptionMismatch);
    }
}

#[test]
fn request_and_response_byte_caps_include_whitespace_and_utf8_bytes() {
    let request = request();
    let mut request_bytes = request.to_json().unwrap();
    request_bytes.resize(MAX_REQUEST_BYTES, b' ');
    assert_eq!(
        Request::from_json(&request_bytes).unwrap().model(),
        "synthetic-alias"
    );
    request_bytes.push(b' ');
    error(Request::from_json(&request_bytes), CodecError::TooLarge);

    let mut response_bytes = serde_json::to_vec(&response_value()).unwrap();
    response_bytes.resize(MAX_RESPONSE_BYTES, b' ');
    assert_eq!(
        choice(&request.decode_response(&response_bytes).unwrap()).choice(),
        "a"
    );
    response_bytes.push(b' ');
    error(
        request.decode_response(&response_bytes),
        CodecError::TooLarge,
    );

    let build =
        |state: String| Request::new("synthetic".into(), json!(state), [("rank".into(), noul())]);
    let overhead = build(String::new()).unwrap().to_json().unwrap().len();
    let available = MAX_REQUEST_BYTES - overhead;
    let state = "é".repeat(available / 2) + &"x".repeat(available % 2);
    let at_cap = build(state.clone()).unwrap();
    assert_eq!(at_cap.to_json().unwrap().len(), MAX_REQUEST_BYTES);
    let wire: Value = serde_json::from_slice(&at_cap.to_json().unwrap()).unwrap();
    assert_eq!(wire["state"], json!(state));
    error(build(state + "x"), CodecError::TooLarge);
}

#[test]
fn depth_limit_applies_to_local_state_and_unknown_response_metadata() {
    let build = |state| Request::new("synthetic".into(), state, [("rank".into(), noul())]);
    // Root is depth zero: 63 arrays below a root field put the leaf at depth 64.
    let accepted = build(nested(63)).unwrap();
    let wire: Value = serde_json::from_slice(&accepted.to_json().unwrap()).unwrap();
    assert_eq!(wire["state"], nested(63));
    error(build(nested(64)), CodecError::InvalidJson);
    let mut raw_request = request_value();
    raw_request["metadata"] = nested(63);
    assert_eq!(
        Request::from_json(&serde_json::to_vec(&raw_request).unwrap())
            .unwrap()
            .model(),
        "synthetic-alias"
    );
    raw_request["metadata"] = nested(64);
    error(
        Request::from_json(&serde_json::to_vec(&raw_request).unwrap()),
        CodecError::InvalidJson,
    );

    let request = request();
    let mut value = response_value();
    value["metadata"] = nested(63);
    assert_eq!(choice(&decode(&request, &value).unwrap()).choice(), "a");
    value["metadata"] = nested(64);
    error(decode(&request, &value), CodecError::InvalidJson);
}

#[test]
fn recorded_live_capture_decodes_with_distinct_model_identity() {
    let request_bytes = std::fs::read("tests/fixtures/jev-recorded-request.v1.json")
        .expect("recorded request fixture");
    let response_bytes = std::fs::read("tests/fixtures/jev-recorded-response.v1.json")
        .expect("recorded response fixture");
    let provenance = recorded_provenance();
    assert_eq!(blake3_hex(&request_bytes), provenance["request_blake3"]);
    assert_eq!(blake3_hex(&response_bytes), provenance["response_blake3"]);
    let request = Request::from_json(&request_bytes).unwrap();
    let response = request.decode_response(&response_bytes).unwrap();
    assert_eq!(response.requested_model, "jev-latest");
    assert_eq!(response.returned_model, "jev-1.13.0");
    assert_ne!(response.requested_model, response.returned_model);
    let (shape, round) = match (response.answers.get("shape"), response.answers.get("round")) {
        (Some(Answer::Choice(shape)), Some(Answer::Noul(round))) => (shape, *round),
        _ => panic!("recorded answers must keep their requested types"),
    };
    assert_eq!(shape.choice(), "circle");
    assert!((shape.confidence() - 1.0).abs() < f64::EPSILON);
    assert_eq!(shape.normalized_probability("circle").unwrap(), 1.0);
    assert_eq!(shape.normalized_probability("square"), Some(0.0));
    assert_eq!(shape.normalized_probability("__none__"), Some(0.0));
    assert_eq!(round, 0.99);
    assert_eq!(
        response.usage,
        Usage {
            input_tokens: 372,
            output_tokens: 57,
        }
    );
}

fn blake3_hex(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_string()
}

fn recorded_provenance() -> Value {
    serde_json::from_str(
        &std::fs::read_to_string("tests/fixtures/jev-recorded-provenance.v1.json")
            .expect("recorded provenance fixture"),
    )
    .unwrap()
}
