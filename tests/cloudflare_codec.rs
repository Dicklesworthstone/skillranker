//! Native-envelope regression tests, with no network, credentials or live consent.

use serde_json::{Value, json};
use skillranker::jev::cloudflare_codec::decode_response;
use skillranker::jev::codec::{Answer, CodecError, MAX_RESPONSE_BYTES, Question, Request};

fn request() -> Request {
    Request::new(
        "typesafe/jev".into(),
        json!("synthetic state"),
        [(
            "fit".into(),
            Question::Noul {
                instructions: json!("Return a bounded synthetic score."),
                criteria: None,
            },
        )],
    )
    .unwrap()
}

fn result() -> Value {
    json!({
        "model": "jev-1.13.0",
        "answers": {"fit": {"type": "noul", "noul": 0.75}},
        "usage": {"input_tokens": 12, "output_tokens": 7}
    })
}

fn wrap(inner: &Value) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "success": true,
        "result": {"gatewayMetadata": {}, "result": inner}
    }))
    .unwrap()
}

#[test]
fn native_envelope_preserves_returned_model_answers_and_usage() {
    let response = decode_response(&request(), &wrap(&result())).unwrap();
    assert_eq!(response.requested_model, "typesafe/jev");
    assert_eq!(response.returned_model, "jev-1.13.0");
    assert_eq!(response.usage.input_tokens, 12);
    assert_eq!(response.usage.output_tokens, 7);
    assert!(matches!(response.answers["fit"], Answer::Noul(v) if v == 0.75));
}

#[test]
fn all_supported_unambiguous_shapes_have_the_same_result() {
    let mut top_level = result();
    top_level["success"] = json!(true);
    for envelope in [
        json!({"success": true, "result": {"result": result()}}),
        json!({"success": true, "result": result()}),
        top_level,
    ] {
        let response = decode_response(&request(), &serde_json::to_vec(&envelope).unwrap())
            .unwrap();
        assert_eq!(response.returned_model, "jev-1.13.0");
        assert_eq!(response.usage.input_tokens, 12);
        assert!(matches!(response.answers["fit"], Answer::Noul(v) if v == 0.75));
    }
}

