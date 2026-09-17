//! TypeSafe endpoint canonicalization, origin-scoped credential routing,
//! redirect prohibition, and proxy safety policies.
//!
//! # Embedded Invariants
//! - `TYPESAFE_ENDPOINT` is a trusted **base origin**, not a full path.
//! - Scheme must be HTTPS, with a test-only loopback HTTP exception.
//! - Equivalent spellings (trailing slash, default ports, case, IDN) normalize to
//!   the exact same canonical origin identity string for cache and budget sharing.
//! - Userinfo, query strings, fragments, and non-root paths are rejected.
//! - Append `/v1/systemone` exactly once to form the target URL.
//! - Loopback HTTP is credential-free; routing an `ApiCredential` over insecure HTTP is forbidden.
//! - `OriginScopedCredential` binds credentials to a specific canonical origin;
//!   mismatched origins refuse to emit the `Authorization` header.
//! - No redirects: 3xx responses are hard failures; redirects are never followed.
//! - Ambient proxy variables (`HTTP_PROXY`, etc.) are not implicitly trusted,
//!   and proxy diagnostics scrub credentials.

use crate::output::ErrorKind;
use crate::privacy::ApiCredential;
use std::fmt;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::str::FromStr;

/// Documented production base origin for TypeSafe.ai Jev evaluations.
pub const DEFAULT_TYPESAFE_ENDPOINT: &str = "https://api.typesafe.ai";

/// The one-time joined API path for Jev SystemOne evaluations.
pub const SYSTEMONE_PATH: &str = "/v1/systemone";

/// User agent header value for SkillRanker requests to TypeSafe.
pub const SKILLRANKER_USER_AGENT: &str = concat!("skillranker/", env!("CARGO_PKG_VERSION"));

/// Maximum allowed bytes for raw endpoint input.
pub const MAX_ENDPOINT_INPUT_BYTES: usize = 2048;

/// Known ambient proxy environment variable names to inspect and scrub.
pub const AMBIENT_PROXY_VARS: &[&str] = &[
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "NO_PROXY",
    "no_proxy",
];

/// Supported URL schemes.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum Scheme {
    Https,
    Http,
}

impl Scheme {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Https => "https",
            Self::Http => "http",
        }
    }

    pub const fn default_port(self) -> u16 {
        match self {
            Self::Https => 443,
            Self::Http => 80,
        }
    }
}

impl fmt::Display for Scheme {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A parsed and canonicalized base origin for TypeSafe Jev requests.
///
/// Canonicalization guarantees that all equivalent spellings share the exact same
/// string representation, which serves as the cache identity and budget scope key.
#[derive(Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct CanonicalOrigin {
    scheme: Scheme,
    host: String,
    port: Option<u16>,
    canonical: String,
}

impl CanonicalOrigin {
    /// Parse and canonicalize a base origin string.
    pub fn parse(input: &str) -> Result<Self, EndpointError> {
        if input.is_empty() {
            return Err(EndpointError::EmptyInput);
        }
        if input.len() > MAX_ENDPOINT_INPUT_BYTES {
            return Err(EndpointError::InputTooLong(input.len()));
        }

        // Check for disallowed URL features before parsing
        // Userinfo check (@ before / or end)
        if let Some(at_idx) = input.find('@') {
            let scheme_end = input.find("://").map(|i| i + 3).unwrap_or(0);
            let slash_idx = input[scheme_end..].find('/').map(|i| i + scheme_end);
            if slash_idx.is_none() || at_idx < slash_idx.unwrap() {
                return Err(EndpointError::UserinfoForbidden);
            }
        }

        // Query string check
        if input.contains('?') {
            return Err(EndpointError::QueryForbidden);
        }

        // Fragment check
        if input.contains('#') {
            return Err(EndpointError::FragmentForbidden);
        }

        // Scheme extraction
        let (scheme, rest) = if let Some(colon_slash) = input.find("://") {
            let scheme_raw = &input[..colon_slash];
            let after_scheme = &input[colon_slash + 3..];
            if scheme_raw.eq_ignore_ascii_case("https") {
                (Scheme::Https, after_scheme)
            } else if scheme_raw.eq_ignore_ascii_case("http") {
                (Scheme::Http, after_scheme)
            } else {
                return Err(EndpointError::UnsupportedScheme(
                    scheme_raw.to_ascii_lowercase(),
                ));
            }
        } else {
            return Err(EndpointError::MissingScheme);
        };

        // Split authority (host + optional port) and path
        let (authority, path) = match rest.find('/') {
            Some(idx) => (&rest[..idx], &rest[idx..]),
            None => (rest, ""),
        };

        // Validate path: must be empty or "/"
        if !path.is_empty() && path != "/" {
            if path == "/v1/systemone"
                || path == "/v1/systemone/"
                || path == "/v1"
                || path == "/v1/"
            {
                return Err(EndpointError::DuplicatePathJoin {
                    path: path.to_owned(),
                });
            }
            return Err(EndpointError::NonRootPathForbidden {
                path: path.to_owned(),
            });
        }

        if authority.is_empty() {
            return Err(EndpointError::EmptyHost);
        }

        // Parse host and port
        let (raw_host, explicit_port) = parse_authority(authority)?;

        // Canonicalize host
        let (canonical_host, is_loopback) = canonicalize_host(raw_host)?;

        // Scheme check: HTTP is permitted ONLY for loopback hosts
        if scheme == Scheme::Http && !is_loopback {
            return Err(EndpointError::InsecureScheme {
                scheme: "http".to_owned(),
                host: canonical_host,
            });
        }

        // Canonicalize port: omit default port
        let canonical_port = match explicit_port {
            Some(p) if p == scheme.default_port() => None,
            other => other,
        };

        // Construct canonical string
        let canonical = match canonical_port {
            Some(p) => format!("{}://{}:{}", scheme.as_str(), canonical_host, p),
            None => format!("{}://{}", scheme.as_str(), canonical_host),
        };

        Ok(Self {
            scheme,
            host: canonical_host,
            port: canonical_port,
            canonical,
        })
    }

