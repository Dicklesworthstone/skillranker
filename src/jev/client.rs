//! One authenticated, bounded HTTPS attempt through Asupersync.
//!
//! Retries, attempt budgets and ranking policy belong to the caller. This
//! boundary never follows redirects, inherits a proxy, or retries internally.

use super::codec::{CodecError, MAX_RESPONSE_BYTES, Request, Response};
use super::{EndpointConfig, OriginScopedCredential, SKILLRANKER_USER_AGENT};
use crate::privacy::{
    CredentialStatus, NetworkConsent, ProviderAdmissionRefusal, admit_provider_attempt,
};
use crate::runtime::EntryClock;
use asupersync::Cx;
use asupersync::http::h1::codec::HttpError;
use asupersync::http::h1::http_client::{ClientError, HttpClient};
use asupersync::tls::Certificate;
use std::fmt;
use std::future::{Future, poll_fn};
use std::task::Poll;
use std::time::Duration;

const MAX_ADDITIONAL_ROOTS: usize = 8;
const MAX_ROOT_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransportErrorKind {
    InvalidConfiguration,
    Admission(ProviderAdmissionRefusal),
    CredentialOriginMismatch,
    Request(CodecError),
    Response(CodecError),
    Cancelled,
    Deadline,
    Dns,
    Connect,
    Tls,
    Protocol,
    BodyTooLarge,
    UnsupportedEncoding,
    InvalidContentType,
    Redirect,
    HttpStatus(u16),
}

/// Once the HTTP client is entered, failure cannot establish zero provider cost.
/// `http_attempt_started` is deliberately not a claim that bytes reached Jev.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TransportError {
    pub kind: TransportErrorKind,
    pub http_attempt_started: bool,
}
impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // All fields are closed enums or a status integer, never library error
        // strings, response bodies, endpoint text, credentials or request state.
        write!(f, "Jev HTTPS attempt failed: {:?}", self.kind)
    }
}
impl std::error::Error for TransportError {}

pub struct JevClient {
    endpoint: EndpointConfig,
    http: HttpClient,
}
impl fmt::Debug for JevClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("JevClient(<private endpoint>)")
    }
}

impl JevClient {
    /// Uses the native trust roots selected by the pinned Cargo feature graph.
    /// The caller supplies an endpoint from trusted configuration.
    pub fn new(endpoint: EndpointConfig) -> Result<Self, TransportError> {
        Self::with_additional_roots(endpoint, Vec::new())
    }

    /// Additional explicit trust roots, for trusted private services or local
    /// TLS tests. These augment public roots; they never disable verification.
    /// Never obtain them from project configuration or transcript content.
    pub fn with_additional_roots(
        endpoint: EndpointConfig,
        roots: Vec<Certificate>,
    ) -> Result<Self, TransportError> {
        if !endpoint.origin().is_secure()
            || roots.len() > MAX_ADDITIONAL_ROOTS
            || roots
                .iter()
                .any(|root| root.as_der().len() > MAX_ROOT_BYTES)
        {
            return Err(failure(TransportErrorKind::InvalidConfiguration, false));
        }
        let mut builder = HttpClient::builder()
            .no_redirects()
            .no_retries()
            .no_proxy()
            .no_cookie_store()
            .max_connections_per_host(1)
            .max_total_connections(1)
            .max_body_size(MAX_RESPONSE_BYTES)
            .user_agent(SKILLRANKER_USER_AGENT);
        for root in roots {
            builder = builder.add_root_certificate(root);
        }
        Ok(Self {
            endpoint,
            http: builder.build(),
        })
    }

