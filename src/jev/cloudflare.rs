//! Cloudflare AI transport through its native Jev endpoint.
//!
//! The ranking pipeline still speaks the validated Jev request/response
//! contract. This adapter maps it to Cloudflare's `/ai/run` envelope, then
//! runs the returned Jev result through the existing decoder before it can
//! affect ranking or enter the cache.

use super::SKILLRANKER_USER_AGENT;
use super::client::{
    TransportError, TransportErrorKind, TransportFuture, budget, drive_exchange, failure,
};
use super::codec::{CodecError, MAX_RESPONSE_BYTES, Request, Response};
use super::endpoint::{CanonicalOrigin, EndpointConfig, OriginScopedCredential};
use super::retry::RetryAfter;
use crate::privacy::{
    CredentialStatus, NetworkConsent, ProviderAdmissionRefusal, admit_provider_attempt,
};
use crate::runtime::EntryClock;
use asupersync::Cx;
use asupersync::http::h1::http_client::HttpClient;
use serde_json::{Value, json};
use std::fmt;
use std::time::Duration;

pub const CLOUDFLARE_API_ORIGIN: &str = "https://api.cloudflare.com";
const CLOUDFLARE_PATH_PREFIX: &str = "/client/v4/accounts/";

/// Build the fixed Cloudflare AI target for one validated account ID.
pub(crate) fn endpoint(account_id: &str) -> Result<EndpointConfig, TransportError> {
    if account_id.len() != 32 || !account_id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(failure(TransportErrorKind::InvalidConfiguration, false));
    }
    let path = format!("{CLOUDFLARE_PATH_PREFIX}{account_id}/ai/run");
    EndpointConfig::from_origin_and_path_str(CLOUDFLARE_API_ORIGIN, &path)
        .map_err(|_| failure(TransportErrorKind::InvalidConfiguration, false))
}

pub struct CloudflareClient {
    endpoint: EndpointConfig,
    http: HttpClient,
}

impl fmt::Debug for CloudflareClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CloudflareClient(<private endpoint>)")
    }
}

impl CloudflareClient {
    pub fn new(account_id: &str) -> Result<Self, TransportError> {
        let endpoint = endpoint(account_id)?;
        let http = HttpClient::builder()
            .no_redirects()
            .no_retries()
            .no_proxy()
            .no_cookie_store()
            .max_connections_per_host(1)
            .max_total_connections(1)
            .max_body_size(MAX_RESPONSE_BYTES)
            .user_agent(SKILLRANKER_USER_AGENT)
            .build();
        Ok(Self { endpoint, http })
    }

    pub fn origin(&self) -> &CanonicalOrigin {
        self.endpoint.origin()
    }

    pub async fn send(
        &self,
        request: &Request,
        credential: Option<&OriginScopedCredential>,
        consent: NetworkConsent,
        cx: &Cx,
        clock: &EntryClock,
    ) -> Result<Response, TransportError> {
        self.send_accounted(request, credential, consent, cx, clock, || Ok(()))
            .await
    }

    async fn send_accounted(
        &self,
        request: &Request,
        credential: Option<&OriginScopedCredential>,
        consent: NetworkConsent,
        cx: &Cx,
        clock: &EntryClock,
        on_start: impl FnOnce() -> Result<(), TransportError>,
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
        let authorization = credential
            .authorization_header_for(self.endpoint.origin())
            .map_err(|_| failure(TransportErrorKind::CredentialOriginMismatch, false))?;
        let body =
            request_body(request).map_err(|e| failure(TransportErrorKind::Request(e), false))?;
        if body.len() > super::codec::MAX_REQUEST_BYTES {
            return Err(failure(
                TransportErrorKind::Request(CodecError::TooLarge),
                false,
            ));
        }
        budget(cx, clock, false)?;
        let timeout = Duration::from_millis(clock.remaining_before_cleanup().as_millis());
        let exchange = self
            .http
            .post(self.endpoint.target_url().as_str())
            .header("Authorization", authorization)
            .header("Accept", "application/json")
            .header("Accept-Encoding", "identity")
            .header("Connection", "close")
            .content_type("application/json")
            .body(body)
            .timeout(timeout)
            .send(cx);
        on_start()?;
        let response = drive_exchange(exchange, cx, clock).await?;
        budget(cx, clock, true)?;
        if (300..400).contains(&response.status) {
            return Err(failure(TransportErrorKind::Redirect, true));
        }
        if !(200..300).contains(&response.status) {
            let mut error = failure(TransportErrorKind::HttpStatus(response.status), true);
            error.retry_after =
                RetryAfter::from_headers(&response.headers, std::time::SystemTime::now());
            return Err(error);
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
        let decoded = decode_response(request, &response.body)
            .map_err(|e| failure(TransportErrorKind::Response(e), true))?;
        budget(cx, clock, true)?;
        Ok(decoded)
    }
}

impl super::client::JevTransport for CloudflareClient {
    fn send<'a>(
        &'a self,
        request: &'a Request,
        credential: Option<&'a OriginScopedCredential>,
        consent: NetworkConsent,
        cx: &'a Cx,
        clock: &'a EntryClock,
    ) -> TransportFuture<'a> {
        Box::pin(CloudflareClient::send(
            self, request, credential, consent, cx, clock,
        ))
    }

    fn send_accounted<'a>(
        &'a self,
        request: &'a Request,
        credential: Option<&'a OriginScopedCredential>,
        consent: NetworkConsent,
        cx: &'a Cx,
        clock: &'a EntryClock,
        on_start: &'a mut (dyn FnMut() -> Result<(), TransportError> + Send),
    ) -> TransportFuture<'a> {
        Box::pin(async move {
            let mut start = || on_start();
            self.send_accounted(request, credential, consent, cx, clock, &mut start)
                .await
        })
    }

    fn origin(&self) -> Option<&CanonicalOrigin> {
        Some(self.origin())
    }
}

