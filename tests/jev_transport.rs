#![cfg(unix)]
//! Real TLS sockets and production client code. No in-memory HTTP substitute.

use asupersync::tls::Certificate;
use asupersync::{CancelKind, Cx};
use serde_json::{Value, json};
use skillranker::config::{ConfigSources, ResolvedConfig};
use skillranker::jev::client::{JevClient, TransportError, TransportErrorKind};
use skillranker::jev::codec::{Answer, Question, Request, Response};
use skillranker::jev::{CanonicalOrigin, EndpointConfig, OriginScopedCredential};
use skillranker::limits::DurationMillis;
use skillranker::privacy::{ConsentSource, NetworkBlock, NetworkConsent, ProviderAdmissionRefusal};
use skillranker::runtime::{EntryClock, ProcessInvocation};
use std::ffi::OsString;
use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

const CANARY: &str = "synthetic-transport-canary";
static NEXT_SERVER: AtomicU64 = AtomicU64::new(0);
const CONSENT: NetworkConsent = NetworkConsent::Authorized(ConsentSource::AllowNetworkFlag);

fn request() -> Request {
    Request::new(
        "jev-latest".into(),
        json!("synthetic transport probe"),
        [(
            "fit".into(),
            Question::Noul {
                instructions: json!("Return how strongly this synthetic example calls for help."),
                criteria: None,
            },
        )],
    )
    .unwrap()
}
fn credential(origin: &CanonicalOrigin, token: &str) -> OriginScopedCredential {
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
fn root() -> Certificate {
    Certificate::from_pem(include_bytes!("fixtures/jev-tls/ca.pem"))
        .unwrap()
        .remove(0)
}
struct Run {
    clock: EntryClock,
    invocation: ProcessInvocation,
    cx: Cx,
}
impl Run {
    fn new(ms: u64) -> Self {
        let clock = EntryClock::capture_with(
            DurationMillis::new("test-total", ms, 30_000).unwrap(),
            DurationMillis::new("test-cleanup", 100, 30_000).unwrap(),
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
        consent: NetworkConsent,
    ) -> Result<Response, TransportError> {
        self.invocation.runtime().block_on(client.send(
            &request(),
            key,
            consent,
            &self.cx,
            &self.clock,
        ))
    }
    fn finish(self) {
        assert!(self.invocation.shutdown(), "owned runtime must shut down");
    }
}
struct Server {
    child: Child,
    lines: BufReader<ChildStdout>,
    port: u16,
}
impl Server {
    fn new(mode: &str) -> Self {
        let directory = std::env::temp_dir().join(format!(
            "sr-jev-tls-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
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
    fn endpoint(&self, host: &str) -> EndpointConfig {
        EndpointConfig::from_base_origin_str(&format!("https://{host}:{}", self.port)).unwrap()
    }
    fn finish(mut self) -> Value {
        let mut line = String::new();
        self.lines.read_line(&mut line).unwrap();
        let report: Value = serde_json::from_str(&line).unwrap();
        assert!(
            self.child.wait().unwrap().success(),
            "real TLS server must finish"
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
fn failure(result: Result<Response, TransportError>) -> TransportError {
    match result {
        Err(error) => {
            let text = format!("{error} {error:?}");
            for private in [CANARY, "private-provider-body", "synthetic transport probe"] {
                assert!(!text.contains(private));
            }
            error
        }
        Ok(_) => panic!("expected transport refusal"),
    }
}

fn record(case: &str, run: &Run, error: Option<TransportErrorKind>, report: &Value) {
    eprintln!(
        "{}",
        json!({
            "schema_version": 1,
            "case_id": case,
            "stage": "https-transport",
            "source_version": "asupersync-81fb7b579ce5",
            "policy_version": "scoped-credential-no-retry-identity-v1",
            "elapsed_ms": run.clock.now().as_millis(),
            "decision": if error.is_some() { "refused" } else { "accepted" },
            "failure_kind": error.map(|kind| format!("{kind:?}")),
            "requests": report.get("requests"),
            "request_bytes": report.get("request_bytes"),
            "connection_closed": report.get("closed"),
        })
    );
}

#[test]
fn authenticated_json_post_uses_real_tls_and_closes_the_connection() {
    for mode in ["success", "chunked-success"] {
        let server = Server::new(mode);
        let endpoint = server.endpoint("localhost");
        let key = credential(endpoint.origin(), CANARY);
        let client = JevClient::with_additional_roots(endpoint, vec![root()]).unwrap();
        let run = Run::new(3000);
        let response = run.send(&client, Some(&key), CONSENT).unwrap();
        assert!(matches!(response.answers["fit"], Answer::Noul(0.75)));
        assert_eq!(response.requested_model, "jev-latest");
        assert_eq!(response.returned_model, "synthetic-test-model");
        assert_eq!(response.usage.input_tokens, 1);
        let report = server.finish();
        assert_eq!(report["requests"], 1);
        assert_eq!(report["closed"], true);
        record(mode, &run, None, &report);
        run.finish();
    }
}

#[test]
fn untrusted_chain_and_wrong_hostname_fail_before_http() {
    for (host, trust) in [("localhost", false), ("127.0.0.1", true)] {
        let server = Server::new("success");
        let endpoint = server.endpoint(host);
        let key = credential(endpoint.origin(), CANARY);
        let client =
            JevClient::with_additional_roots(endpoint, if trust { vec![root()] } else { vec![] })
                .unwrap();
        let run = Run::new(3000);
        let error = failure(run.send(&client, Some(&key), CONSENT));
        assert_eq!(error.kind, TransportErrorKind::Tls);
        assert!(error.http_attempt_started);
        let report = server.finish();
        assert_eq!(report["handshake_rejected"], true);
        assert_eq!(report["requests"], 0);
        record(
            if trust {
                "wrong-hostname"
            } else {
                "untrusted-chain"
            },
            &run,
            Some(error.kind),
            &report,
        );
        run.finish();
    }
}

#[test]
fn consent_credentials_cancellation_and_deadline_gate_before_connect() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = EndpointConfig::from_base_origin_str(&format!(
        "https://localhost:{}",
        listener.local_addr().unwrap().port()
    ))
    .unwrap();
    let key = credential(endpoint.origin(), CANARY);
    let foreign = credential(&CanonicalOrigin::production(), CANARY);
    let client = JevClient::new(endpoint).unwrap();
    for (consent, expected) in [
        (
            NetworkConsent::NotAuthorized,
            ProviderAdmissionRefusal::NetworkNotAuthorized,
        ),
        (
            NetworkConsent::Blocked(NetworkBlock::Offline),
            ProviderAdmissionRefusal::Offline,
        ),
        (
            NetworkConsent::Blocked(NetworkBlock::DryRun),
            ProviderAdmissionRefusal::DryRun,
        ),
    ] {
        let run = Run::new(3000);
        let error = failure(run.send(&client, Some(&key), consent));
        assert_eq!(error.kind, TransportErrorKind::Admission(expected));
        assert!(!error.http_attempt_started);
        run.finish();
    }
    let run = Run::new(3000);
    assert_eq!(
        failure(run.send(&client, None, CONSENT)).kind,
        TransportErrorKind::Admission(ProviderAdmissionRefusal::MissingCredential)
    );
    assert_eq!(
        failure(run.send(&client, Some(&foreign), CONSENT)).kind,
        TransportErrorKind::CredentialOriginMismatch
    );
    run.cx
        .cancel_with(CancelKind::User, Some("synthetic cancellation"));
    let error = failure(run.send(&client, Some(&key), CONSENT));
    assert_eq!(error.kind, TransportErrorKind::Cancelled);
    assert!(!error.http_attempt_started);
    run.finish();
    let run = Run::new(200);
    std::thread::sleep(Duration::from_millis(120));
    let error = failure(run.send(&client, Some(&key), CONSENT));
    assert_eq!(error.kind, TransportErrorKind::Deadline);
    assert!(!error.http_attempt_started);
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    run.finish();
}

#[test]
fn redirects_errors_oversized_and_encoded_bodies_fail_without_hidden_retry() {
    for (mode, expected) in [
        ("redirect", TransportErrorKind::Redirect),
        ("unavailable", TransportErrorKind::HttpStatus(503)),
        ("oversized", TransportErrorKind::BodyTooLarge),
        ("chunked-overflow", TransportErrorKind::BodyTooLarge),
        ("compression", TransportErrorKind::UnsupportedEncoding),
        ("wrong-type", TransportErrorKind::InvalidContentType),
        (
            "malformed",
            TransportErrorKind::Response(skillranker::jev::codec::CodecError::InvalidJson),
        ),
    ] {
        let server = Server::new(mode);
        let endpoint = server.endpoint("localhost");
        let key = credential(endpoint.origin(), CANARY);
        let client = JevClient::with_additional_roots(endpoint, vec![root()]).unwrap();
        let run = Run::new(3000);
        let error = failure(run.send(&client, Some(&key), CONSENT));
        assert_eq!(error.kind, expected, "case {mode}");
        assert!(error.http_attempt_started);
        let report = server.finish();
        assert_eq!(report["requests"], 1);
        assert_eq!(report["closed"], true);
        if mode == "redirect" {
            assert_eq!(report["redirect_connections"], 0);
        }
        record(mode, &run, Some(error.kind), &report);
        run.finish();
    }
}

#[test]
fn handshake_and_body_deadlines_close_owned_connections() {
    for mode in ["slow-handshake", "slow-body"] {
        let server = Server::new(mode);
        let endpoint = server.endpoint("localhost");
        let key = credential(endpoint.origin(), CANARY);
        let client = JevClient::with_additional_roots(endpoint, vec![root()]).unwrap();
        let run = Run::new(1000);
        let started = Instant::now();
        let error = failure(run.send(&client, Some(&key), CONSENT));
        assert_eq!(error.kind, TransportErrorKind::Deadline, "case {mode}");
        assert!(error.http_attempt_started);
        assert!(started.elapsed() < Duration::from_secs(3));
        let report = server.finish();
        assert_eq!(report["closed"], true);
        if mode == "slow-handshake" {
            assert!(report["handshake_bytes"].as_u64().unwrap() > 0);
        } else {
            assert_eq!(report["requests"], 1);
        }
        record(mode, &run, Some(error.kind), &report);
        run.finish();
    }
}

#[test]
fn cancellation_during_body_read_closes_the_connection() {
    let mut server = Server::new("cancel-body");
    let endpoint = server.endpoint("localhost");
    let key = credential(endpoint.origin(), CANARY);
    let client = JevClient::with_additional_roots(endpoint, vec![root()]).unwrap();
    let run = Run::new(3000);
    let cancel = run.cx.clone();
    let thread = std::thread::spawn(move || {
        let mut line = String::new();
        server.lines.read_line(&mut line).unwrap();
        let stage: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(stage["body_pending"], true);
        let cancelled_at = Instant::now();
        cancel.cancel_with(CancelKind::User, Some("synthetic body cancellation"));
        (server.finish(), cancelled_at)
    });
    let result = run.send(&client, Some(&key), CONSENT);
    let completed_at = Instant::now();
    let (report, cancelled_at) = thread.join().unwrap();
    let error = failure(result);
    assert_eq!(error.kind, TransportErrorKind::Cancelled);
    assert!(error.http_attempt_started);
    assert!(completed_at.saturating_duration_since(cancelled_at) < Duration::from_secs(1));
    assert_eq!(report["requests"], 1);
    assert_eq!(report["closed"], true);
    record("cancel-body", &run, Some(error.kind), &report);
    run.finish();
}

#[test]
#[ignore = "requires explicit live test invocation and TYPESAFE_API_KEY; sends synthetic data only"]
fn actual_typesafe_https_accepts_synthetic_typed_request() {
    let token = std::env::var("TYPESAFE_API_KEY").expect("explicit live test credential");
    let endpoint = EndpointConfig::production();
    let key = credential(endpoint.origin(), &token);
    let client = JevClient::new(endpoint).unwrap();
    let run = Run::new(30_000);
    let response = run.send(&client, Some(&key), CONSENT).unwrap();
    assert!(matches!(response.answers["fit"], Answer::Noul(value) if (0.0..=1.0).contains(&value)));
    assert_eq!(response.requested_model, "jev-latest");
    record("actual-typesafe", &run, None, &json!({"requests": 1}));
    run.finish();
}

#[test]
fn full_choice_mass_deficit_is_rejected_over_real_tls_without_relaxing_rounding() {
    use skillranker::jev::codec::{CodecError, MAX_CHOICE_OPTIONS};
    let options = (0..MAX_CHOICE_OPTIONS - 1)
        .map(|i| (format!("skill_{i:03}"), "Synthetic skill".to_owned()))
        .chain(std::iter::once(("__none__".to_owned(), "None".to_owned())));
    let request = Request::new(
        "synthetic-test-model".into(),
        json!("synthetic transport probe"),
        [(
            "rank".into(),
            Question::choice(json!("Synthetic choice"), options).unwrap(),
        )],
    )
    .unwrap();
    // Reproduce the measured count/sum failure class, not the unavailable raw
    // provider body. Honest valid and tolerated-rounding counterparts traverse
    // the same sockets, verified TLS, body parser and production decoder.
    for (mode, expected_sum) in [
        ("choice-255-deficit", None),
        ("choice-255-valid", Some(1.0)),
        ("choice-255-rounding", Some(0.99995)),
    ] {
        let server = Server::new(mode);
        let endpoint = server.endpoint("localhost");
        let key = credential(endpoint.origin(), CANARY);
        let client = JevClient::with_additional_roots(endpoint, vec![root()]).unwrap();
        let run = Run::new(3000);
        let result = run.invocation.runtime().block_on(client.send(
            &request,
            Some(&key),
            CONSENT,
            &run.cx,
            &run.clock,
        ));
        let error = if let Some(sum) = expected_sum {
            let response = result.unwrap();
            let Answer::Choice(answer) = &response.answers["rank"] else {
                panic!("expected validated choice");
            };
            assert_eq!(answer.raw_probabilities().len(), 255);
            assert!((answer.raw_sum() - sum).abs() < 1e-12);
            assert!((answer.raw_probabilities()["__none__"] - sum / 255.0).abs() < 1e-12);
            assert_eq!(response.usage.input_tokens, 1);
            None
        } else {
            let error = failure(result);
            assert_eq!(
                error.kind,
                TransportErrorKind::Response(CodecError::InvalidDistribution)
            );
            assert!(
                error.http_attempt_started,
                "refused response cannot establish zero provider cost"
            );
            Some(error.kind)
        };
        let report = server.finish();
        assert_eq!(report["requests"], 1);
        assert_eq!(report["closed"], true);
        record(mode, &run, error, &report);
        run.finish();
    }
}