    /// The default production canonical origin (`https://api.typesafe.ai`).
    pub fn production() -> Self {
        Self::parse(DEFAULT_TYPESAFE_ENDPOINT).expect("default production origin is valid")
    }

    /// Parse and canonicalize an endpoint from a configuration override.
    pub fn from_override(
        endpoint: &crate::config::EndpointOverride,
    ) -> Result<Self, EndpointError> {
        Self::parse(endpoint.as_str())
    }

    /// Returns the canonical origin string representation (e.g. `https://api.typesafe.ai`).
    pub fn as_str(&self) -> &str {
        &self.canonical
    }

    /// Returns the origin key for cache and attempt-allowance scope identity.
    pub fn origin_key(&self) -> &str {
        &self.canonical
    }

    pub const fn scheme(&self) -> Scheme {
        self.scheme
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub const fn port(&self) -> Option<u16> {
        self.port
    }

    /// Whether this origin uses HTTPS.
    pub const fn is_secure(&self) -> bool {
        matches!(self.scheme, Scheme::Https)
    }

    /// Whether this origin points to a loopback interface.
    pub fn is_loopback(&self) -> bool {
        is_loopback_host(&self.host)
    }

    /// Append `/v1/systemone` exactly once to form the target URL.
    pub fn join_systemone(&self) -> TargetUrl {
        TargetUrl::new(self.clone())
    }
}

impl fmt::Display for CanonicalOrigin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.canonical)
    }
}

impl fmt::Debug for CanonicalOrigin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CanonicalOrigin(\"{}\")", self.canonical)
    }
}

/// The target API endpoint URL with `/v1/systemone` joined exactly once.
#[derive(Clone, Eq, PartialEq, Hash)]
pub struct TargetUrl {
    origin: CanonicalOrigin,
    url: String,
}

impl TargetUrl {
    fn new(origin: CanonicalOrigin) -> Self {
        let url = format!("{}{}", origin.as_str(), SYSTEMONE_PATH);
        Self { origin, url }
    }

    pub fn as_str(&self) -> &str {
        &self.url
    }

    pub fn origin(&self) -> &CanonicalOrigin {
        &self.origin
    }
}

impl fmt::Display for TargetUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.url)
    }
}

impl fmt::Debug for TargetUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TargetUrl(\"{}\")", self.url)
    }
}

/// High-level endpoint configuration holding validated origin and target URL.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EndpointConfig {
    origin: CanonicalOrigin,
    target: TargetUrl,
}

impl EndpointConfig {
    pub fn from_base_origin_str(raw: &str) -> Result<Self, EndpointError> {
        let origin = CanonicalOrigin::parse(raw)?;
        let target = origin.join_systemone();
        Ok(Self { origin, target })
    }

    pub fn production() -> Self {
        let origin = CanonicalOrigin::production();
        let target = origin.join_systemone();
        Self { origin, target }
    }

