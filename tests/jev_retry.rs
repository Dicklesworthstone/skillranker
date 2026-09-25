#![cfg(unix)]
//! Production retry/admission paths over real local TLS, plus deterministic
//! scheduling tests. All data and trust roots are public synthetic fixtures.

use asupersync::tls::Certificate;
use asupersync::{CancelKind, Cx};
use serde_json::{Value, json};
use skillranker::config::{ConfigSources, ResolvedConfig};
use skillranker::jev::client::{JevClient, TransportError, TransportErrorKind};
use skillranker::jev::retry::{
    ModelPairStatus, RetryAfter, RetryErrorKind, RetrySession, RetryStop, retry_delay, retryable,
};
use skillranker::jev::{
    AdmissionError, AdmissionRefusal, AttemptAdmission, AttemptBudget, AttemptFailure,
    CanonicalOrigin, EndpointConfig, OriginScopedCredential, Question, RankingStage, Request,
    Usage,
};
use skillranker::limits::DurationMillis;
use skillranker::privacy::{ConsentSource, NetworkConsent};
use skillranker::runtime::{EntryClock, ProcessInvocation};
use std::ffi::OsString;
use std::future::{Future, poll_fn};
use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::Poll;
use std::time::{Duration, SystemTime};

const CONSENT: NetworkConsent = NetworkConsent::Authorized(ConsentSource::AllowNetworkFlag);
const CANARY: &str = "synthetic-retry-canary";