#[test]
fn duplicate_definitions_are_rejected_before_translation() {
    let inner = r#"{"model":"jev-1.13.0","answers":{"fit":{"type":"noul","noul":0.75}},"usage":{"input_tokens":12,"output_tokens":7}}"#;
    let envelope = format!(r#"{{"success":true,"result":{{"result":{inner}}}}}"#);
    let variants = [
        envelope.replacen("\"success\":true", "\"success\":false,\"success\":true", 1),
        envelope.replacen("\"model\":", "\"model\":\"discarded\",\"model\":", 1),
        envelope.replacen("\"answers\":", "\"answers\":{},\"answers\":", 1),
        envelope.replacen("\"fit\":", "\"fit\":{},\"fit\":", 1),
        envelope.replacen("\"fit\":", r#""\u0066it":{},"fit":"#, 1),
        envelope.replacen("\"type\":", "\"type\":\"choice\",\"type\":", 1),
        envelope.replacen("\"noul\":", "\"noul\":0,\"noul\":", 1),
        envelope.replacen("\"usage\":", "\"usage\":{},\"usage\":", 1),
        envelope.replacen("\"input_tokens\":", "\"input_tokens\":99,\"input_tokens\":", 1),
        envelope.replacen("\"output_tokens\":", "\"output_tokens\":99,\"output_tokens\":", 1),
        envelope.replacen("\"result\":", "\"result\":{},\"result\":", 1),
        format!(r#"{{"success":true,"metadata":{{"x":1,"x":2}},"result":{inner}}}"#),
    ];
    for body in variants {
        assert_ne!(body, envelope, "fixture mutation must introduce a duplicate");
        assert_eq!(
            decode_response(&request(), body.as_bytes()).err(),
            Some(CodecError::InvalidJson)
        );
    }
}

#[test]
fn malformed_brace_boundaries_and_non_json_never_panic() {
    for body in [
        "}{", "explanation } then {", "é } then {", "{", "}", "", "null", "[]",
        "```json\n{}\n```", "{} trailing", "{}{}",
    ] {
        assert!(decode_response(&request(), body.as_bytes()).is_err());
    }
    assert_eq!(
        decode_response(&request(), &[0xff, b'{', b'}']).err(),
        Some(CodecError::InvalidJson)
    );
}

#[test]
fn missing_or_non_boolean_success_is_not_success() {
    for value in [Value::Null, json!(false), json!("true"), json!(1)] {
        let envelope = json!({"success": value, "result": result()});
        assert_eq!(
            decode_response(&request(), &serde_json::to_vec(&envelope).unwrap()).err(),
            Some(CodecError::InvalidAnswer)
        );
    }
    assert_eq!(
        decode_response(&request(), &serde_json::to_vec(&result()).unwrap()).err(),
        Some(CodecError::InvalidAnswer)
    );
}

#[test]
fn absent_null_and_incomplete_usage_are_not_zero() {
    let mut absent = result();
    absent.as_object_mut().unwrap().remove("usage");
    assert_eq!(
        decode_response(&request(), &wrap(&absent)).err(),
        Some(CodecError::InvalidAnswer)
    );
    for usage in [
        Value::Null,
        json!({}),
        json!({"input_tokens": 12}),
        json!({"output_tokens": 7}),
    ] {
        let mut inner = result();
        inner["usage"] = usage;
        assert_eq!(
            decode_response(&request(), &wrap(&inner)).err(),
            Some(CodecError::InvalidAnswer)
        );
    }
}

#[test]
fn usage_must_contain_unsigned_integer_counts() {
    for key in ["input_tokens", "output_tokens"] {
        for value in [Value::Null, json!(-1), json!(0.5), json!("7"), json!(true)] {
            let mut inner = result();
            inner["usage"][key] = value;
            assert_eq!(
                decode_response(&request(), &wrap(&inner)).err(),
                Some(CodecError::InvalidAnswer)
            );
        }
    }
    let oversized_count = String::from_utf8(wrap(&result()))
        .unwrap()
        .replace("\"input_tokens\":12", "\"input_tokens\":18446744073709551616");
    assert_eq!(
        decode_response(&request(), oversized_count.as_bytes()).err(),
        Some(CodecError::InvalidAnswer)
    );
}

#[test]
fn explicit_zero_and_maximum_usage_counts_are_preserved() {
    for count in [0, u64::MAX] {
        let mut inner = result();
        inner["usage"] = json!({"input_tokens": count, "output_tokens": count});
        let response = decode_response(&request(), &wrap(&inner)).unwrap();
        assert_eq!(response.usage.input_tokens, count);
        assert_eq!(response.usage.output_tokens, count);
        assert_eq!(response.usage.total_tokens(), count.saturating_add(count));
    }
}

#[test]
fn missing_and_malformed_model_identity_never_fall_back_to_the_request_alias() {
    let mut absent = result();
    absent.as_object_mut().unwrap().remove("model");
    assert_eq!(
        decode_response(&request(), &wrap(&absent)).err(),
        Some(CodecError::InvalidAnswer)
    );
    for model in [Value::Null, json!(""), json!(12), json!("jev\ncanary")] {
        let mut inner = result();
        inner["model"] = model;
        assert_eq!(
            decode_response(&request(), &wrap(&inner)).err(),
            Some(CodecError::InvalidAnswer)
        );
    }
}

#[test]
fn competing_or_malformed_result_locations_are_rejected() {
    let mut top = result();
    top["success"] = json!(true);
    top["result"] = result();
    let mut outer = result();
    outer["result"] = result();
    for envelope in [
        top,
        json!({"success": true, "result": outer}),
        json!({"success": true, "result": null}),
        json!({"success": true, "result": []}),
        json!({"success": true, "result": {"result": null}}),
        json!({"success": true, "result": {"result": []}}),
    ] {
        assert_eq!(
            decode_response(&request(), &serde_json::to_vec(&envelope).unwrap()).err(),
            Some(CodecError::InvalidAnswer)
        );
    }
}

#[test]
fn whole_envelope_including_ignored_metadata_is_byte_bounded() {
    let body = wrap(&result());
    let mut at_limit = body.clone();
    at_limit.resize(MAX_RESPONSE_BYTES, b' ');
    assert!(decode_response(&request(), &at_limit).is_ok());
    at_limit.push(b' ');
    assert_eq!(
        decode_response(&request(), &at_limit).err(),
        Some(CodecError::TooLarge)
    );
    let envelope = json!({
        "success": true,
        "result": result(),
        "metadata": "x".repeat(MAX_RESPONSE_BYTES)
    });
    assert_eq!(
        decode_response(&request(), &serde_json::to_vec(&envelope).unwrap()).err(),
        Some(CodecError::TooLarge)
    );
}

#[test]
fn ignored_metadata_cannot_bypass_the_nesting_limit() {
    let mut metadata = json!(0);
    for _ in 0..70 {
        metadata = json!([metadata]);
    }
    let envelope = json!({"success": true, "result": result(), "metadata": metadata});
    assert_eq!(
        decode_response(&request(), &serde_json::to_vec(&envelope).unwrap()).err(),
        Some(CodecError::InvalidJson)
    );
}

#[test]
fn envelope_validation_preserves_common_answer_rejections() {
    let mut foreign = result();
    foreign["answers"] = json!({"foreign": {"type": "noul", "noul": 0.75}});
    let mut missing = result();
    missing["answers"] = json!({});
    let mut invalid_probability = result();
    invalid_probability["answers"]["fit"]["noul"] = json!(1.1);
    let mut wrong_type = result();
    wrong_type["answers"]["fit"] = json!({
        "type": "choice", "choice": "a", "probabilities": {"a": 1.0}, "confidence": 1.0
    });
    for inner in [foreign, missing, invalid_probability, wrong_type] {
        let req = request();
        let expected = req.decode_response(&serde_json::to_vec(&inner).unwrap()).err();
        assert!(expected.is_some());
        assert_eq!(decode_response(&req, &wrap(&inner)).err(), expected);
    }
}

#[test]
fn choice_probabilities_are_validated_without_losing_duplicate_options() {
    let req = Request::new(
        "typesafe/jev".into(),
        json!("synthetic choice"),
        [(
            "pick".into(),
            Question::choice(
                json!("Pick an option"),
                [("a".into(), "A".into()), ("b".into(), "B".into())],
            )
            .unwrap(),
        )],
    )
    .unwrap();
    let mut inner = json!({
        "model": "jev-1.13.0",
        "answers": {"pick": {
            "type": "choice", "choice": "a",
            "probabilities": {"a": 0.75, "b": 0.25}, "confidence": 0.8
        }},
        "usage": {"input_tokens": 12, "output_tokens": 7}
    });
    let response = decode_response(&req, &wrap(&inner)).unwrap();
    assert!(matches!(&response.answers["pick"], Answer::Choice(v) if v.choice() == "a"));
    let duplicate = String::from_utf8(wrap(&inner))
        .unwrap()
        .replace("\"a\":0.75", "\"a\":0.01,\"a\":0.75");
    assert_eq!(
        decode_response(&req, duplicate.as_bytes()).err(),
        Some(CodecError::InvalidJson)
    );
    inner["answers"]["pick"]["probabilities"] = json!({"a": 0.75, "foreign": 0.25});
    assert_eq!(
        decode_response(&req, &wrap(&inner)).err(),
        Some(CodecError::OptionMismatch)
    );
}

#[test]
fn rejection_diagnostics_do_not_include_provider_content() {
    let mut inner = result();
    inner["model"] = json!("private-provider-canary\n");
    let error = decode_response(&request(), &wrap(&inner)).err().unwrap();
    assert!(!format!("{error} {error:?}").contains("private-provider-canary"));
}
