//! The actual native client and RetrySession against an owned local TLS peer.

use super::*;
use crate::jev::AttemptBudget;
use crate::jev::retry::{RetryErrorKind, RetrySession};
use std::io::{BufRead, BufReader};
use std::process::{Child, ChildStdout, Command, Stdio};

struct Server {
    child: Child,
    lines: Option<BufReader<ChildStdout>>,
    port: u16,
}

fn read_json(lines: &mut impl BufRead) -> Value {
    let mut line = String::new();
    assert!(lines.read_line(&mut line).unwrap() > 0, "fixture exited early");
    serde_json::from_str(&line).unwrap()
}

impl Server {
    fn new(steps: &[&str], native: bool) -> Self {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/jev-tls/cloudflare_server.py");
        let mut child = Command::new("/usr/bin/python3")
            .arg(path)
            .arg(serde_json::to_string(steps).unwrap())
            .arg(if native { "native" } else { "typesafe" })
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let lines = BufReader::new(child.stdout.take().unwrap());
        let mut server = Self {
            child,
            lines: Some(lines),
            port: 0,
        };
        let hello = read_json(server.lines.as_mut().unwrap());
        server.port = u16::try_from(hello["port"].as_u64().unwrap()).unwrap();
        server
    }

    fn client(&self, native: bool, host: &str, trusted: bool) -> JevClient {
        let endpoint =
            EndpointConfig::from_base_origin_str(&format!("https://{host}:{}", self.port))
                .unwrap();
        let roots = if trusted {
            Certificate::from_pem(include_bytes!("../../../../tests/fixtures/jev-tls/ca.pem"))
                .unwrap()
        } else {
            Vec::new()
        };
        if native {
            JevClient::cloudflare_at(endpoint, ACCOUNT, roots).unwrap()
        } else {
            JevClient::with_additional_roots(endpoint, roots).unwrap()
        }
    }

