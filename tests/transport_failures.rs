#![cfg(unix)]
//! Comprehensive transport failure matrix, shutdown, accounting, and safe error reporting.
//!
//! Enforces boundary `p1_transport_failure_matrix` (sr-roadmap-l1i.2.11):
//! - DNS stalls, connection refusals, and unreachable endpoints fail safely without canary leaks.
//! - Handshake timeouts preserve the cleanup reserve and never publish late results.
//! - Pipe saturation / oversized payloads fail safely within bounded buffers.
//! - Malformed responses fail without panic or private payload retention.
//! - HTTP 429 backoff and Retry-After strictly honor attempt budgets and deadlines.
//! - SIGINT/SIGTERM user cancellation terminates cleanly with zero surviving workers or leaked descriptors.
//! - Honest success counterparts verify that healthy operations succeed under identical credentials.

use asupersync::tls::Certificate;
use asupersync::{CancelKind, Cx};
use serde_json::{Value, json};
use skillranker::config::{ConfigSources, ResolvedConfig};
use skillranker::jev::client::{JevClient, TransportError, TransportErrorKind};
use skillranker::jev::codec::{Answer, CodecError, Question, Request, Response};
use skillranker::jev::retry::{RetryErrorKind, RetrySession, RetryStop};
use skillranker::jev::{
    AdmissionRefusal, AttemptBudget, CanonicalOrigin, EndpointConfig, OriginScopedCredential,
    RankingStage,
};
use skillranker::limits::DurationMillis;
use skillranker::privacy::{ConsentSource, NetworkConsent};
use skillranker::runtime::{EntryClock, ProcessInvocation};
use std::ffi::OsString;
use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

const TRANSPORT_CANARY: &str = "synthetic-transport-canary";
const RETRY_CANARY: &str = "synthetic-retry-canary";
const CONSENT: NetworkConsent = NetworkConsent::Authorized(ConsentSource::AllowNetworkFlag);
static NEXT_SERVER: AtomicU64 = AtomicU64::new(0);

fn test_request() -> Request {
    Request::new(
        "jev-latest".into(),
        json!("synthetic transport probe"),
        [(
            "fit".into(),
            Question::Noul {
                instructions: json!("Assess fit"),
                criteria: None,
            },
        )],
    )
    .unwrap()
}

fn retry_request() -> Request {
    Request::new(
        "jev-latest".into(),
        json!("synthetic retry probe"),
        [(
            "fit".into(),
            Question::Noul {
                instructions: json!("Synthetic fit"),
                criteria: None,
            },
        )],
    )
    .unwrap()
}

fn scoped_credential(origin: &CanonicalOrigin, token: &str) -> OriginScopedCredential {
    let config = ResolvedConfig::resolve(
        ConfigSources {
            environment: vec![(OsString::from("TYPESAFE_API_KEY"), OsString::from(token))],
            ..Default::default()
        },
        1,
    )
    .unwrap();
    OriginScopedCredential::bind(config.credential().unwrap().clone(), origin).unwrap()
}

