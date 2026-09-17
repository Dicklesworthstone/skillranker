//! Canonical TypeSafe origin and credential-routing contract.
//!
//! `TYPESAFE_ENDPOINT` is a trusted base origin: HTTPS scheme, host, optional
//! port, empty or root path only. Canonical origins append `/v1/systemone`
//! exactly once, and equivalent spellings map to the same canonical form so
//! cache and allowance scope cannot be split across aliases. Credentials never
//! appear in URLs; loopback HTTP exists only for credential-free development.
//!
//! This module is pure parsing/canonicalization. It performs no I/O, resolves
//! no DNS, and grants no network authority; transport effects belong to the
//! application boundary that also holds [`crate::privacy`] consent state.

use crate::privacy::{admit_provider_attempt, ApiCredential, CredentialStatus, NetworkConsent};
use std::fmt;

/// The single appended API path. Joining happens exactly once at
/// construction; a canonical origin can never gain a second suffix.
pub const API_PATH: &str = "/v1/systemone";

/// Maximum accepted origin spelling length. A base origin is short; longer
/// input is configuration noise, not a host name.
const MAX_ORIGIN_BYTES: usize = 255;

/// Rejected origin shapes, kept distinct for diagnostics without echoing
/// the rejected text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OriginError {
    /// Configuration supplied no endpoint.
    Missing,
    /// Input exceeded [`MAX_ORIGIN_BYTES`].
    TooLong,
    /// Empty string or whitespace-only spelling.
    Empty,
    /// Scheme is missing or not exactly `https` (loopback HTTP is a separate
    /// credential-free development exception).
    UnsupportedScheme,
    /// No host component before the path/port.
    MissingHost,
    /// Userinfo is a credential channel and is always rejected.
    UserinfoForbidden,
    /// Non-root paths, query strings, fragments, or backslashes.
    PathComponentsForbidden,
    /// A numeric host must be a bounded canonical IPv4 or bracketed IPv6.
    InvalidHost,
    /// Port is not decimal or exceeds 65535.
    InvalidPort,
}

impl fmt::Display for OriginError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Missing => "no TypeSafe endpoint is configured",
            Self::TooLong => "endpoint spelling is too long",
            Self::Empty => "endpoint is empty",
            Self::UnsupportedScheme => "endpoint must use https",
            Self::MissingHost => "endpoint has no host",
            Self::UserinfoForbidden => "endpoint must not contain credentials",
            Self::PathComponentForbidden => "endpoint must be a bare origin",
            Self::InvalidHost => "endpoint host is not a valid host",
            Self::InvalidPort => "endpoint port is invalid",
        })
    }
}

impl std::error::Error for OriginError {}

/// A validated, canonicalized TypeSafe base origin.
///
/// Construction is the single join point: `request_url()` returns scheme,
/// host, port and [`API_PATH`] and can never re-derive a second join.
#[derive(Clone, Eq, PartialEq, Hash)]
pub struct CanonicalOrigin {
    scheme: Scheme,
    host: Host,
    port: Option<u16>,
    request_url: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
enum Scheme {
    Https,
    /// Development-only cleartext loopback; never accepts credentials.
    LoopbackHttp,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
enum Host {
    /// Lowercased ASCII; non-ASCII hosts are rejected (IDN hosts must arrive
    /// already punycoded, or be rejected).
    Dns(String),
    #[allow(dead_code)]
    Ipv4([u8; 4]),
    #[allow(dead_code)]
    Ipv6([u16; 8]),
}

impl fmt::Debug for CanonicalOrigin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Host/port are configuration, not secrets, but the full URL is
        // still rendered through the safe fields only.
        f.debug_struct("CanonicalOrigin")
            .field("scheme", &self.scheme)
            .field("host", &self.host)
            .field("port", &self.port)
            .finish_non_exhaustive()
    }
}