    /// Construct endpoint configuration from an endpoint override.
    pub fn from_override(
        endpoint: &crate::config::EndpointOverride,
    ) -> Result<Self, EndpointError> {
        Self::from_base_origin_str(endpoint.as_str())
    }

    pub fn origin(&self) -> &CanonicalOrigin {
        &self.origin
    }

    pub fn target_url(&self) -> &TargetUrl {
        &self.target
    }
}

impl Default for EndpointConfig {
    fn default() -> Self {
        Self::production()
    }
}

/// An `ApiCredential` bound to a specific `CanonicalOrigin`.
///
/// Guarantees that credentials can only be routed to the exact origin for which
/// they were provisioned, and strictly prevents sending credentials over unencrypted HTTP.
pub struct OriginScopedCredential {
    origin: CanonicalOrigin,
    credential: ApiCredential,
}

impl OriginScopedCredential {
    /// Bind an API credential to a canonical origin.
    ///
    /// # Errors
    /// Returns `CredentialRoutingError::InsecureHttpForbidden` if the origin is unencrypted HTTP.
    pub fn bind(
        credential: ApiCredential,
        origin: &CanonicalOrigin,
    ) -> Result<Self, CredentialRoutingError> {
        if !origin.is_secure() {
            return Err(CredentialRoutingError::InsecureHttpForbidden {
                origin: origin.to_string(),
            });
        }
        Ok(Self {
            origin: origin.clone(),
            credential,
        })
    }

    pub fn origin(&self) -> &CanonicalOrigin {
        &self.origin
    }

    /// Obtain the Authorization header value (`Bearer <token>`) for a request to `target_origin`.
    ///
    /// # Errors
    /// Returns `CredentialRoutingError::OriginMismatch` if `target_origin` does not match the bound origin.
    pub fn authorization_header_for(
        &self,
        target_origin: &CanonicalOrigin,
    ) -> Result<String, CredentialRoutingError> {
        if target_origin != &self.origin {
            return Err(CredentialRoutingError::OriginMismatch {
                expected: self.origin.to_string(),
                actual: target_origin.to_string(),
            });
        }
        Ok(format!(
            "Bearer {}",
            self.credential.expose_for_authorization_header()
        ))
    }
}

impl fmt::Debug for OriginScopedCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "OriginScopedCredential {{ origin: \"{}\", credential: <redacted> }}",
            self.origin
        )
    }
}

impl fmt::Display for OriginScopedCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "OriginScopedCredential(<redacted> for {})", self.origin)
    }
}

/// Errors occurring during credential binding and routing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CredentialRoutingError {
    /// Credentials cannot be sent over unencrypted HTTP.
    InsecureHttpForbidden { origin: String },
    /// Attempted to route credentials to an origin differing from the bound origin.
    OriginMismatch { expected: String, actual: String },
}

impl CredentialRoutingError {
    pub const fn kind(&self) -> ErrorKind {
        match self {
            Self::InsecureHttpForbidden { .. } => ErrorKind::InvalidConfiguration,
            Self::OriginMismatch { .. } => ErrorKind::Authentication,
        }
    }
}

impl fmt::Display for CredentialRoutingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InsecureHttpForbidden { origin } => write!(
                f,
                "refusing to bind credentials to unencrypted HTTP origin '{origin}'; loopback HTTP is credential-free"
            ),
            Self::OriginMismatch { expected, actual } => write!(
                f,
                "credential origin mismatch: credential is bound to '{expected}' but request target is '{actual}'"
            ),
        }
    }
}

impl std::error::Error for CredentialRoutingError {}

/// Strict redirect policy: all redirects are prohibited.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RedirectPolicy;

impl RedirectPolicy {
    /// Check response HTTP status code and location header.
    ///
    /// Rejects any 3xx redirect status code to prevent routing requests or credentials
    /// to unintended destinations or attacker servers.
    pub fn validate_response_status(
        status_code: u16,
        location_header: Option<&str>,
    ) -> Result<(), RedirectError> {
        if (300..=399).contains(&status_code) {
            let sanitized_location = location_header.map(sanitize_url_for_diagnostics);
            Err(RedirectError::RedirectForbidden {
                status_code,
                location: sanitized_location,
            })
        } else {
            Ok(())
        }
    }
}

/// Errors raised when an HTTP redirect is encountered.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RedirectError {
    RedirectForbidden {
        status_code: u16,
        location: Option<String>,
    },
}