fn decode_response(request: &Request, body: &[u8]) -> Result<Response, CodecError> {
    let envelope: Value = serde_json::from_slice(body).map_err(|_| CodecError::InvalidJson)?;
    if envelope.get("success").and_then(Value::as_bool) == Some(false) {
        return Err(CodecError::InvalidAnswer);
    }
    let result = envelope
        .pointer("/result/result")
        .or_else(|| {
            envelope
                .get("result")
                .filter(|value| value.get("answers").is_some())
        })
        .or_else(|| envelope.get("answers").map(|_| &envelope))
        .ok_or(CodecError::InvalidAnswer)?;
    let model = result
        .get("model")
        .and_then(Value::as_str)
        .filter(|model| !model.is_empty())
        .unwrap_or(request.model());
    let answers = result.get("answers").ok_or(CodecError::InvalidAnswer)?;
    let usage = result.get("usage").ok_or(CodecError::InvalidAnswer)?;
    let input_tokens = usage
        .get("input_tokens")
        .and_then(Value::as_u64)
        .ok_or(CodecError::InvalidAnswer)?;
    let output_tokens = usage
        .get("output_tokens")
        .and_then(Value::as_u64)
        .ok_or(CodecError::InvalidAnswer)?;
    let wire = json!({
        "model": model,
        "answers": answers,
        "usage": {"input_tokens": input_tokens, "output_tokens": output_tokens},
    });
    let wire = serde_json::to_vec(&wire).map_err(|_| CodecError::InvalidJson)?;
    request.decode_response(&wire)
}

fn request_body(request: &Request) -> Result<Vec<u8>, CodecError> {
    let request_json = request.to_json()?;
    let request_value: Value =
        serde_json::from_slice(&request_json).map_err(|_| CodecError::InvalidJson)?;
    let state = request_value
        .get("state")
        .cloned()
        .ok_or(CodecError::InvalidRequest)?;
    let questions = request_value
        .get("questions")
        .cloned()
        .ok_or(CodecError::InvalidRequest)?;
    serde_json::to_vec(&json!({
        "model": request.model(),
        "input": {
            "state": state,
            "questions": questions,
        },
    }))
    .map_err(|_| CodecError::TooLarge)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jev::codec::{Answer, Question};

    #[test]
    fn cloudflare_content_is_translated_and_validated_as_jev() {
        let request = Request::new(
            "typesafe/jev".into(),
            json!("synthetic cloudflare adapter probe"),
            [(
                "fit".into(),
                Question::Noul {
                    instructions: json!("Return a bounded synthetic score."),
                    criteria: None,
                },
            )],
        )
        .unwrap();
        let body = serde_json::to_vec(&json!({
            "success": true,
            "result": {
                "gatewayMetadata": {},
                "result": {
                    "model": "jev-1.13.0",
                    "answers": {"fit": {"type": "noul", "noul": 0.75}},
                    "usage": {"input_tokens": 12, "output_tokens": 7}
                },
                "state": "synthetic cloudflare adapter probe"
            }
        }))
        .unwrap();
        let response = decode_response(&request, &body).unwrap();
        assert!(matches!(response.answers["fit"], Answer::Noul(value) if value == 0.75));
        assert_eq!(response.usage.input_tokens, 12);
        assert_eq!(response.usage.output_tokens, 7);
        assert_eq!(response.returned_model, "jev-1.13.0");
    }

    #[test]
    fn request_uses_cloudflare_native_jev_envelope() {
        let request = Request::new(
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
        .unwrap();
        let body: Value = serde_json::from_slice(&request_body(&request).unwrap()).unwrap();
        assert_eq!(body["model"], "typesafe/jev");
        assert_eq!(body["input"]["state"], "synthetic state");
        assert_eq!(body["input"]["questions"]["fit"]["type"], "noul");
    }

    #[test]
    fn missing_usage_is_rejected_instead_of_fabricating_zero_tokens() {
        let request = Request::new(
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
        .unwrap();
        let body = serde_json::json!({
            "success": true,
            "result": {
                "result": {
                    "model": "jev-1.13.0",
                    "answers": {"fit": {"type": "noul", "noul": 0.75}}
                }
            }
        });
        assert!(matches!(
            decode_response(&request, body.to_string().as_bytes()),
            Err(CodecError::InvalidAnswer)
        ));
    }

    #[test]
    fn endpoint_contains_only_the_validated_account_in_the_target_path() {
        let endpoint = endpoint("0123456789abcdef0123456789abcdef").unwrap();
        assert_eq!(endpoint.origin().as_str(), CLOUDFLARE_API_ORIGIN);
        assert_eq!(
            endpoint.target_url().as_str(),
            "https://api.cloudflare.com/client/v4/accounts/0123456789abcdef0123456789abcdef/ai/run"
        );
    }
}