fn request(model: &str) -> Request {
    Request::new(
        model.into(),
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
fn credential(origin: &CanonicalOrigin) -> OriginScopedCredential {
    let config = ResolvedConfig::resolve(
        ConfigSources {
            environment: vec![(OsString::from("TYPESAFE_API_KEY"), OsString::from(CANARY))],
            ..Default::default()
        },
        1,
    )
    .unwrap();
    OriginScopedCredential::bind(config.credential().unwrap().clone(), origin).unwrap()
}
fn clock(ms: u64) -> EntryClock {
    EntryClock::capture_with(
        DurationMillis::new("test", ms, 30_000).unwrap(),
        DurationMillis::new("cleanup", 100, 30_000).unwrap(),
    )
    .unwrap()
}
struct Run {
    clock: EntryClock,
    invocation: ProcessInvocation,
    cx: Cx,
}
impl Run {
    fn new(ms: u64) -> Self {
        let clock = clock(ms);
        let invocation = ProcessInvocation::from_clock(clock).unwrap();
        let cx = invocation.request_cx().unwrap();
        Self {
            clock,
            invocation,
            cx,
        }
    }
    fn session<'a>(
        &self,
        client: &'a JevClient,
        key: &'a OriginScopedCredential,
    ) -> RetrySession<'a> {
        RetrySession::new(
            client,
            Some(key),
            self.clock,
            AttemptBudget::default(),
            "synthetic-invocation",
        )
        .unwrap()
    }
    fn finish(self) {
        assert!(self.invocation.shutdown());
    }
}
struct Server {
    child: Child,
    lines: BufReader<ChildStdout>,
    port: u16,
}
impl Server {
    fn new(steps: &[&str]) -> Self {
        // Concurrent fixtures can observe the same wall-clock tick on Darwin.
        static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);
        let directory = std::env::temp_dir().join(format!(
            "sr-retry-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
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
        Self {
            child,
            lines,
            port: u16::try_from(hello["port"].as_u64().unwrap()).unwrap(),
        }
    }
    fn client(&self) -> (JevClient, OriginScopedCredential) {
        let endpoint =
            EndpointConfig::from_base_origin_str(&format!("https://localhost:{}", self.port))
                .unwrap();
        let key = credential(endpoint.origin());
        let roots = Certificate::from_pem(include_bytes!("fixtures/jev-tls/ca.pem")).unwrap();
        (
            JevClient::with_additional_roots(endpoint, roots).unwrap(),
            key,
        )
    }
    fn finish(mut self, case: &str, expected: u64) -> Value {
        let mut line = String::new();
        self.lines.read_line(&mut line).unwrap();
        let report: Value = serde_json::from_str(&line).unwrap();
        assert!(self.child.wait().unwrap().success());
        assert_eq!(report["requests"], expected, "{case}");
        assert_eq!(report["extra_connections"], 0, "{case}");
        assert!(
            report["closed"]
                .as_array()
                .unwrap()
                .iter()
                .all(|v| v == true)
        );
        eprintln!(
            "{}",
            json!({"schema_version":1, "source_version":"asupersync-81fb7b5", "policy_version":"jev-retry-v1", "case_id":case, "stage":"retry-https", "decision":"assertions-passed", "report":report})
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
fn transient_sequence_then_rerank_uses_exactly_four_admissions() {
    let server = Server::new(&["429:0", "529", "ok", "ok"]);
    let (client, key) = server.client();
    let run = Run::new(5000);
    let mut session = run.session(&client, &key);
    let mut authorizations = 0;
    let wide = run
        .invocation
        .runtime()
        .block_on(
            session.send_stage(RankingStage::Wide, &request("jev-latest"), &run.cx, || {
                authorizations += 1;
                Ok(CONSENT)
            }),
        )
        .unwrap();
    assert_eq!(wide.attempts, 3);
    assert_eq!(wide.receipt.unknown_usage_attempts, 2);
    assert_eq!(wide.receipt.total_tokens(), 5);
    assert!(wide.model.received_elapsed_ms >= wide.model.started_elapsed_ms);
    let rerank = run
        .invocation
        .runtime()
        .block_on(session.send_stage(
            RankingStage::Rerank,
            &request("jev-latest"),
            &run.cx,
            || {
                authorizations += 1;
                Ok(CONSENT)
            },
        ))
        .unwrap();
    assert_eq!(rerank.attempts, 1);
    assert_eq!(
        rerank.pair_status,
        ModelPairStatus::MatchingUnpinnedIdentifiers
    );
    assert_eq!(rerank.receipt.admitted_attempts, 4);
    assert_eq!(rerank.receipt.sent_attempts, 4);
    assert_eq!(rerank.receipt.completed_attempts, 2);
    assert_eq!(rerank.receipt.total_tokens(), 10);
    assert_eq!(authorizations, 4);
    let error = run
        .invocation
        .runtime()
        .block_on(session.send_stage(
            RankingStage::Rerank,
            &request("jev-latest"),
            &run.cx,
            || Ok(CONSENT),
        ))
        .err()
        .unwrap();
    assert_eq!(error.receipt, rerank.receipt);
    assert!(matches!(error.kind, RetryErrorKind::Admission(_)));
    server.finish("two-stage-four-attempts", 4);
    run.finish();
}

#[test]
fn a_rerank_reuses_the_wide_connection_and_closes_it() {
    let server = Server::new(&["reuse", "ok"]);
    let (client, key) = server.client();
    let run = Run::new(5000);
    let mut session = run.session(&client, &key);
    for stage in [RankingStage::Wide, RankingStage::Rerank] {
        let answer = run
            .invocation
            .runtime()
            .block_on(session.send_stage(stage, &request("jev-latest"), &run.cx, || Ok(CONSENT)))
            .unwrap();
        assert_eq!(answer.attempts, 1);
    }
    let report = server.finish("rerank-reuses-wide-connection", 2);
    // One handshake for both stages. The Wide attempt kept its connection;
    // the Rerank, the invocation's last request, asked to close it and did.
    assert_eq!(report["connections"], 1);
    assert_ne!(report["connection_headers"][0], "close");
    assert_eq!(report["connection_headers"][1], "close");
    assert_eq!(report["closed"], json!([true]));
    run.finish();
}

#[test]
fn a_wide_answered_with_close_is_not_reused() {
    // The provider may refuse keep-alive: the client then opens a fresh
    // connection for the Rerank, and still closes it afterwards.
    let server = Server::new(&["ok", "ok"]);
    let (client, key) = server.client();
    let run = Run::new(5000);
    let mut session = run.session(&client, &key);
    for stage in [RankingStage::Wide, RankingStage::Rerank] {
        run.invocation
            .runtime()
            .block_on(session.send_stage(stage, &request("jev-latest"), &run.cx, || Ok(CONSENT)))
            .unwrap();
    }
    let report = server.finish("wide-close-not-reused", 2);
    assert_eq!(report["connections"], 2);
    assert_eq!(report["connection_headers"][1], "close");
    assert_eq!(report["closed"], json!([true, true]));
    run.finish();
}

#[test]
fn four_transient_errors_never_send_a_fifth_attempt() {
    let server = Server::new(&["503", "502", "500", "504"]);
    let (client, key) = server.client();
    let run = Run::new(5000);
    let mut session = run.session(&client, &key);
    let error = run
        .invocation
        .runtime()
        .block_on(
            session.send_stage(RankingStage::Wide, &request("jev-latest"), &run.cx, || {
                Ok(CONSENT)
            }),
        )
        .err()
        .unwrap();
    assert!(matches!(
        error.kind,
        RetryErrorKind::Admission(AdmissionRefusal::AttemptsExhausted {
            attempts_used: 4,
            limit: 4
        })
    ));
    assert_eq!(error.receipt.unknown_usage_attempts, 4);
    assert_eq!(error.receipt.completed_attempts, 0);
    assert_eq!(error.receipt.known_usage.total_tokens(), 0);
    server.finish("no-fifth-attempt", 4);
    run.finish();
}

#[test]
fn authentication_validation_and_malformed_answers_do_not_retry() {
    for mode in ["401", "403", "400", "422", "501", "malformed"] {
        let server = Server::new(&[mode]);
        let (client, key) = server.client();
        let run = Run::new(3000);
        let mut session = run.session(&client, &key);
        let error = run
            .invocation
            .runtime()
            .block_on(session.send_stage(
                RankingStage::Wide,
                &request("jev-latest"),
                &run.cx,
                || Ok(CONSENT),
            ))
            .err()
            .unwrap();
        assert!(matches!(error.kind, RetryErrorKind::Transport(_)));
        assert_eq!(error.receipt.admitted_attempts, 1);
        assert_eq!(error.receipt.unknown_usage_attempts, 1);
        for private in [CANARY, "private-provider-body", "synthetic retry probe"] {
            assert!(!format!("{error:?} {error}").contains(private));
        }
        server.finish(mode, 1);
        run.finish();
    }
}

#[test]
fn retry_after_honors_delay_and_refuses_unfittable_or_ambiguous_headers() {
    let server = Server::new(&["429:1", "ok"]);
    let (client, key) = server.client();
    let run = Run::new(5000);
    let mut session = run.session(&client, &key);
    let response = run
        .invocation
        .runtime()
        .block_on(
            session.send_stage(RankingStage::Wide, &request("jev-latest"), &run.cx, || {
                Ok(CONSENT)
            }),
        )
        .unwrap();
    assert_eq!(response.attempts, 2);
    let report = server.finish("retry-after-one-second", 2);
    assert!(
        report["received_ns"][1].as_u64().unwrap() - report["received_ns"][0].as_u64().unwrap()
            >= 1_000_000_000
    );
    run.finish();
    for (mode, stop) in [
        ("429:86400", RetryStop::DoesNotFitDeadline),
        ("503:duplicate", RetryStop::InvalidRetryAfter),
        ("529:bad", RetryStop::InvalidRetryAfter),
    ] {
        let server = Server::new(&[mode]);
        let (client, key) = server.client();
        let run = Run::new(3000);
        let mut session = run.session(&client, &key);
        let error = run
            .invocation
            .runtime()
            .block_on(session.send_stage(
                RankingStage::Wide,
                &request("jev-latest"),
                &run.cx,
                || Ok(CONSENT),
            ))
            .err()
            .unwrap();
        assert_eq!(error.kind, RetryErrorKind::RetryStopped(stop));
        assert_eq!(error.receipt.admitted_attempts, 1);
        assert!(run.clock.now().as_millis() < 2000);
        server.finish(mode, 1);
        run.finish();
    }
}

#[test]
fn paid_rerank_timeout_preserves_wide_cost_and_unknown_usage() {
    let server = Server::new(&["ok", "slow"]);
    let (client, key) = server.client();
    let run = Run::new(1000);
    let mut session = run.session(&client, &key);
    run.invocation
        .runtime()
        .block_on(
            session.send_stage(RankingStage::Wide, &request("jev-latest"), &run.cx, || {
                Ok(CONSENT)
            }),
        )
        .unwrap();
    let error = run
        .invocation
        .runtime()
        .block_on(session.send_stage(
            RankingStage::Rerank,
            &request("jev-latest"),
            &run.cx,
            || Ok(CONSENT),
        ))
        .err()
        .unwrap();
    assert_eq!(
        error.kind,
        RetryErrorKind::Transport(TransportErrorKind::Deadline)
    );
    assert_eq!(error.receipt.admitted_attempts, 2);
    assert_eq!(error.receipt.completed_attempts, 1);
    assert_eq!(error.receipt.total_tokens(), 5);
    assert_eq!(error.receipt.unknown_usage_attempts, 1);
    // The server's independent no-extra-connection observation takes 250ms.
    // Shut down within the original 100ms cleanup reserve before collecting
    // that report; test observation must not consume the runtime's deadline.
    assert!(run.clock.remaining_until_expiry().as_millis() > 0);
    run.finish();
    server.finish("paid-rerank-timeout", 2);
}

#[test]
fn policy_is_refreshed_before_retry_and_preserves_failed_attempt_cost() {
    let server = Server::new(&["503"]);
    let (client, key) = server.client();
    let run = Run::new(3000);
    let mut session = run.session(&client, &key);
    let mut calls = 0;
    let error = run
        .invocation
        .runtime()
        .block_on(
            session.send_stage(RankingStage::Wide, &request("jev-latest"), &run.cx, || {
                calls += 1;
                if calls == 1 { Ok(CONSENT) } else { Err(()) }
            }),
        )
        .err()
        .unwrap();
    assert_eq!(error.kind, RetryErrorKind::PolicyChanged);
    assert_eq!(calls, 2);
    assert_eq!(error.receipt.admitted_attempts, 1);
    assert_eq!(error.receipt.unknown_usage_attempts, 1);
    server.finish("revoked-before-retry", 1);
    run.finish();
}

#[test]
fn different_returned_revisions_invalidate_pair_but_retain_both_costs() {
    let server = Server::new(&["ok:immutable-r1", "ok:immutable-r2"]);
    let (client, key) = server.client();
    let run = Run::new(3000);
    let mut session = run.session(&client, &key);
    run.invocation
        .runtime()
        .block_on(
            session.send_stage(RankingStage::Wide, &request("jev-latest"), &run.cx, || {
                Ok(CONSENT)
            }),
        )
        .unwrap();
    let error = run
        .invocation
        .runtime()
        .block_on(session.send_stage(
            RankingStage::Rerank,
            &request("jev-latest"),
            &run.cx,
            || Ok(CONSENT),
        ))
        .err()
        .unwrap();
    assert_eq!(error.kind, RetryErrorKind::ModelPairMismatch);
    assert_eq!(error.receipt.completed_attempts, 2);
    assert_eq!(error.receipt.total_tokens(), 10);
    assert_eq!(session.wide_model().unwrap().returned, "immutable-r1");
    assert_eq!(session.last_model().unwrap().returned, "immutable-r2");
    server.finish("model-pair-mismatch", 2);
    run.finish();
}

#[test]
fn cancellation_during_paid_body_is_unknown_cost_and_closes_socket() {
    let mut server = Server::new(&["cancel"]);
    let (client, key) = server.client();
    let run = Run::new(3000);
    let mut session = run.session(&client, &key);
    let cancel = run.cx.clone();
    let observer = std::thread::spawn(move || {
        let mut line = String::new();
        server.lines.read_line(&mut line).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&line).unwrap()["body_pending"],
            true
        );
        cancel.cancel_with(CancelKind::User, Some("synthetic cancellation"));
        server.finish("cancel-paid-body", 1)
    });
    let error = run
        .invocation
        .runtime()
        .block_on(
            session.send_stage(RankingStage::Wide, &request("jev-latest"), &run.cx, || {
                Ok(CONSENT)
            }),
        )
        .err()
        .unwrap();
    assert_eq!(
        error.kind,
        RetryErrorKind::Transport(TransportErrorKind::Cancelled)
    );
    assert_eq!(error.receipt.unknown_usage_attempts, 1);
    observer.join().unwrap();
    run.finish();
}

#[test]
fn cancellation_during_retry_after_wait_does_not_admit_another_attempt() {
    let server = Server::new(&["429:1"]);
    let (client, key) = server.client();
    let run = Run::new(3000);
    let mut session = run.session(&client, &key);
    let cancel = run.cx.clone();
    let observer = std::thread::spawn(move || {
        // Server completion establishes first response and socket teardown;
        // its 250ms extra-connection observation is within the 1s retry wait.
        server.finish("cancel-retry-wait", 1);
        cancel.cancel_with(CancelKind::User, Some("synthetic wait cancellation"));
    });
    let error = run
        .invocation
        .runtime()
        .block_on(
            session.send_stage(RankingStage::Wide, &request("jev-latest"), &run.cx, || {
                Ok(CONSENT)
            }),
        )
        .err()
        .unwrap();
    assert_eq!(
        error.kind,
        RetryErrorKind::Transport(TransportErrorKind::Cancelled)
    );
    assert_eq!(error.receipt.admitted_attempts, 1);
    assert_eq!(error.receipt.unknown_usage_attempts, 1);
    observer.join().unwrap();
    run.finish();
}

#[test]
fn preflight_refusal_is_zero_sent_and_dropped_http_future_is_unknown() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = EndpointConfig::from_base_origin_str(&format!(
        "https://localhost:{}",
        listener.local_addr().unwrap().port()
    ))
    .unwrap();
    let key = credential(endpoint.origin());
    let client = JevClient::new(endpoint).unwrap();
    let run = Run::new(3000);
    let mut session = run.session(&client, &key);
    let error = run
        .invocation
        .runtime()
        .block_on(
            session.send_stage(RankingStage::Wide, &request("jev-latest"), &run.cx, || {
                Ok(NetworkConsent::NotAuthorized)
            }),
        )
        .err()
        .unwrap();
    assert_eq!(error.receipt.sent_attempts, 0);
    assert_eq!(error.receipt.unknown_usage_attempts, 0);
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    let req = request("jev-latest");
    let mut future =
        Box::pin(session.send_stage(RankingStage::Wide, &req, &run.cx, || Ok(CONSENT)));
    run.invocation.runtime().block_on(poll_fn(|cx| {
        assert!(future.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    }));
    drop(future);
    assert_eq!(session.receipt().sent_attempts, 1);
    assert_eq!(session.receipt().unknown_usage_attempts, 1);
    run.finish();
}

#[test]
fn retry_schedule_and_http_dates_are_deterministic_bounded_and_overflow_safe() {
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(784111777);
    assert_eq!(
        RetryAfter::parse("Sun, 06 Nov 1994 08:49:38 GMT", now),
        RetryAfter::Delay(Duration::from_secs(1))
    );
    assert_eq!(
        RetryAfter::parse("Sunday, 06-Nov-94 08:49:38 GMT", now),
        RetryAfter::Delay(Duration::from_secs(1))
    );
    assert_eq!(
        RetryAfter::parse("Sun Nov  6 08:49:38 1994", now),
        RetryAfter::Delay(Duration::from_secs(1))
    );
    assert_eq!(
        RetryAfter::parse(
            "Sun, 06 Nov 1994 08:49:37 GMT",
            now + Duration::from_secs(1)
        ),
        RetryAfter::Delay(Duration::ZERO)
    );
    for input in ["-1", "+1", "1.5", "", "1,2", "\u{a0}1", "secret-canary"] {
        assert_eq!(RetryAfter::parse(input, now), RetryAfter::Invalid);
    }
    assert_eq!(
        RetryAfter::parse("999999999999999999999999", now),
        RetryAfter::Delay(Duration::MAX)
    );
    let mut error = TransportError {
        kind: TransportErrorKind::HttpStatus(429),
        http_attempt_started: true,
        retry_after: RetryAfter::Absent,
    };
    for failures in 1..=100 {
        for entropy in [0, 1, 50, 51, u64::MAX] {
            let delay = retry_delay(
                &error,
                failures,
                Duration::from_secs(3),
                Duration::from_millis(50),
                entropy,
            )
            .unwrap();
            assert!((Duration::from_millis(100)..=Duration::from_millis(850)).contains(&delay));
        }
    }
    error.retry_after = RetryAfter::Delay(Duration::from_secs(1));
    assert_eq!(
        retry_delay(
            &error,
            1,
            Duration::from_millis(1050),
            Duration::from_millis(50),
            0
        ),
        Err(RetryStop::DoesNotFitDeadline)
    );
    assert_eq!(
        retry_delay(
            &error,
            1,
            Duration::from_millis(1051),
            Duration::from_millis(50),
            0
        ),
        Ok(Duration::from_secs(1))
    );
    error.retry_after = RetryAfter::Delay(Duration::MAX);
    assert_eq!(
        retry_delay(
            &error,
            u32::MAX,
            Duration::from_secs(60),
            Duration::ZERO,
            50
        ),
        Err(RetryStop::DoesNotFitDeadline)
    );
    for status in 100..600 {
        assert_eq!(
            retryable(TransportErrorKind::HttpStatus(status)),
            [429, 500, 502, 503, 504, 529].contains(&status)
        );
    }
    for kind in [
        TransportErrorKind::Tls,
        TransportErrorKind::Deadline,
        TransportErrorKind::Cancelled,
        TransportErrorKind::Protocol,
        TransportErrorKind::Redirect,
    ] {
        assert!(!retryable(kind));
    }
}

#[test]
fn accounting_rejects_duplicate_foreign_and_out_of_order_completion() {
    let mut first = AttemptAdmission::default_invocation(clock(3000), "same-id");
    let mut second = AttemptAdmission::default_invocation(clock(3000), "same-id");
    let endpoint = CanonicalOrigin::production();
    let sent = first
        .admit(RankingStage::Wide, &endpoint)
        .unwrap()
        .mark_sent()
        .unwrap();
    let foreign = second
        .admit(RankingStage::Wide, &endpoint)
        .unwrap()
        .mark_sent()
        .unwrap();
    assert!(first.admit(RankingStage::Wide, &endpoint).is_err());
    let usage = Usage {
        input_tokens: 3,
        output_tokens: 2,
    };
    assert_eq!(
        first.record_response(&sent, usage),
        Err(AdmissionError::AttemptNotActive)
    );
    assert_eq!(
        first.record_sent(&foreign),
        Err(AdmissionError::AttemptNotActive)
    );
    first.record_sent(&sent).unwrap();
    assert_eq!(
        first.record_sent(&sent),
        Err(AdmissionError::AttemptNotActive)
    );
    first.record_response(&sent, usage).unwrap();
    assert_eq!(
        first.record_response(&sent, usage),
        Err(AdmissionError::AttemptNotActive)
    );
    assert_eq!(
        first.record_terminal_failure(&sent, AttemptFailure::Local("duplicate")),
        Err(AdmissionError::AttemptNotActive)
    );
    first.record_cache_hit();
    assert_eq!(first.receipt().admitted_attempts, 1);
    assert_eq!(first.receipt().total_tokens(), 5);
    let discard = first
        .admit(RankingStage::Rerank, &endpoint)
        .unwrap()
        .discard_before_send("synthetic");
    first.record_discard(&discard).unwrap();
    assert!(
        first.can_attempt_rerank(),
        "a retry uses the existing logical request"
    );
    assert_eq!(
        first.record_discard(&discard),
        Err(AdmissionError::AttemptNotActive)
    );
    second.record_sent(&foreign).unwrap();
    second
        .record_terminal_failure(&foreign, AttemptFailure::Indeterminate("transient-io"))
        .unwrap();
    assert_eq!(second.receipt().unknown_usage_attempts, 1);
}

#[test]
fn successful_evaluations_consume_distinct_logical_requests() {
    let mut admission = AttemptAdmission::default_invocation(clock(3000), "eval");
    for _ in 0..2 {
        let sent = admission
            .admit(RankingStage::Evaluation, &CanonicalOrigin::production())
            .unwrap()
            .mark_sent()
            .unwrap();
        admission.record_sent(&sent).unwrap();
        admission
            .record_response(
                &sent,
                Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            )
            .unwrap();
    }
    assert!(matches!(
        admission.admit(RankingStage::Evaluation, &CanonicalOrigin::production()),
        Err(AdmissionRefusal::LogicalRequestsExhausted { .. })
    ));
    assert_eq!(admission.receipt().admitted_attempts, 2);
}

#[test]
fn usage_overflow_preserves_previous_exact_counts_and_marks_new_attempt_unknown() {
    let mut admission = AttemptAdmission::default_invocation(clock(3000), "overflow");
    let origin = CanonicalOrigin::production();
    let wide = admission
        .admit(RankingStage::Wide, &origin)
        .unwrap()
        .mark_sent()
        .unwrap();
    admission.record_sent(&wide).unwrap();
    admission
        .record_response(
            &wide,
            Usage {
                input_tokens: u64::MAX,
                output_tokens: 0,
            },
        )
        .unwrap();
    let rerank = admission
        .admit(RankingStage::Rerank, &origin)
        .unwrap()
        .mark_sent()
        .unwrap();
    admission.record_sent(&rerank).unwrap();
    assert_eq!(
        admission.record_response(
            &rerank,
            Usage {
                input_tokens: 1,
                output_tokens: 0
            }
        ),
        Err(AdmissionError::UsageOverflow)
    );
    assert_eq!(admission.receipt().completed_attempts, 1);
    assert_eq!(admission.receipt().total_tokens(), u64::MAX);
    assert_eq!(admission.receipt().unknown_usage_attempts, 1);
    assert_eq!(
        admission.record_terminal_failure(&rerank, AttemptFailure::Local("again")),
        Err(AdmissionError::AttemptNotActive)
    );
}

/// Records whether each attempt carried a credential, and answers it.
struct CredentialProbe {
    origin: Option<CanonicalOrigin>,
    carried: std::sync::Mutex<Vec<bool>>,
}

impl skillranker::jev::client::JevTransport for CredentialProbe {
    fn send<'a>(
        &'a self,
        request: &'a Request,
        credential: Option<&'a OriginScopedCredential>,
        _consent: NetworkConsent,
        _cx: &'a Cx,
        _clock: &'a EntryClock,
    ) -> skillranker::jev::client::TransportFuture<'a> {
        self.carried.lock().unwrap().push(credential.is_some());
        let answer = request
            .decode_response(
                br#"{"model":"jev-test","answers":{"fit":{"type":"noul","noul":0.5}},"usage":{"input_tokens":1,"output_tokens":1}}"#,
            )
            .map_err(|error| TransportError {
                kind: TransportErrorKind::Response(error),
                http_attempt_started: true,
                retry_after: skillranker::jev::retry::RetryAfter::Absent,
            });
        Box::pin(async move { answer })
    }

    fn origin(&self) -> Option<&CanonicalOrigin> {
        self.origin.as_ref()
    }
}

#[test]
fn an_origin_less_transport_never_receives_a_credential() {
    let origin = EndpointConfig::production().origin().clone();
    let key = credential(&origin);
    // Twin: a transport bound to the credential's origin does receive it.
    for (bound, carried) in [(None, false), (Some(origin.clone()), true)] {
        let probe = CredentialProbe {
            origin: bound,
            carried: std::sync::Mutex::default(),
        };
        let run = Run::new(5_000);
        let mut session = RetrySession::new(
            &probe,
            Some(&key),
            run.clock,
            AttemptBudget::default(),
            "synthetic-invocation",
        )
        .unwrap();
        let answer = run.invocation.runtime().block_on(session.send_stage(
            RankingStage::Wide,
            &request("jev-test"),
            &run.cx,
            || Ok(CONSENT),
        ));
        assert!(answer.is_ok());
        assert_eq!(*probe.carried.lock().unwrap(), vec![carried]);
        drop(session);
        run.finish();
    }
}