    fn finish(mut self, requests: u64) -> Value {
        let report = read_json(self.lines.as_mut().unwrap());
        assert!(self.child.wait().unwrap().success());
        assert_eq!(report["requests"], requests);
        assert_eq!(report["valid_requests"], requests);
        assert_eq!(report["extra_connections"], 0);
        assert!(
            report["closed"]
                .as_array()
                .unwrap()
                .iter()
                .all(|closed| closed == true)
        );
        report
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn native_https_sends_the_account_path_and_native_envelope() {
    for shape in ["ok", "flat", "top"] {
        let server = Server::new(&[shape], true);
        let client = server.client(true, "localhost", true);
        let key = credential(client.origin());
        let invocation = invocation();
        let cx = invocation.request_cx().unwrap();
        let mut starts = 0;
        let mut start = || {
            starts += 1;
            Ok(())
        };
        let response = invocation
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
            .unwrap();
        assert_eq!(starts, 1);
        assert_eq!(response.returned_model, "jev-synthetic-revision");
        assert_eq!(response.requested_model, CLOUDFLARE_JEV_MODEL);
        assert_eq!(response.usage.total_tokens(), 19);
        assert!(matches!(response.answers["fit"], Answer::Noul(0.75)));
        let report = server.finish(1);
        assert_eq!(report["closed"], json!([true]));
        assert!(invocation.shutdown());
    }
}

#[test]
fn both_protocols_reuse_wide_tls_and_close_after_rerank() {
    for native in [false, true] {
        let server = Server::new(&["reuse", "ok"], native);
        let client = server.client(native, "localhost", true);
        let key = credential(client.origin());
        let invocation = invocation();
        let cx = invocation.request_cx().unwrap();
        let model = if native {
            CLOUDFLARE_JEV_MODEL
        } else {
            "jev-latest"
        };
        let request = request_with_state(model, "synthetic context");
        let mut session = RetrySession::new(
            &client,
            Some(&key),
            invocation.clock(),
            AttemptBudget::default(),
            "native-reuse-test",
        )
        .unwrap();
        assert_eq!(session.origin(), client.origin());
        for stage in [RankingStage::Wide, RankingStage::Rerank] {
            let answer = invocation
                .runtime()
                .block_on(session.send_stage(stage, &request, &cx, || Ok(CONSENT)))
                .unwrap();
            assert_eq!(answer.attempts, 1);
        }
        assert_eq!(session.receipt().sent_attempts, 2);
        assert_eq!(session.receipt().completed_attempts, 2);
        assert_eq!(session.receipt().unknown_usage_attempts, 0);
        assert_eq!(session.receipt().total_tokens(), 38);
        let report = server.finish(2);
        assert_eq!(report["connections"], 1);
        assert_ne!(report["connection_headers"][0], "close");
        assert_eq!(report["connection_headers"][1], "close");
        assert_eq!(report["closed"], json!([true]));
        assert!(invocation.shutdown());
    }
}

#[test]
fn malformed_native_https_is_not_retried_or_recorded_as_zero_usage_success() {
    for step in [
        "missing-usage",
        "partial-usage",
        "missing-model",
        "duplicate",
        "malformed",
        "unsuccessful",
    ] {
        let server = Server::new(&[step], true);
        let client = server.client(true, "localhost", true);
        let key = credential(client.origin());
        let invocation = invocation();
        let cx = invocation.request_cx().unwrap();
        let mut session = RetrySession::new(
            &client,
            Some(&key),
            invocation.clock(),
            AttemptBudget::default(),
            "native-invalid-response-test",
        )
        .unwrap();
        let error = invocation
            .runtime()
            .block_on(session.send_stage(RankingStage::Wide, &request(), &cx, || Ok(CONSENT)))
            .err()
            .unwrap();
        assert_eq!(error.receipt.sent_attempts, 1, "{step}");
        assert_eq!(error.receipt.completed_attempts, 0, "{step}");
        assert_eq!(error.receipt.unknown_usage_attempts, 1, "{step}");
        assert_eq!(error.receipt.total_tokens(), 0, "{step}");
        assert!(matches!(
            error.last_transport.unwrap().kind,
            TransportErrorKind::Response(_)
        ));
        server.finish(1);
        assert!(invocation.shutdown());
    }
}

#[test]
fn native_transient_retry_is_reauthorized_and_each_attempt_is_accounted() {
    let server = Server::new(&["429:0", "ok"], true);
    let client = server.client(true, "localhost", true);
    let key = credential(client.origin());
    let invocation = invocation();
    let cx = invocation.request_cx().unwrap();
    let mut session = RetrySession::new(
        &client,
        Some(&key),
        invocation.clock(),
        AttemptBudget::default(),
        "native-retry-test",
    )
    .unwrap();
    let mut authorized = 0;
    let answer = invocation
        .runtime()
        .block_on(session.send_stage(RankingStage::Wide, &request(), &cx, || {
            authorized += 1;
            Ok(CONSENT)
        }))
        .unwrap();
    assert_eq!(authorized, 2);
    assert_eq!(answer.attempts, 2);
    assert_eq!(answer.receipt.sent_attempts, 2);
    assert_eq!(answer.receipt.completed_attempts, 1);
    assert_eq!(answer.receipt.unknown_usage_attempts, 1);
    assert_eq!(answer.receipt.total_tokens(), 19);
    server.finish(2);
    assert!(invocation.shutdown());
}

#[test]
fn revoked_native_retry_authorization_prevents_another_send() {
    let server = Server::new(&["429:0"], true);
    let client = server.client(true, "localhost", true);
    let key = credential(client.origin());
    let invocation = invocation();
    let cx = invocation.request_cx().unwrap();
    let mut session = RetrySession::new(
        &client,
        Some(&key),
        invocation.clock(),
        AttemptBudget::default(),
        "native-revoked-retry-test",
    )
    .unwrap();
    let mut authorized = 0;
    let error = invocation
        .runtime()
        .block_on(session.send_stage(RankingStage::Wide, &request(), &cx, || {
            authorized += 1;
            if authorized == 1 {
                Ok(CONSENT)
            } else {
                Err(())
            }
        }))
        .err()
        .unwrap();
    assert_eq!(authorized, 2);
    assert!(matches!(error.kind, RetryErrorKind::PolicyChanged));
    assert_eq!(error.receipt.sent_attempts, 1);
    assert_eq!(error.receipt.unknown_usage_attempts, 1);
    server.finish(1);
    assert!(invocation.shutdown());
}

#[test]
fn native_http_enforces_response_headers_and_body_limits() {
    for (step, expected) in [
        ("wrong-type", TransportErrorKind::InvalidContentType),
        ("missing-type", TransportErrorKind::InvalidContentType),
        ("duplicate-type", TransportErrorKind::InvalidContentType),
        ("encoding", TransportErrorKind::UnsupportedEncoding),
        ("duplicate-encoding", TransportErrorKind::UnsupportedEncoding),
        ("oversized", TransportErrorKind::BodyTooLarge),
    ] {
        let server = Server::new(&[step], true);
        let client = server.client(true, "localhost", true);
        let key = credential(client.origin());
        let invocation = invocation();
        let cx = invocation.request_cx().unwrap();
        let error = invocation
            .runtime()
            .block_on(client.send(&request(), Some(&key), CONSENT, &cx, &invocation.clock()))
            .err()
            .unwrap();
        assert_eq!(error.kind, expected, "{step}");
        assert!(error.http_attempt_started);
        server.finish(1);
        assert!(invocation.shutdown());
    }
}

#[test]
fn native_http_rejects_redirects_and_retains_retry_after() {
    for (status, expected) in [
        (302, TransportErrorKind::Redirect),
        (401, TransportErrorKind::HttpStatus(401)),
        (503, TransportErrorKind::HttpStatus(503)),
    ] {
        let step = format!("status:{status}");
        let server = Server::new(&[&step], true);
        let client = server.client(true, "localhost", true);
        let key = credential(client.origin());
        let invocation = invocation();
        let cx = invocation.request_cx().unwrap();
        let error = invocation
            .runtime()
            .block_on(client.send(&request(), Some(&key), CONSENT, &cx, &invocation.clock()))
            .err()
            .unwrap();
        assert_eq!(error.kind, expected);
        assert!(error.http_attempt_started);
        if status != 302 {
            assert_eq!(error.retry_after, RetryAfter::Delay(Duration::from_secs(3)));
        }
        server.finish(1);
        assert!(invocation.shutdown());
    }
}

#[test]
fn native_tls_rejects_untrusted_roots_and_wrong_hostnames_before_http() {
    for (host, trusted) in [("localhost", false), ("127.0.0.1", true)] {
        let server = Server::new(&["ok"], true);
        let client = server.client(true, host, trusted);
        let key = credential(client.origin());
        let invocation = invocation();
        let cx = invocation.request_cx().unwrap();
        let error = invocation
            .runtime()
            .block_on(client.send(&request(), Some(&key), CONSENT, &cx, &invocation.clock()))
            .err()
            .unwrap();
        assert_eq!(error.kind, TransportErrorKind::Tls);
        assert!(error.http_attempt_started);
        server.finish(0);
        assert!(invocation.shutdown());
    }
}

#[test]
fn native_stalled_body_cancellation_closes_the_socket() {
    let mut server = Server::new(&["stall"], true);
    let client = server.client(true, "localhost", true);
    let key = credential(client.origin());
    let invocation = invocation();
    let cx = invocation.request_cx().unwrap();
    let canceller = cx.clone();
    let mut lines = server.lines.take().unwrap();
    let cancellation = std::thread::spawn(move || {
        let event = read_json(&mut lines);
        // Synchronize on an actual started body, not a guessed sleep duration.
        canceller.set_cancel_requested(true);
        (lines, event)
    });
    let error = invocation
        .runtime()
        .block_on(client.send(&request(), Some(&key), CONSENT, &cx, &invocation.clock()))
        .err()
        .unwrap();
    let (lines, event) = cancellation.join().unwrap();
    server.lines = Some(lines);
    assert_eq!(event["body_started"], true);
    assert_eq!(error.kind, TransportErrorKind::Cancelled);
    assert!(error.http_attempt_started);
    assert_eq!(server.finish(1)["closed"], json!([true]));
    assert!(invocation.shutdown());
}

#[test]
fn native_stalled_body_deadline_closes_the_socket() {
    let mut server = Server::new(&["stall"], true);
    let client = server.client(true, "localhost", true);
    let key = credential(client.origin());
    let invocation = invocation();
    let cx = invocation.request_cx().unwrap();
    // A separate work clock leaves the runtime enough time to drain after
    // this intentionally expired exchange. No public provider is involved.
    let work = clock(1_000, 100);
    let error = invocation
        .runtime()
        .block_on(client.send(&request(), Some(&key), CONSENT, &cx, &work))
        .err()
        .unwrap();
    let event = read_json(server.lines.as_mut().unwrap());
    assert_eq!(event["body_started"], true);
    assert_eq!(error.kind, TransportErrorKind::Deadline);
    assert!(error.http_attempt_started);
    assert_eq!(server.finish(1)["closed"], json!([true]));
    assert!(invocation.shutdown());
}