impl RedirectError {
    pub const fn kind(&self) -> ErrorKind {
        ErrorKind::ProviderFailure
    }
}

impl fmt::Display for RedirectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RedirectForbidden {
                status_code,
                location,
            } => {
                if let Some(loc) = location {
                    write!(
                        f,
                        "HTTP redirect ({status_code}) to '{loc}' refused; redirects are strictly disabled by policy"
                    )
                } else {
                    write!(
                        f,
                        "HTTP redirect ({status_code}) refused; redirects are strictly disabled by policy"
                    )
                }
            }
        }
    }
}

impl std::error::Error for RedirectError {}

/// Explicit proxy policies and ambient proxy scrubbing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProxyPolicy {
    /// Direct connection only; ambient proxy environment variables are ignored.
    DirectOnly,
    /// Explicitly authorized proxy configuration.
    Explicit {
        host: String,
        port: u16,
        is_secure: bool,
    },
}

impl ProxyPolicy {
    /// Detect if any ambient proxy environment variables are present in an iterator.
    ///
    /// Returns a list of detected variable names and their sanitized values (credentials scrubbed).
    pub fn inspect_ambient_environment<'a, I>(env_vars: I) -> Vec<(&'static str, String)>
    where
        I: IntoIterator<Item = (&'a str, &'a str)>,
    {
        let mut detected = Vec::new();
        for (key, val) in env_vars {
            for &known_proxy in AMBIENT_PROXY_VARS {
                if key == known_proxy && !val.is_empty() {
                    detected.push((known_proxy, sanitize_url_for_diagnostics(val)));
                }
            }
        }
        detected
    }

    /// Detect if any ambient proxy environment variables are present in the process environment.
    pub fn inspect_ambient_process_env() -> Vec<(&'static str, String)> {
        let mut detected = Vec::new();
        for &known_proxy in AMBIENT_PROXY_VARS {
            if let Ok(val) = std::env::var(known_proxy) {
                if !val.is_empty() {
                    detected.push((known_proxy, sanitize_url_for_diagnostics(&val)));
                }
            }
        }
        detected
    }
}

/// Remove userinfo credentials and query strings from URLs before logging in diagnostics.
pub fn sanitize_url_for_diagnostics(url: &str) -> String {
    let (prefix, rest) = if let Some(colon_slash) = url.find("://") {
        let scheme_end = colon_slash + 3;
        (&url[..scheme_end], &url[scheme_end..])
    } else {
        ("", url)
    };

    let (authority, remainder) = match rest.find('/') {
        Some(slash) => (&rest[..slash], &rest[slash..]),
        None => (rest, ""),
    };

    let sanitized_authority = if let Some(at_idx) = authority.find('@') {
        format!("[REDACTED]@{}", &authority[at_idx + 1..])
    } else {
        authority.to_owned()
    };

    let sanitized_remainder = if let Some(q_idx) = remainder.find('?') {
        format!("{}[QUERY-REDACTED]", &remainder[..q_idx])
    } else {
        remainder.to_owned()
    };

    format!("{prefix}{sanitized_authority}{sanitized_remainder}")
}

/// Errors encountered while validating and canonicalizing endpoints.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EndpointError {
    EmptyInput,
    InputTooLong(usize),
    MissingScheme,
    UnsupportedScheme(String),
    InsecureScheme { scheme: String, host: String },
    UserinfoForbidden,
    QueryForbidden,
    FragmentForbidden,
    NonRootPathForbidden { path: String },
    DuplicatePathJoin { path: String },
    EmptyHost,
    InvalidHost(String),
    InvalidPort(String),
    PunycodeEncodingError(String),
}

impl EndpointError {
    pub const fn kind(&self) -> ErrorKind {
        ErrorKind::InvalidConfiguration
    }
}