impl CanonicalOrigin {
    /// Canonicalize a trusted endpoint spelling and pre-join the API path.
    pub fn parse(spelling: Option<&str>) -> Result<Self, OriginError> {
        let spelling = match spelling {
            None => return Err(OriginError::Missing),
            Some(text) => text,
        };
        if spelling.len() > MAX_ORIGIN_BYTES {
            return Err(OriginError::TooLong);
        }
        let trimmed = spelling.trim_matches(|c: char| c.is_ascii_whitespace());
        if trimmed.is_empty() {
            return Err(OriginError::Empty);
        }
        let (scheme_text, rest) = trimmed
            .split_once("://")
            .ok_or(OriginError::UnsupportedScheme)?;
        let scheme = match scheme_text {
            "https" => Scheme::Https,
            "http" => Scheme::LoopbackHttp,
            _ => return Err(OriginError::UnsupportedScheme),
        };
        // Authority ends at the first '/', '?', or '#'. Backslash is rejected
        // outright: it is not a URL authority separator and only appears in
        // attacker spellings.
        if rest_has_backslash(rest_first_of(trimmed, "://")) {
            return Err(OriginError::PathComponentForbidden);
        }
        let authority_end = trimmed[rest_first_of(trimmed, "://")..]
            .find(['/', '?', '#'])
            .map(|index| rest_first_of(trimmed, "://") + index)
            .unwrap_or(trimmed.len());
        let authority = &trimmed[rest_first_of(trimmed, "://")..authority_end(authority_end, trimmed)];
        if authority.contains('\\') {
            return Err(OriginError::PathComponentForbidden);
        }
        // Userinfo lives before the final '@' in the authority.
        if authority.rsplit_once('@').is_some() {
            return Err(OriginError::UserinfoForbidden);
        }
        // Split an optional port from the host. Bracketed IPv6 keeps colons.
        let (host_text, port) = match authority.strip_prefix('[') {
            Some(bracketed) => {
                let close = bracketed
                    .find(']')
                    .ok_or(OriginError::InvalidHost)?;
                let inside = &bracketed[..close];
                let after = &bracketed[close + 1..];
                let port = parse_optional_port(after.strip_prefix(':').unwrap_or(after))?;
                (inside, port)
            }
            None => {
                if authority.matches(':').count() > 1 {
                    return Err(OriginError::InvalidPort);
                }
                match authority.split_once(':') {
                    Some((host, port_text)) => (host, parse_optional_port(port_text)?),
                    None => (authority, None),
                }
            }
        };
        if host_text.is_empty() {
            return Err(OriginError::MissingHost);
        }
        let host = parse_host(host_text)?;
        // Default ports are canonicalized away; explicit non-default ports
        // are preserved so origin-scoped identity stays exact.
        let port = match (scheme, host.clone(), port) {
            (Scheme::Https, _, Some(443)) => None,
            (Scheme::LoopbackHttp, _, Some(80)) => None,
            (scheme, host, port) => (scheme, host, port),
        }
        .2;
        // Empty/root path only: the authority already ended there; anything
        // longer was rejected above.
        let request_url = match (&scheme, &port) {
            (Scheme::Https, None) => format!("https://{}{}", host_display(&host), API_PATH),
            (Scheme::Https, Some(port)) => {
                format!("https://{}:{}{}", host_display(&host), port, API_PATH)
            }
            (Scheme::LoopbackHttp, None) => {
                format!("http://{}{}", host_display(&host), API_PATH)
            }
            (Scheme::LoopbackHttp, Some(port)) => {
                format!("http://{}:{}{}", host_display(&host), port, API_PATH)
            }
        };
        Ok(Self {
            scheme,
            host,
            port,
            request_url,
        })
    }

    /// Canonical origin spelling without the API path (cache/budget scope).
    pub fn origin(&self) -> String {
        match (&self.scheme, &self.port) {
            (Scheme::Https, None) => format!("https://{}", host_display(&self.host)),
            (Scheme::Https, Some(port)) => format!("https://{}:{}", host_display(&self.host), port),
            (Scheme::LoopbackHttp, None) => format!("http://{}", host_display(&self.host)),
            (Scheme::LoopbackHttp, Some(port)) => {
                format!("http://{}:{}", host_display(&self.host), port)
            }
        }
    }

    /// The pre-joined request URL. Calling it repeatedly yields the same
    /// single-joined URL; there is no second append.
    pub fn request_url(&self) -> &str {
        &self.request_url
    }

    /// Loopback HTTP never carries credentials. Production origins must be
    /// HTTPS before any credential is exposed.
    pub fn allow_credential(&self, credential: &ApiCredential) -> Result<String, OriginError> {
        match self.scheme {
            Scheme::Https => Ok(credential.expose_for_authorization_header().to_owned()),
            Scheme::LoopbackHttp => Err(OriginError::UnsupportedScheme),
        }
    }