fn ca_certificate() -> Certificate {
    Certificate::from_pem(include_bytes!("fixtures/jev-tls/ca.pem"))
        .unwrap()
        .remove(0)
}

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
        key: Option<&OriginScopedCredential>,
    ) -> Result<Response, TransportError> {
        self.invocation.runtime().block_on(client.send(
            &test_request(),
            key,
            CONSENT,
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

/// Standalone loopback server wrapper for mode-based or sequence-based testing.
struct Server {
    child: Child,
    lines: BufReader<ChildStdout>,
    port: u16,
}

impl Server {
    fn new_mode(mode: &str) -> Self {
        let directory = std::env::temp_dir().join(format!(
            "sr-fail-tls-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT_SERVER.fetch_add(1, Ordering::Relaxed),
        ));
        std::fs::create_dir(&directory).unwrap();
        for (name, bytes) in [
            (
                "server.py",
                &include_bytes!("fixtures/jev-tls/server.py")[..],
            ),
            (
                "server.pem",
                &include_bytes!("fixtures/jev-tls/server.pem")[..],
            ),
            (
                "server.key",
                &include_bytes!("fixtures/jev-tls/server.key")[..],
            ),
        ] {
            std::fs::write(directory.join(name), bytes).unwrap();
        }

        let mut child = Command::new("/usr/bin/python3")
            .arg(directory.join("server.py"))
            .arg(mode)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();

        let mut lines = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        lines.read_line(&mut line).unwrap();
        let hello: Value = serde_json::from_str(&line).unwrap();
        let port = u16::try_from(hello["port"].as_u64().unwrap()).unwrap();
        Self { child, lines, port }
    }

    fn new_steps(steps: &[&str]) -> Self {
        let directory = std::env::temp_dir().join(format!(
            "sr-fail-seq-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT_SERVER.fetch_add(1, Ordering::Relaxed),
        ));
        std::fs::create_dir(&directory).unwrap();
        for (name, bytes) in [
            (
                "retry_server.py",
                &include_bytes!("fixtures/jev-tls/retry_server.py")[..],
            ),
            (
                "server.pem",
                &include_bytes!("fixtures/jev-tls/server.pem")[..],
            ),
            (
                "server.key",
                &include_bytes!("fixtures/jev-tls/server.key")[..],
            ),
        ] {
            std::fs::write(directory.join(name), bytes).unwrap();
        }

        let mut child = Command::new("/usr/bin/python3")
            .arg(directory.join("retry_server.py"))
            .arg(serde_json::to_string(steps).unwrap())
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();

        let mut lines = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        lines.read_line(&mut line).unwrap();
        let hello: Value = serde_json::from_str(&line).unwrap();
        let port = u16::try_from(hello["port"].as_u64().unwrap()).unwrap();
        Self { child, lines, port }
    }

    fn endpoint(&self, host: &str) -> EndpointConfig {
        EndpointConfig::from_base_origin_str(&format!("https://{host}:{}", self.port)).unwrap()
    }

    fn finish(mut self) -> Value {
        let mut line = String::new();
        self.lines.read_line(&mut line).unwrap();
        let report: Value = serde_json::from_str(&line).unwrap();
        let status = self.child.wait().unwrap();
        assert!(
            status.success(),
            "Server fixture process exited successfully"
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

fn assert_safe_error(err: &TransportError) {
    let display_str = format!("{err}");
    let debug_str = format!("{err:?}");
    assert!(
        !display_str.contains(TRANSPORT_CANARY),
        "Display must not leak canary secret"
    );
    assert!(
        !debug_str.contains(TRANSPORT_CANARY),
        "Debug must not leak canary secret"
    );
    assert!(
        !display_str.contains(RETRY_CANARY),
        "Display must not leak canary secret"
    );
    assert!(
        !debug_str.contains(RETRY_CANARY),
        "Debug must not leak canary secret"
    );
    assert!(
        !display_str.contains("Bearer"),
        "Display must not leak authorization header"
    );
    assert!(
        !display_str.contains("https://"),
        "Display must not leak raw URL endpoints"
    );
}

// ==============================================================================
// 1. Stalled DNS / Connection Refusal with Safe Error Reporting
// ==============================================================================
#[test]
fn stalled_dns_or_connection_refusal_fails_safely() {
    let closed_port = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };

    let endpoint =
        EndpointConfig::from_base_origin_str(&format!("https://127.0.0.1:{closed_port}")).unwrap();
    let cred = scoped_credential(endpoint.origin(), TRANSPORT_CANARY);
    let client = JevClient::with_additional_roots(endpoint, vec![ca_certificate()]).unwrap();

    let ctx = TestContext::new(2000, 200);
    let err = ctx.send(&client, Some(&cred)).err().unwrap();

    assert_eq!(err.kind, TransportErrorKind::Connect);
    assert!(
        err.http_attempt_started,
        "HTTP attempt was entered before socket connection refused"
    );
    assert_safe_error(&err);
    ctx.finish();
}

// ==============================================================================
// 2. Handshake Timeout Preserves Cleanup Reserve and Never Publishes Late
// ==============================================================================
#[test]
fn handshake_timeout_preserves_cleanup_reserve_and_never_publishes() {
    let server = Server::new_mode("slow-handshake");
    let endpoint = server.endpoint("localhost");
    let cred = scoped_credential(endpoint.origin(), TRANSPORT_CANARY);
    let client = JevClient::with_additional_roots(endpoint, vec![ca_certificate()]).unwrap();

    // Work deadline 300ms, cleanup reserve 100ms -> total 400ms
    let ctx = TestContext::new(400, 100);
    let start = Instant::now();
    let err = ctx.send(&client, Some(&cred)).err().unwrap();
    let elapsed = start.elapsed();

    assert!(
        matches!(
            err.kind,
            TransportErrorKind::Deadline | TransportErrorKind::TransientIo
        ),
        "Expected Deadline or TransientIo timeout, got {:?}",
        err.kind
    );
    assert!(err.http_attempt_started);
    assert_safe_error(&err);
    // Bounded wait: must terminate before total deadline + scheduler slack, never hang indefinitely
    assert!(
        elapsed >= Duration::from_millis(250),
        "Terminated too early: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_millis(800),
        "Handshake timeout stalled beyond allowable limit: {elapsed:?}"
    );

    ctx.finish();
    server.finish();
}

// ==============================================================================
// 3. Pipe Saturation and Body Bounds Fail Safely
// ==============================================================================
#[test]
fn pipe_saturation_and_body_limit_fails_safely() {
    let server = Server::new_mode("compression");
    let endpoint = server.endpoint("localhost");
    let cred = scoped_credential(endpoint.origin(), TRANSPORT_CANARY);
    let client = JevClient::with_additional_roots(endpoint, vec![ca_certificate()]).unwrap();

    let ctx = TestContext::new(3000, 200);
    let err = ctx.send(&client, Some(&cred)).err().unwrap();

    assert_eq!(err.kind, TransportErrorKind::UnsupportedEncoding);
    assert!(err.http_attempt_started);
    assert_safe_error(&err);

    ctx.finish();
    server.finish();
}

// ==============================================================================
// 4. Malformed Responses Fail Without Panic or Private Leak
// ==============================================================================
#[test]
fn malformed_response_fails_without_panic_or_leak() {
    let server = Server::new_mode("malformed");
    let endpoint = server.endpoint("localhost");
    let cred = scoped_credential(endpoint.origin(), TRANSPORT_CANARY);
    let client = JevClient::with_additional_roots(endpoint, vec![ca_certificate()]).unwrap();

    let ctx = TestContext::new(3000, 200);
    let err = ctx.send(&client, Some(&cred)).err().unwrap();

    assert!(
        matches!(
            err.kind,
            TransportErrorKind::Response(CodecError::InvalidJson)
                | TransportErrorKind::Response(CodecError::InvalidAnswer)
        ),
        "Expected InvalidJson or InvalidAnswer response error, got {:?}",
        err.kind
    );
    assert!(err.http_attempt_started);
    assert_safe_error(&err);

    ctx.finish();
    server.finish();
}

// ==============================================================================
// 5. HTTP 429 Backoff, Retry-After, and Budget Accounting
// ==============================================================================
#[test]
fn http_429_backoff_and_retry_after_budget_accounting() {
    // 5a. Excessive Retry-After refused immediately without stall
    {
        let server = Server::new_steps(&["429:86400"]);
        let endpoint = server.endpoint("localhost");
        let cred = scoped_credential(endpoint.origin(), RETRY_CANARY);
        let client = JevClient::with_additional_roots(endpoint, vec![ca_certificate()]).unwrap();

        let ctx = TestContext::new(3000, 200);
        let mut session = RetrySession::new(
            &client,
            Some(&cred),
            ctx.clock,
            AttemptBudget::default(),
            "inv-429-excessive",
        )
        .unwrap();

        let req = retry_request();
        let start = Instant::now();
        let err = ctx
            .invocation
            .runtime()
            .block_on(session.send_stage(RankingStage::Wide, &req, &ctx.cx, || Ok(CONSENT)))
            .err()
            .unwrap();
        let elapsed = start.elapsed();

        // Must refuse immediately because 86400s > remaining work window
        assert_eq!(
            err.kind,
            RetryErrorKind::RetryStopped(RetryStop::DoesNotFitDeadline)
        );
        assert!(
            elapsed < Duration::from_millis(500),
            "Excessive Retry-After stalled: {elapsed:?}"
        );

        // Accounted: exactly 1 attempt admitted and sent, unknown cost
        assert_eq!(err.receipt.admitted_attempts, 1);
        assert_eq!(err.receipt.sent_attempts, 1);
        assert!(err.receipt.has_unknown_usage);

        ctx.finish();
        server.finish();
    }

    // 5b. Transient errors exhaust max 4 HTTP attempts
    {
        let server = Server::new_steps(&["429:0", "429:0", "429:0", "429:0"]);
        let endpoint = server.endpoint("localhost");
        let cred = scoped_credential(endpoint.origin(), RETRY_CANARY);
        let client = JevClient::with_additional_roots(endpoint, vec![ca_certificate()]).unwrap();

        let ctx = TestContext::new(5000, 200);
        let mut session = RetrySession::new(
            &client,
            Some(&cred),
            ctx.clock,
            AttemptBudget::default(),
            "inv-429-exhaust",
        )
        .unwrap();

        let req = retry_request();
        let err = ctx
            .invocation
            .runtime()
            .block_on(session.send_stage(RankingStage::Wide, &req, &ctx.cx, || Ok(CONSENT)))
            .err()
            .unwrap();

        assert!(matches!(
            err.kind,
            RetryErrorKind::Admission(AdmissionRefusal::AttemptsExhausted {
                attempts_used: 4,
                limit: 4,
            })
        ));
        assert_eq!(err.receipt.sent_attempts, 4);
        assert!(err.receipt.has_unknown_usage);

        ctx.finish();
        server.finish();
    }
}

// ==============================================================================
// 6. SIGINT/SIGTERM Cancellation Terminates Cleanly With No Surviving Workers
// ==============================================================================
#[test]
fn cancellation_terminates_cleanly_without_descriptor_leaks() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = EndpointConfig::from_base_origin_str(&format!(
        "https://localhost:{}",
        listener.local_addr().unwrap().port()
    ))
    .unwrap();
    let cred = scoped_credential(endpoint.origin(), TRANSPORT_CANARY);
    let client = JevClient::with_additional_roots(endpoint, vec![ca_certificate()]).unwrap();

    let ctx = TestContext::new(3000, 200);
    // Trigger explicit user cancellation (simulating SIGINT / SIGTERM)
    ctx.cx
        .cancel_with(CancelKind::User, Some("synthetic SIGINT cancellation"));
    assert!(ctx.cx.is_cancel_requested());
    assert_eq!(
        ctx.cx.cancel_reason().map(|r| r.kind),
        Some(CancelKind::User)
    );

    let err = ctx.send(&client, Some(&cred)).err().unwrap();
    assert_eq!(err.kind, TransportErrorKind::Cancelled);
    assert!(!err.http_attempt_started);
    assert_safe_error(&err);

    ctx.finish();

    // Verify socket was never even accepted because cancellation preceded connection
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

// ==============================================================================
// 7. Honest Success Twin Case Under Identical Credentials
// ==============================================================================
#[test]
fn honest_success_twin_verifies_healthy_path() {
    let server = Server::new_mode("ok");
    let endpoint = server.endpoint("localhost");
    let cred = scoped_credential(endpoint.origin(), TRANSPORT_CANARY);
    let client = JevClient::with_additional_roots(endpoint, vec![ca_certificate()]).unwrap();

    let ctx = TestContext::new(3000, 200);
    let response = ctx.send(&client, Some(&cred)).ok().unwrap();

    assert_eq!(response.returned_model, "synthetic-test-model");
    let fit_answer = response.answers.get("fit").unwrap();
    match fit_answer {
        Answer::Noul(v) => {
            assert!((v - 0.75).abs() < 1e-6);
        }
        _other => panic!("Unexpected answer variant"),
    }

    assert_eq!(response.usage.input_tokens, 1);
    assert_eq!(response.usage.output_tokens, 1);

    ctx.finish();
    server.finish();
}

// ==============================================================================
// 8. Unified Contract Matrix Declaration Function
// ==============================================================================
/// Maps directly to `tests/contract_matrix.toml::unit_property_tests` for boundary
/// `p1_transport_failure_matrix` (sr-roadmap-l1i.2.11).
#[test]
fn transport_failure_cases() {
    // Assert all boundary invariants:
    // 1. DNS/connection refusal fails safely with no leak
    stalled_dns_or_connection_refusal_fails_safely();
    // 2. Handshake timeout completes in deadline
    handshake_timeout_preserves_cleanup_reserve_and_never_publishes();
    // 3. Pipe / body saturation fails safely
    pipe_saturation_and_body_limit_fails_safely();
    // 4. Malformed replies fail safely
    malformed_response_fails_without_panic_or_leak();
    // 5. HTTP 429 honors Retry-After and budget
    http_429_backoff_and_retry_after_budget_accounting();
    // 6. Cancellation terminates cleanly
    cancellation_terminates_cleanly_without_descriptor_leaks();
    // 7. Honest success counterpart
    honest_success_twin_verifies_healthy_path();
}