impl fmt::Display for EndpointError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyInput => f.write_str("endpoint base origin cannot be empty"),
            Self::InputTooLong(len) => {
                write!(f, "endpoint base origin length ({len} bytes) exceeds limit ({MAX_ENDPOINT_INPUT_BYTES})")
            }
            Self::MissingScheme => f.write_str("endpoint base origin must include scheme (e.g. 'https://')"),
            Self::UnsupportedScheme(scheme) => {
                write!(f, "unsupported scheme '{scheme}'; only HTTPS (and loopback HTTP) is permitted")
            }
            Self::InsecureScheme { scheme, host } => {
                write!(f, "insecure scheme '{scheme}://' for non-loopback host '{host}'; HTTPS is required")
            }
            Self::UserinfoForbidden => {
                f.write_str("userinfo in endpoint URL is forbidden; credentials must be provided via TYPESAFE_API_KEY")
            }
            Self::QueryForbidden => f.write_str("query string in base origin is forbidden"),
            Self::FragmentForbidden => f.write_str("fragment in base origin is forbidden"),
            Self::NonRootPathForbidden { path } => {
                write!(f, "base origin must have root path, found '{path}'; TYPESAFE_ENDPOINT is an origin only")
            }
            Self::DuplicatePathJoin { path } => {
                write!(
                    f,
                    "endpoint base origin already contains '{path}'; TYPESAFE_ENDPOINT must be an origin without /v1/systemone"
                )
            }
            Self::EmptyHost => f.write_str("endpoint host cannot be empty"),
            Self::InvalidHost(reason) => write!(f, "invalid host in base origin: {reason}"),
            Self::InvalidPort(port_str) => write!(f, "invalid port '{port_str}' in base origin"),
            Self::PunycodeEncodingError(reason) => write!(f, "IDN punycode encoding error: {reason}"),
        }
    }
}

impl std::error::Error for EndpointError {}

// --- Internal helpers ---

fn parse_authority(authority: &str) -> Result<(&str, Option<u16>), EndpointError> {
    if let Some(stripped) = authority.strip_prefix('[') {
        // IPv6 authority: [addr]:port or [addr]
        let close_bracket = stripped.find(']').ok_or_else(|| {
            EndpointError::InvalidHost("unclosed IPv6 bracket in authority".to_owned())
        })?;
        let ipv6_str = &stripped[..close_bracket];
        let after_bracket = &stripped[close_bracket + 1..];
        let port = if let Some(colon_port) = after_bracket.strip_prefix(':') {
            let p = u16::from_str(colon_port)
                .map_err(|_| EndpointError::InvalidPort(colon_port.to_owned()))?;
            if p == 0 {
                return Err(EndpointError::InvalidPort("0".to_owned()));
            }
            Some(p)
        } else if after_bracket.is_empty() {
            None
        } else {
            return Err(EndpointError::InvalidHost(format!(
                "unexpected trailing data after IPv6: '{after_bracket}'"
            )));
        };
        Ok((ipv6_str, port))
    } else if Ipv6Addr::from_str(authority).is_ok() {
        // Unbracketed IPv6 address without port
        Ok((authority, None))
    } else {
        // Standard hostname or IPv4
        match authority.rfind(':') {
            Some(colon_idx) => {
                let host = &authority[..colon_idx];
                let port_str = &authority[colon_idx + 1..];
                let port = u16::from_str(port_str)
                    .map_err(|_| EndpointError::InvalidPort(port_str.to_owned()))?;
                if port == 0 {
                    return Err(EndpointError::InvalidPort("0".to_owned()));
                }
                Ok((host, Some(port)))
            }
            None => Ok((authority, None)),
        }
    }
}

fn canonicalize_host(raw_host: &str) -> Result<(String, bool), EndpointError> {
    if raw_host.is_empty() {
        return Err(EndpointError::EmptyHost);
    }

    // Try parsing as IPv4
    if let Ok(ipv4) = Ipv4Addr::from_str(raw_host) {
        let is_loopback = ipv4.is_loopback();
        return Ok((ipv4.to_string(), is_loopback));
    }

    // Try parsing as IPv6 (bracketed or unbracketed)
    let inner_ipv6 = raw_host
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(raw_host);
    if let Ok(ipv6) = Ipv6Addr::from_str(inner_ipv6) {
        let is_loopback = ipv6.is_loopback();
        // Canonical IPv6 in brackets
        return Ok((format!("[{ipv6}]"), is_loopback));
    }

    // Hostname canonicalization
    let is_loopback = raw_host.eq_ignore_ascii_case("localhost");

    // Strip trailing dot if present (DNS root zone notation, e.g. api.typesafe.ai. -> api.typesafe.ai)
    let host_to_split =
        if raw_host.ends_with('.') && raw_host.len() > 1 && !raw_host.ends_with("..") {
            &raw_host[..raw_host.len() - 1]
        } else {
            raw_host
        };

    let mut canonical_labels = Vec::new();
    for label in host_to_split.split('.') {
        if label.is_empty() {
            return Err(EndpointError::InvalidHost(
                "empty label in hostname".to_owned(),
            ));
        }

        // Check if label contains non-ASCII characters
        if label.is_ascii() {
            let lower = label.to_ascii_lowercase();
            validate_ascii_label(&lower)?;
            canonical_labels.push(lower);
        } else {
            // IDN Punycode encoding
            let puny =
                encode_punycode_label(label).map_err(EndpointError::PunycodeEncodingError)?;
            let idn_label = format!("xn--{puny}");
            validate_ascii_label(&idn_label)?;
            canonical_labels.push(idn_label);
        }
    }

    let canonical_hostname = canonical_labels.join(".");
    Ok((canonical_hostname, is_loopback))
}