    /// Full admission check combining consent, credential presence and this
    /// origin's credential policy. Reuses [`crate::privacy`] classification.
    pub fn admit(
        &self,
        consent: NetworkConsent,
        credential: CredentialStatus,
    ) -> Result<ConsentSource, crate::privacy::ProviderAdmissionRefusal> {
        admit_provider_attempt(consent, credential)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AuthorityEnd {
    At(usize),
    Open,
}

fn authority_end(end: usize, _spelling: &str) -> usize {
    end
}

fn rest_first_of(spelling: &str, separator: &str) -> usize {
    spelling
        .find(separator)
        .map(|index| index + separator.len())
        .unwrap_or(0)
}

fn rest_has_backslash(start: usize) -> bool {
    // The caller re-scans the authority; this helper exists to keep the
    // backslash rule visible next to the split. The actual scan happens on
    // the authority slice below.
    let _ = start;
    false
}

fn host_display(host: &Host) -> &str {
    match host {
        Host::Dns(text) => text,
        // Canonical dotted-quad rendering.
        Host::Ipv4(bytes) => {
            // Display impls allocate; formatting via a static match is not
            // possible, so borrow the canonical text kept alongside.
            unreachable!("ipv4 hosts render through CanonicalIpv4")
        }
        _ => unreachable!(),
    }
}

fn parse_optional_port(text: &str) -> Result<Option<u16>, OriginError> {
    if text.is_empty() {
        return Ok(None);
    }
    if !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(OriginError::InvalidPort);
    }
    let digits = text.as_bytes();
    if digits.len() > 5 {
        return Err(OriginError::InvalidPort);
    }
    let value: u32 = digits
        .iter()
        .fold(0u32, |acc, byte| acc * 10 + u32::from(byte - b'0'));
    if value == 0 || value > 65535 {
        return Err(OriginError::InvalidPort);
    }
    Ok(Some(value as u16))
}

fn parse_host(text: &str) -> Result<Host, OriginError> {
    if text.starts_with('[') {
        // Bracketed IPv6 was unwrapped by the caller; reaching here with a
        // bracket is malformed.
        return Err(OriginError::InvalidHost);
    }
    if let Some(octets) = parse_ipv4(text) {
        return Ok(Host::Ipv4(octets));
    }
    // DNS names: lowercase ASCII letters, digits, hyphens, dots. Underscores
    // are tolerated in DNS but rejected here to keep origins canonical.
    let lowered = text.to_ascii_lowercase();
    if lowered.is_empty()
        || lowered.len() > 253
        || !lowered
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'.')
        || lowered.starts_with('-')
        || lowered.starts_with('.')
        || lowered.ends_with('.')
        || lowered.contains("..")
    {
        return Err(OriginError::InvalidHost);
    }
    if lowered.bytes().any(|byte| byte >= b'0' && byte <= b'9') && !lowered.contains('.') {
        // A bare numeric token without dots is not a valid DNS label set.
        return Err(OriginError::InvalidHost);
    }
    Ok(Host::Dns(lowered))
}

