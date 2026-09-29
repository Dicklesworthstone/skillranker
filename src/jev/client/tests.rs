//! Offline native transport regressions. No test contacts a public provider.

use super::*;
use crate::jev::CanonicalOrigin;
use crate::jev::cloudflare_codec::{CLOUDFLARE_JEV_MODEL, encode_request};
use crate::jev::codec::{Answer, MAX_REQUEST_BYTES, Question};
use crate::limits::DurationMillis;
use crate::privacy::{ApiCredential, ConsentSource, NetworkBlock};
use crate::runtime::ProcessInvocation;
use serde_json::{Value, json};
use std::net::TcpListener;

const ACCOUNT: &str = "0123456789abcdef0123456789abcdef";
const TOKEN: &str = "synthetic-cloudflare-transport-token";
const CONSENT: NetworkConsent = NetworkConsent::Authorized(ConsentSource::AllowNetworkFlag);

fn request_with_state(model: &str, state: &str) -> Request {
    Request::new(
        model.into(),
        json!(state),
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

fn request() -> Request {
    request_with_state(CLOUDFLARE_JEV_MODEL, "synthetic context")
}

fn native_response() -> Vec<u8> {
    serde_json::to_vec(&json!({
        "success": true,
        "result": {"result": {
            "model": "jev-synthetic-revision",
            "answers": {"fit": {"type": "noul", "noul": 0.75}},
            "usage": {"input_tokens": 12, "output_tokens": 7}
        }}
    }))
    .unwrap()
}

fn clock(total: u64, reserve: u64) -> EntryClock {
    EntryClock::capture_with(
        DurationMillis::new("test-total", total, 120_000).unwrap(),
        DurationMillis::new("test-reserve", reserve, 120_000).unwrap(),
    )
    .unwrap()
}

fn invocation() -> ProcessInvocation {
    ProcessInvocation::from_clock(clock(60_000, 1_000)).unwrap()
}

fn credential(origin: &CanonicalOrigin) -> OriginScopedCredential {
    let token = ApiCredential::from_environment(TOKEN.into())
        .unwrap()
        .unwrap();
    OriginScopedCredential::bind(token, origin).unwrap()
}

// Keep a bound loopback listener so a preflight regression can neither contact
// a public origin nor accidentally find some other service on a guessed port.
fn local_client() -> (TcpListener, JevClient) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = EndpointConfig::from_base_origin_str(&format!(
        "https://127.0.0.1:{}",
        listener.local_addr().unwrap().port(),
    ))
    .unwrap();
    let client = JevClient::cloudflare_at(endpoint, ACCOUNT, Vec::new()).unwrap();
    (listener, client)
}

fn assert_no_connection(listener: &TcpListener) {
    assert!(matches!(
        listener.accept(),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
    ));
}

#[test]
fn native_target_is_fixed_scoped_and_canonical() {
    let client = JevClient::cloudflare(&ACCOUNT.to_ascii_uppercase()).unwrap();
    assert_eq!(client.origin().as_str(), CLOUDFLARE_ORIGIN);
    assert_eq!(
        client.target_url(),
        format!("{CLOUDFLARE_ORIGIN}/client/v4/accounts/{ACCOUNT}/ai/run"),
    );
    assert!(!format!("{client:?}").contains(ACCOUNT));
    let ordinary = JevClient::new(EndpointConfig::production()).unwrap();
    assert_eq!(
        ordinary.target_url(),
        "https://api.typesafe.ai/v1/systemone"
    );
}

#[test]
fn invalid_accounts_cannot_change_the_native_host_or_path() {
    for account in [
        "",
        "0123456789abcdef",
        "0123456789abcdef0123456789abcdef0",
        "0123456789abcdef0123456789abcdeg",
        "../0123456789abcdef0123456789abcde",
        "0123456789abcdef0123456789abcdef?",
        "0123456789abcdef0123456789abcdef#",
        "https://attacker.invalid/account",
        "１２３４５６７８９０abcdef",
    ] {
        let error = JevClient::cloudflare(account).err().unwrap();
        assert_eq!(error.kind, TransportErrorKind::InvalidConfiguration);
        assert!(!error.http_attempt_started);
    }
}

#[test]
fn supplied_native_token_is_validated_and_bound_without_environment_reads() {
    let (client, key) =
        JevClient::cloudflare_with_environment_token(ACCOUNT, TOKEN.into()).unwrap();
    assert!(key.authorization_header_for(client.origin()).is_ok());
    assert!(
        key.authorization_header_for(&CanonicalOrigin::production())
            .is_err()
    );
    assert!(!format!("{client:?} {key:?}").contains(TOKEN));
    for token in ["", "bad\nheader", "bad token"] {
        let error = JevClient::cloudflare_with_environment_token(ACCOUNT, token.into())
            .err()
            .unwrap();
        assert!(!error.http_attempt_started);
    }
}

#[test]
fn native_request_preserves_typed_questions_and_escaped_context() {
    let request = request_with_state(CLOUDFLARE_JEV_MODEL, "é界\n\"quoted\"\\state");
    let ordinary: Value = serde_json::from_slice(&request.to_json().unwrap()).unwrap();
    let native: Value = serde_json::from_slice(&encode_request(&request).unwrap()).unwrap();
    assert_eq!(native["model"], ordinary["model"]);
    assert_eq!(native["input"]["state"], ordinary["state"]);
    assert_eq!(native["input"]["questions"], ordinary["questions"]);
    assert_eq!(native.as_object().unwrap().len(), 2);
    assert_eq!(native["input"].as_object().unwrap().len(), 2);
}

#[test]
fn native_wrapper_counts_toward_the_exact_request_limit() {
    let empty = request_with_state(CLOUDFLARE_JEV_MODEL, "");
    let overhead = encode_request(&empty).unwrap().len();
    let full = request_with_state(
        CLOUDFLARE_JEV_MODEL,
        &"x".repeat(MAX_REQUEST_BYTES - overhead),
    );
    assert_eq!(encode_request(&full).unwrap().len(), MAX_REQUEST_BYTES);
    let oversized = request_with_state(
        CLOUDFLARE_JEV_MODEL,
        &"x".repeat(MAX_REQUEST_BYTES - overhead + 1),
    );
    assert!(oversized.to_json().unwrap().len() <= MAX_REQUEST_BYTES);
    assert!(matches!(
        encode_request(&oversized),
        Err(CodecError::TooLarge)
    ));
}

#[test]
fn native_encoding_does_not_substitute_or_accept_another_model_route() {
    for model in ["jev-latest", "@cf/other/model", "typesafe/jev?x=1"] {
        assert!(matches!(
            encode_request(&request_with_state(model, "synthetic context")),
            Err(CodecError::InvalidRequest)
        ));
    }
}

#[test]
fn invalid_native_request_fails_before_debit_or_socket() {
    let (listener, client) = local_client();
    let invocation = invocation();
    let cx = invocation.request_cx().unwrap();
    let key = credential(client.origin());
    let empty = request_with_state(CLOUDFLARE_JEV_MODEL, "");
    let oversized = request_with_state(
        CLOUDFLARE_JEV_MODEL,
        &"x".repeat(MAX_REQUEST_BYTES - encode_request(&empty).unwrap().len() + 1),
    );
    for request in [
        request_with_state("jev-latest", "synthetic context"),
        oversized,
    ] {
        let mut starts = 0;
        let mut start = || {
            starts += 1;
            Ok(())
        };
        let error = invocation
            .runtime()
            .block_on(JevTransport::send_accounted(
                &client,
                &request,
                Some(&key),
                CONSENT,
                &cx,
                &invocation.clock(),
                &mut start,
            ))
            .err()
            .unwrap();
        assert!(matches!(error.kind, TransportErrorKind::Request(_)));
        assert!(!error.http_attempt_started);
        assert_eq!(starts, 0);
        assert_no_connection(&listener);
    }
    assert!(invocation.shutdown());
}

#[test]
fn native_admission_and_origin_refusals_do_not_debit_or_connect() {
    let (listener, client) = local_client();
    let invocation = invocation();
    let cx = invocation.request_cx().unwrap();
    let valid = credential(client.origin());
    let wrong = credential(&CanonicalOrigin::production());
    for (key, consent) in [
        (Some(&valid), NetworkConsent::NotAuthorized),
        (Some(&valid), NetworkConsent::Blocked(NetworkBlock::Offline)),
        (Some(&valid), NetworkConsent::Blocked(NetworkBlock::DryRun)),
        (None, CONSENT),
        (Some(&wrong), CONSENT),
    ] {
        let mut starts = 0;
        let mut start = || {
            starts += 1;
            Ok(())
        };
        let error = invocation
            .runtime()
            .block_on(JevTransport::send_accounted(
                &client,
                &request(),
                key,
                consent,
                &cx,
                &invocation.clock(),
                &mut start,
            ))
            .err()
            .unwrap();
        assert!(!error.http_attempt_started);
        assert_eq!(starts, 0);
        assert_no_connection(&listener);
    }
    assert!(invocation.shutdown());
}

#[test]
fn native_accounting_refusal_prevents_polling_the_http_future() {
    let (listener, client) = local_client();
    let invocation = invocation();
    let cx = invocation.request_cx().unwrap();
    let key = credential(client.origin());
    let mut starts = 0;
    let mut start = || {
        starts += 1;
        Err(failure(TransportErrorKind::Deadline, false))
    };
    let error = invocation
        .runtime()
        .block_on(JevTransport::send_accounted(
            &client,
            &request(),
            Some(&key),
            CONSENT,
            &cx,
            &invocation.clock(),
            &mut start,
        ))
        .err()
        .unwrap();
    assert_eq!(error.kind, TransportErrorKind::Deadline);
    assert!(!error.http_attempt_started);
    assert_eq!(starts, 1);
    assert_no_connection(&listener);
    assert!(invocation.shutdown());
}

#[test]
fn completion_keeps_provider_model_answers_and_usage() {
    let invocation = invocation();
    let cx = invocation.request_cx().unwrap();
    let (_, client) = local_client();
    let result = finish_response(&cx, &invocation.clock(), || {
        client.protocol.decode(&request(), &native_response())
    })
    .unwrap();
    assert_eq!(result.requested_model, CLOUDFLARE_JEV_MODEL);
    assert_eq!(result.returned_model, "jev-synthetic-revision");
    assert_eq!(result.usage.input_tokens, 12);
    assert_eq!(result.usage.output_tokens, 7);
    assert!(matches!(result.answers["fit"], Answer::Noul(0.75)));
    assert!(invocation.shutdown());
}

#[test]
fn cancellation_during_decode_withholds_an_otherwise_valid_response() {
    let invocation = invocation();
    let cx = invocation.request_cx().unwrap();
    let (_, client) = local_client();
    let error = finish_response(&cx, &invocation.clock(), || {
        let decoded = client.protocol.decode(&request(), &native_response());
        cx.set_cancel_requested(true);
        decoded
    })
    .err()
    .unwrap();
    assert_eq!(error.kind, TransportErrorKind::Cancelled);
    assert!(error.http_attempt_started);
    assert!(invocation.shutdown());
}

#[test]
fn expiry_at_decode_completion_is_not_success() {
    let invocation = invocation();
    let cx = invocation.request_cx().unwrap();
    let (_, client) = local_client();
    let clock = clock(20, 10);
    let error = finish_response(&cx, &clock, || {
        let decoded = client.protocol.decode(&request(), &native_response());
        while clock.admit_new_work().is_ok() {
            std::thread::sleep(Duration::from_millis(1));
        }
        decoded
    })
    .err()
    .unwrap();
    assert_eq!(error.kind, TransportErrorKind::Deadline);
    assert!(error.http_attempt_started);
    assert!(invocation.shutdown());
}

#[test]
fn malformed_native_completion_remains_a_started_unknown_usage_failure() {
    let invocation = invocation();
    let cx = invocation.request_cx().unwrap();
    let (_, client) = local_client();
    for body in [
        b"} malformed {".as_slice(),
        br#"{"success":true,"result":{"model":"jev-synthetic","answers":{"fit":{"type":"noul","noul":0.5}}}}"#,
    ] {
        let error = finish_response(&cx, &invocation.clock(), || {
            client.protocol.decode(&request(), body)
        })
        .err()
        .unwrap();
        assert!(matches!(error.kind, TransportErrorKind::Response(_)));
        assert!(error.http_attempt_started);
        assert!(!format!("{error} {error:?}").contains("malformed"));
    }
    assert!(invocation.shutdown());
}

#[test]
fn native_wrapper_also_counts_toward_the_nesting_limit() {
    for (layers, accepted) in [
        (crate::output::MAX_OUTPUT_DEPTH - 2, true),
        (crate::output::MAX_OUTPUT_DEPTH - 1, false),
    ] {
        let mut state = Value::Null;
        for _ in 0..layers {
            state = json!([state]);
        }
        let request = Request::new(
            CLOUDFLARE_JEV_MODEL.into(),
            state,
            request()
                .questions()
                .iter()
                .map(|(id, q)| (id.clone(), q.clone())),
        )
        .unwrap();
        assert!(request.to_json().is_ok());
        let encoded = encode_request(&request);
        if accepted {
            assert!(encoded.is_ok());
        } else {
            assert!(matches!(encoded, Err(CodecError::InvalidJson)));
        }
    }
}

mod tls;