fn validate_ascii_label(label: &str) -> Result<(), EndpointError> {
    if label.len() > 63 {
        return Err(EndpointError::InvalidHost(format!(
            "label '{label}' exceeds 63 characters"
        )));
    }
    if label.starts_with('-') || label.ends_with('-') {
        return Err(EndpointError::InvalidHost(format!(
            "label '{label}' cannot start or end with hyphen"
        )));
    }
    for c in label.chars() {
        if !c.is_ascii_alphanumeric() && c != '-' {
            return Err(EndpointError::InvalidHost(format!(
                "invalid character '{c}' in hostname label"
            )));
        }
    }
    Ok(())
}

fn is_loopback_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    if let Ok(ipv4) = Ipv4Addr::from_str(host) {
        return ipv4.is_loopback();
    }
    let inner = host
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(host);
    if let Ok(ipv6) = Ipv6Addr::from_str(inner) {
        return ipv6.is_loopback();
    }
    false
}

// RFC 3492 Punycode encoder for IDN domain labels
const BASE: u32 = 36;
const TMIN: u32 = 1;
const TMAX: u32 = 26;
const SKEW: u32 = 38;
const DAMP: u32 = 700;
const INITIAL_BIAS: u32 = 72;
const INITIAL_N: u32 = 128;

fn adapt_punycode(mut delta: u32, numpoints: u32, firsttime: bool) -> u32 {
    if firsttime {
        delta /= DAMP;
    } else {
        delta /= 2;
    }
    delta += delta / numpoints;
    let mut k = 0;
    while delta > ((BASE - TMIN) * TMAX) / 2 {
        delta /= BASE - TMIN;
        k += BASE;
    }
    k + (((BASE - TMIN + 1) * delta) / (delta + SKEW))
}

fn encode_digit(d: u32) -> char {
    match d {
        0..=25 => (b'a' + (d as u8)) as char,
        26..=35 => (b'0' + ((d - 26) as u8)) as char,
        _ => '0',
    }
}

pub fn encode_punycode_label(s: &str) -> Result<String, String> {
    let mut output = String::new();
    let mut basic_count = 0;
    for c in s.chars() {
        if c.is_ascii() {
            output.push(c.to_ascii_lowercase());
            basic_count += 1;
        }
    }

    let b = basic_count;
    let mut h = basic_count;
    if b > 0 {
        output.push('-');
    }

    let mut n = INITIAL_N;
    let mut delta: u32 = 0;
    let mut bias = INITIAL_BIAS;
    let total_chars = s.chars().count();

    while h < total_chars {
        let mut m = u32::MAX;
        for c in s.chars() {
            let cp = c as u32;
            if cp >= n && cp < m {
                m = cp;
            }
        }

        delta = delta
            .checked_add(
                (m - n)
                    .checked_mul((h as u32) + 1)
                    .ok_or_else(|| "punycode delta overflow".to_owned())?,
            )
            .ok_or_else(|| "punycode delta overflow".to_owned())?;
        n = m;

        for c in s.chars() {
            let cp = c as u32;
            if cp < n {
                delta = delta
                    .checked_add(1)
                    .ok_or_else(|| "punycode delta overflow".to_owned())?;
            }
            if cp == n {
                let mut q = delta;
                let mut k = BASE;
                loop {
                    let t = if k <= bias + TMIN {
                        TMIN
                    } else if k >= bias + TMAX {
                        TMAX
                    } else {
                        k - bias
                    };
                    if q < t {
                        break;
                    }
                    let digit = t + ((q - t) % (BASE - t));
                    output.push(encode_digit(digit));
                    q = (q - t) / (BASE - t);
                    k += BASE;
                }
                output.push(encode_digit(q));
                bias = adapt_punycode(delta, (h as u32) + 1, h == b);
                delta = 0;
                h += 1;
            }
        }
        delta += 1;
        n += 1;
    }

    Ok(output)
}
