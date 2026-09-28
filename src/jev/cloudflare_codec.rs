//! Bounded native wire encoding and validation for Cloudflare-hosted TypeSafe Jev.
//!
//! This codec does not select a provider or authorize a network request. A
//! transport must still enforce HTTP policy and check cancellation/deadline
//! after decoding, before returning a successful accounted response.

use super::codec::{CodecError, MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES, Request, Response};
use crate::output::{JsonSeed, check_depth};
use serde::de::DeserializeSeed;
use serde_json::{Value, json};

/// The documented Cloudflare route for TypeSafe's structured evaluation model.
/// Do not silently route a TypeSafe alias or a different inference model.
pub const CLOUDFLARE_JEV_MODEL: &str = "typesafe/jev";

/// Encode the native `{model,input:{state,questions}}` request. The final byte
/// limit includes the native wrapper, not just the inner TypeSafe document.
/// Authorization and redaction remain the caller's responsibility.
pub fn encode_request(request: &Request) -> Result<Vec<u8>, CodecError> {
    if request.model() != CLOUDFLARE_JEV_MODEL {
        return Err(CodecError::InvalidRequest);
    }
    // Validate and bound even a Request constructed via serde before allocating
    // another value tree. This JSON is locally generated, not provider output.
    let validated = request.to_json()?;
    let mut input: Value =
        serde_json::from_slice(&validated).map_err(|_| CodecError::InvalidRequest)?;
    let model = input
        .as_object_mut()
        .and_then(|object| object.remove("model"))
        .ok_or(CodecError::InvalidRequest)?;
    let native = json!({"model": model, "input": input});
    // The wrapper also adds nesting. A valid inner request at the depth limit
    // must not exceed the same bound when transmitted in its native envelope.
    check_depth(&native, 0).map_err(|_| CodecError::InvalidJson)?;
    let body = serde_json::to_vec(&native).map_err(|_| CodecError::TooLarge)?;
    // The intermediate serialization is bounded by the validated request plus
    // the fixed wrapper. An over-limit request must fail before attempt debit.
    if body.len() > MAX_REQUEST_BYTES {
        return Err(CodecError::TooLarge);
    }
    Ok(body)
}

/// Translate one bounded native envelope into the common validated Jev result.
///
/// Validate the original bytes before extracting the inner result: parsing
/// straight into `Value` first would erase duplicate definitions, and checking
/// only the extracted result would overlook oversized or deeply nested metadata.
/// Accept the native `result.result`, flat `result`, and top-level Jev shapes,
/// but never choose between competing result locations. Model identity and usage
/// must come from the provider; the request alias and zero tokens are not defaults.
pub fn decode_response(request: &Request, body: &[u8]) -> Result<Response, CodecError> {
    if body.len() > MAX_RESPONSE_BYTES {
        return Err(CodecError::TooLarge);
    }
    let mut decoder = serde_json::Deserializer::from_slice(body);
    let envelope = JsonSeed(0)
        .deserialize(&mut decoder)
        .map_err(|_| CodecError::InvalidJson)?;
    decoder.end().map_err(|_| CodecError::InvalidJson)?;
    if envelope.get("success").and_then(Value::as_bool) != Some(true) {
        return Err(CodecError::InvalidAnswer);
    }
    let result = result_object(&envelope)?;
    // Preserve the entire Jev result. The common decoder enforces model identity,
    // exact question/option bindings, answer types, probabilities and u64 usage.
    // Do not rebuild selected fields with fallback values or lossy conversions.
    let wire = serde_json::to_vec(result).map_err(|_| CodecError::InvalidJson)?;
    request.decode_response(&wire)
}

fn result_object(envelope: &Value) -> Result<&Value, CodecError> {
    if envelope.get("answers").is_some() {
        if envelope.get("result").is_some() {
            return Err(CodecError::InvalidAnswer);
        }
        return Ok(envelope);
    }
    let outer = envelope
        .get("result")
        .filter(|value| value.is_object())
        .ok_or(CodecError::InvalidAnswer)?;
    match outer.get("result") {
        Some(inner) => {
            if outer.get("answers").is_some() || !inner.is_object() {
                return Err(CodecError::InvalidAnswer);
            }
            Ok(inner)
        }
        None => Ok(outer),
    }
}