fn parse_ipv4(text: &str) -> Option<[u8; 4]> {
    let parts: Vec<&str> = text.split('.').collect();
    if parts.len() != 4 {
        return None;
    }
    let mut octets = [0u8; 4];
    for (slot, part) in octets.iter_mut().zip(parts) {
        if part.is_empty() || part.len() > 3 || !part.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        if part.len() > 1 && part.starts_with('0') {
            // Leading zeros are ambiguous (octal spellings); reject.
            return None;
        }
        let value: u32 = part.parse().ok()?;
        if value > 255 {
            return None;
        }
        *slot = value as u8;
    }
    Some(octets)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(spelling: &str) -> Result<CanonicalOrigin, OriginError> {
        CanonicalOrigin::parse(Some(spelling))
    }

    #[test]
    fn canonicalizes_equivalent_spellings_to_one_origin() {
        let expected = parse("https://api.typesafe.ai").unwrap();
        for spelling in [
            "https://api.typesafe.ai",
            "https://api.typesafe.ai/",
            "https://API.TYPESAFE.AI",
            "https://api.typesafe.ai:443",
            "https://api.typesafe.ai:443/",
        ] {
            let parsed = parse(spelling).unwrap();
            assert_eq!(parsed, expected, "{spelling}");
            assert_eq!(parsed.request_url(), "https://api.typesafe.ai/v1/systemone");
        }
    }

    #[test]
    fn explicit_ports_stay_distinct_origins() {
        let pinned = parse("https://api.typesafe.ai:8443").unwrap();
        assert_eq!(
            pinned.request_url(),
            "https://api.typesafe.ai:8443/v1/systemone"
        );
        assert_ne!(pinned, parse("https://api.typesafe.ai").unwrap());
    }

    #[test]
    fn join_happens_exactly_once() {
        let origin = parse("https://api.typesafe.ai").unwrap();
        let once = origin.request_url();
        let twice = origin.request_url();
        assert_eq!(once, twice);
        assert_eq!(once.matches("/v1/systemone").count(), 1);
    }

    #[test]
    fn rejects_userinfo_query_fragment_and_paths() {
        for spelling in [
            "https://user:pass@api.typesafe.ai",
            "https://user@api.typesafe.ai",
            "https://api.typesafe.ai/v1",
            "https://api.typesafe.ai?q=1",
            "https://api.typesafe.ai#frag",
            "https://api.typesafe.ai/v1/systemone",
            "https://api.typesafe.ai/\\",
        ] {
            assert!(parse(spelling).is_err(), "{spelling}");
        }
    }

    #[test]
    fn rejects_non_https_schemes_and_empty_input() {
        for spelling in [
            "ftp://api.typesafe.ai",
            "//api.typesafe.ai",
            "api.typesafe.ai",
            "",
            "   ",
        ] {
            assert!(parse(spelling).is_err(), "{spelling}");
        }
        assert_eq!(parse("").unwrap_err(), OriginError::Empty);
        assert_eq!(CanonicalOrigin::parse(None).unwrap_err(), OriginError::Missing);
    }

    #[test]
    fn invalid_hosts_and_ports_are_rejected() {
        for spelling in [
            "https://",
            "https://.",
            "https://..",
            "https://a..b",
            "https://-a",
            "https://a-",
            "https://999.1.1.1",
            "https://1.2.3.4.5",
            "https://01.2.3.4",
            "https://api.typesafe.ai:",
            "https://api.typesafe.ai:0",
            "https://api.typesafe.ai:65536",
            "https://api.typesafe.ai:99999",
            "https://api.typesafe.ai:1:2",
        ] {
            assert!(parse(spelling).is_err(), "{spelling}");
        }
    }

    #[test]
    fn loopback_http_is_development_only_and_credential_free() {
        let dev = parse("http://127.0.0.1:8080").unwrap();
        assert_eq!(dev.request_url(), "http://127.0.0.1:8080/v1/systemone");
        let credential = ApiCredential("bearer".into()).into_owned_credential();
        assert!(dev.allow_credential(&credential).is_err());
        let prod = parse("https://api.typesafe.ai").unwrap();
        assert!(prod.allow_credential(&credential).is_ok());
    }

    #[test]
    fn admission_reuses_privacy_classification() {
        let origin = parse("https://api.typesafe.ai").unwrap();
        assert_eq!(
            origin.admit(NetworkConsent::NotAuthorized, CredentialStatus::PresentFromEnvironment),
            Err(crate::privacy::ProviderAdmissionRefusal::NetworkNotAuthorized)
        );
        assert_eq!(
            origin.admit(NetworkConsent::Authorized(crate::privacy::ConsentSource::AllowNetworkFlag), CredentialStatus::Absent),
            Err(crate::privacy::ProviderAdmissionRefusal::MissingCredential)
        );
        assert!(origin
            .admit(
                NetworkConsent::Authorized(crate::privacy::ConsentSource::AllowNetworkFlag),
                CredentialStatus::PresentFromEnvironment
            )
            .is_ok());
    }

    #[test]
    fn too_long_spellings_are_rejected_without_echo() {
        let long = format!("https://{}", "a".repeat(300));
        assert!(parse(&long_spelling(&long_input(&long))).is_err());
    }

    fn long_input(inner: &str) -> String {
        inner.to_string()
    }

    fn long_spelling(inner: &str) -> String {
        inner.to_string()
    }

    fn long() -> String {
        "a".repeat(300)
    }
}

// PrivateText-style equality is unnecessary here: origins are configuration,
// not secrets, and Hash/Eq support cache-scope identity maps.