    /// Perform exactly one logical HTTP attempt. The caller must reserve its
    /// invocation-wide attempt allowance before calling and account for every
    /// error with `http_attempt_started`, including unknown returned usage.
    pub async fn send(
        &self,
        request: &Request,
        credential: Option<&OriginScopedCredential>,
        consent: NetworkConsent,
        cx: &Cx,
        clock: &EntryClock,
    ) -> Result<Response, TransportError> {
        admit_provider_attempt(
            consent,
            if credential.is_some() {
                CredentialStatus::PresentFromEnvironment
            } else {
                CredentialStatus::Absent
            },
        )
        .map_err(|e| failure(TransportErrorKind::Admission(e), false))?;
        budget(cx, clock, false)?;
        let credential = credential.ok_or_else(|| {
            failure(
                TransportErrorKind::Admission(ProviderAdmissionRefusal::MissingCredential),
                false,
            )
        })?;
        let header = credential
            .authorization_header_for(self.endpoint.origin())
            .map_err(|_| failure(TransportErrorKind::CredentialOriginMismatch, false))?;
        let bytes = request
            .to_json()
            .map_err(|e| failure(TransportErrorKind::Request(e), false))?;
        budget(cx, clock, false)?;
        let timeout = Duration::from_millis(clock.remaining_before_cleanup().as_millis());
        let exchange = self
            .http
            .post(self.endpoint.target_url().as_str())
            .header("Authorization", header)
            .header("Accept", "application/json")
            .header("Accept-Encoding", "identity")
            // No idle sockets survive a completed request. The exchange's own
            // connection is also dropped by Asupersync on deadline/cancellation.
            .header("Connection", "close")
            .content_type("application/json")
            .body(bytes)
            .timeout(timeout)
            .send(cx);
        let response = drive_exchange(exchange, cx, clock).await?;
        budget(cx, clock, true)?;
        if (300..400).contains(&response.status) {
            return Err(failure(TransportErrorKind::Redirect, true));
        }
        if !(200..300).contains(&response.status) {
            return Err(failure(
                TransportErrorKind::HttpStatus(response.status),
                true,
            ));
        }
        let mut encoding_seen = false;
        let mut content_type_seen = false;
        for (name, value) in &response.headers {
            if name.eq_ignore_ascii_case("content-encoding") {
                if encoding_seen || !value.trim().eq_ignore_ascii_case("identity") {
                    return Err(failure(TransportErrorKind::UnsupportedEncoding, true));
                }
                encoding_seen = true;
            }
            if name.eq_ignore_ascii_case("content-type") {
                if content_type_seen
                    || !value
                        .split(';')
                        .next()
                        .is_some_and(|v| v.trim().eq_ignore_ascii_case("application/json"))
                {
                    return Err(failure(TransportErrorKind::InvalidContentType, true));
                }
                content_type_seen = true;
            }
        }
        if !content_type_seen {
            return Err(failure(TransportErrorKind::InvalidContentType, true));
        }
        // No decompression is negotiated or performed. A compressed body is
        // rejected above, never inflated outside the decoded-body byte bound.
        let result = request
            .decode_response(&response.body)
            .map_err(|e| failure(TransportErrorKind::Response(e), true))?;
        budget(cx, clock, true)?;
        Ok(result)
    }
}

fn failure(kind: TransportErrorKind, http_attempt_started: bool) -> TransportError {
    TransportError {
        kind,
        http_attempt_started,
    }
}
fn budget(cx: &Cx, clock: &EntryClock, started: bool) -> Result<(), TransportError> {
    if cx.is_cancel_requested() {
        return Err(failure(TransportErrorKind::Cancelled, started));
    }
    clock
        .admit_new_work()
        .map_err(|_| failure(TransportErrorKind::Deadline, started))?;
    Ok(())
}
fn client_failure(error: ClientError) -> TransportError {
    let kind = match error {
        ClientError::Cancelled => TransportErrorKind::Cancelled,
        ClientError::DeadlineExceeded => TransportErrorKind::Deadline,
        ClientError::DnsError(_) => TransportErrorKind::Dns,
        ClientError::ConnectError(_) => TransportErrorKind::Connect,
        ClientError::TlsError(_) => TransportErrorKind::Tls,
        ClientError::HttpError(
            HttpError::BodyTooLarge | HttpError::BodyTooLargeDetailed { .. },
        ) => TransportErrorKind::BodyTooLarge,
        _ => TransportErrorKind::Protocol,
    };
    failure(kind, true)
}

/// The pinned HTTP client checks Cx at I/O boundaries, but a stalled body read
/// does not itself wake when this invocation's cancellation flag changes.
/// Keep one owned timer beside the same HTTP future so cancellation drops the
/// exchange promptly even when the peer sends no more bytes. No task is spawned
/// and no request is reconstructed or retried on a timer tick.
async fn drive_exchange(
    future: impl Future<Output = Result<asupersync::http::h1::types::Response, ClientError>>,
    cx: &Cx,
    clock: &EntryClock,
) -> Result<asupersync::http::h1::types::Response, TransportError> {
    let mut exchange = std::pin::pin!(future);
    loop {
        budget(cx, clock, true)?;
        let delay = Duration::from_millis(clock.remaining_before_cleanup().as_millis().min(10));
        let mut tick = std::pin::pin!(asupersync::time::sleep(asupersync::time::wall_now(), delay));
        let step = poll_fn(|task| {
            if let Err(error) = budget(cx, clock, true) {
                return Poll::Ready(Err(error));
            }
            if let Poll::Ready(result) = exchange.as_mut().poll(task) {
                return Poll::Ready(Ok(Some(result)));
            }
            match tick.as_mut().poll(task) {
                Poll::Ready(()) => Poll::Ready(Ok(None)),
                Poll::Pending => Poll::Pending,
            }
        })
        .await?;
        if let Some(result) = step {
            return result.map_err(client_failure);
        }
    }
}
