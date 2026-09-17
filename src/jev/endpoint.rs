//! TypeSafe endpoint canonicalization, origin-scoped credential routing,
//! redirect prohibition, and proxy safety policies.
//!
//! # Embedded Invariants
//! - `TYPESAFE_ENDPOINT` is a trusted **base origin**, not a full path.
//! - Scheme must be HTTPS, with a test-only loopback HTTP exception.
//! - Equivalent spellings (trailing slash, default ports, case, IDN punycode) normalize to
//!   the exact same canonical origin identity string for cache and budget sharing.
//! - Userinfo, query strings, fragments, and non-root paths are rejected.
//! - Append `/v1/systemone` exactly once to form the target URL.
//! - Loopback HTTP is credential-free; routing an `ApiCredential` over insecure HTTP is forbidden.
//! - `OriginScopedCredential` binds credentials to a specific canonical origin;
//!   mismatched origins refuse to emit the `Authorization` header.
//! - No redirects: 3xx responses are hard failures; redirects are never followed.
//! - Ambient proxy variables (`HTTP_PROXY`, etc.) are not implicitly trusted,
//!   and proxy diagnostics scrub credentials, query strings, and fragments.
//! - Untrusted inputs (paths, ports, hosts, query tokens) are never echoed in error diagnostics.

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

/// Maximum characters for sanitized diagnostic URLs before truncation.
pub const MAX_DIAGNOSTIC_URL_CHARS: usize = 256;

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

        // Scheme extraction
        let (scheme, rest) = if let Some(colon_slash) = input.find("://") {
            let scheme_raw = &input[..colon_slash];
            let after_scheme = &input[colon_slash + 3..];
            if scheme_raw.eq_ignore_ascii_case("https") {
                (Scheme::Https, after_scheme)
            } else if scheme_raw.eq_ignore_ascii_case("http") {
                (Scheme::Http, after_scheme)
            } else {
                return Err(EndpointError::UnsupportedScheme);
            }
        } else {
            return Err(EndpointError::MissingScheme);
        };

        // Authority ends at the FIRST of '/', '?', or '#'
        let auth_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let authority = &rest[..auth_end];
        let after_auth = &rest[auth_end..];

        // Userinfo check (@ in authority)
        if authority.contains('@') {
            return Err(EndpointError::UserinfoForbidden);
        }

        // Query string check
        if after_auth.contains('?') {
            return Err(EndpointError::QueryForbidden);
        }

        // Fragment check
        if after_auth.contains('#') {
            return Err(EndpointError::FragmentForbidden);
        }

        // Validate path: must be empty or "/"
        if !after_auth.is_empty() && after_auth != "/" {
            if after_auth == "/v1/systemone"
                || after_auth == "/v1/systemone/"
                || after_auth == "/v1"
                || after_auth == "/v1/"
            {
                return Err(EndpointError::DuplicatePathJoin);
            }
            return Err(EndpointError::NonRootPathForbidden);
        }

        if authority.is_empty() {
            return Err(EndpointError::EmptyHost);
        }

        // Parse authority into host and optional port
        let (raw_host, explicit_port) = parse_authority(authority)?;

        // Canonicalize host
        let (canonical_host, is_loopback) = canonicalize_host(raw_host)?;

        // Scheme check: HTTP is permitted ONLY for loopback hosts
        if scheme == Scheme::Http && !is_loopback {
            return Err(EndpointError::InsecureScheme);
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

    /// Check if credentials can be routed to this origin.
    ///
    /// Refuses unencrypted HTTP origins to prevent cleartext credential leakage.
    pub fn allow_credential(
        &self,
        credential: &ApiCredential,
    ) -> Result<OriginScopedCredential, CredentialRoutingError> {
        OriginScopedCredential::bind(credential.clone(), self)
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
            return Err(CredentialRoutingError::InsecureHttpForbidden);
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
            return Err(CredentialRoutingError::OriginMismatch);
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
///
/// Contains no untrusted input strings or token data.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CredentialRoutingError {
    /// Credentials cannot be sent over unencrypted HTTP.
    InsecureHttpForbidden,
    /// Attempted to route credentials to an origin differing from the bound origin.
    OriginMismatch,
}

impl CredentialRoutingError {
    pub const fn kind(&self) -> ErrorKind {
        match self {
            Self::InsecureHttpForbidden => ErrorKind::InvalidConfiguration,
            Self::OriginMismatch => ErrorKind::Authentication,
        }
    }
}

impl fmt::Display for CredentialRoutingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InsecureHttpForbidden => {
                f.write_str("refusing to bind credentials to unencrypted HTTP origin; loopback HTTP is credential-free")
            }
            Self::OriginMismatch => {
                f.write_str("credential origin mismatch: request target does not match bound credential origin")
            }
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
            if let Ok(val) = std::env::var(known_proxy)
                && !val.is_empty()
            {
                detected.push((known_proxy, sanitize_url_for_diagnostics(&val)));
            }
        }
        detected
    }
}

/// Remove userinfo credentials, query strings, and fragments from URLs before logging in diagnostics.
///
/// Also strips control characters (preventing terminal escape injection) and bounds output length.
pub fn sanitize_url_for_diagnostics(url: &str) -> String {
    // 1. Separate scheme prefix if present
    let (scheme_prefix, after_scheme) = if let Some(colon_slash) = url.find("://") {
        let end = colon_slash + 3;
        (&url[..end], &url[end..])
    } else {
        ("", url)
    };

    // 2. Identify the end of authority (ends at first '/', '?', or '#')
    let auth_end = after_scheme
        .find(['/', '?', '#'])
        .unwrap_or(after_scheme.len());
    let authority = &after_scheme[..auth_end];
    let after_auth = &after_scheme[auth_end..];

    // 3. Userinfo in authority: use LAST '@' to prevent password '@' leakage
    let sanitized_auth = if let Some(at_idx) = authority.rfind('@') {
        let host_port = &authority[at_idx + 1..];
        format!("[REDACTED]@{host_port}")
    } else {
        authority.to_owned()
    };

    // 4. In after_auth, separate path, query, and fragment
    let (path, query_and_frag) = match after_auth.find(['?', '#']) {
        Some(idx) => (&after_auth[..idx], &after_auth[idx..]),
        None => (after_auth, ""),
    };

    let (query, fragment) = if let Some(frag_idx) = query_and_frag.find('#') {
        (&query_and_frag[..frag_idx], &query_and_frag[frag_idx..])
    } else {
        (query_and_frag, "")
    };

    let sanitized_query = if !query.is_empty() {
        "?[QUERY-REDACTED]"
    } else {
        ""
    };

    let sanitized_fragment = if !fragment.is_empty() {
        "#[FRAGMENT-REDACTED]"
    } else {
        ""
    };

    let combined =
        format!("{scheme_prefix}{sanitized_auth}{path}{sanitized_query}{sanitized_fragment}");

    // 5. Replace control characters with '?' to prevent terminal injection
    let mut safe = String::with_capacity(combined.len());
    for c in combined.chars() {
        if c.is_control() {
            safe.push('?');
        } else {
            safe.push(c);
        }
    }

    // 6. Bound length to prevent diagnostic log floods
    if safe.chars().count() > MAX_DIAGNOSTIC_URL_CHARS {
        let mut truncated: String = safe.chars().take(MAX_DIAGNOSTIC_URL_CHARS).collect();
        truncated.push_str("...[TRUNCATED]");
        truncated
    } else {
        safe
    }
}

/// Errors encountered while validating and canonicalizing endpoints.
///
/// Contains NO raw untrusted user input strings (paths, ports, hosts, tokens)
/// to ensure diagnostic logging and Display never leak secrets.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EndpointError {
    EmptyInput,
    InputTooLong(usize),
    MissingScheme,
    UnsupportedScheme,
    InsecureScheme,
    UserinfoForbidden,
    QueryForbidden,
    FragmentForbidden,
    NonRootPathForbidden,
    DuplicatePathJoin,
    EmptyHost,
    InvalidHost,
    InvalidPort,
    NonAsciiHostForbidden,
    UnbracketedIpv6Forbidden,
    NumericIpv4AliasForbidden,
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
                write!(
                    f,
                    "endpoint base origin length ({len} bytes) exceeds limit ({MAX_ENDPOINT_INPUT_BYTES})"
                )
            }
            Self::MissingScheme => {
                f.write_str("endpoint base origin must include scheme (e.g. 'https://')")
            }
            Self::UnsupportedScheme => {
                f.write_str("unsupported scheme in base origin; only HTTPS (and loopback HTTP) is permitted")
            }
            Self::InsecureScheme => {
                f.write_str("insecure HTTP scheme is forbidden for non-loopback host; HTTPS is required")
            }
            Self::UserinfoForbidden => {
                f.write_str("userinfo in endpoint URL is forbidden; credentials must be provided via TYPESAFE_API_KEY")
            }
            Self::QueryForbidden => f.write_str("query string in base origin is forbidden"),
            Self::FragmentForbidden => f.write_str("fragment in base origin is forbidden"),
            Self::NonRootPathForbidden => {
                f.write_str("base origin must have root path ('/'); non-root paths are forbidden in TYPESAFE_ENDPOINT")
            }
            Self::DuplicatePathJoin => {
                f.write_str("endpoint base origin already contains '/v1/systemone'; TYPESAFE_ENDPOINT must be an origin without API path")
            }
            Self::EmptyHost => f.write_str("endpoint host cannot be empty"),
            Self::InvalidHost => {
                f.write_str("invalid host in base origin; host must be a valid DNS hostname or canonical IP")
            }
            Self::InvalidPort => {
                f.write_str("invalid port in base origin; port must be an integer between 1 and 65535 without leading plus or zero")
            }
            Self::NonAsciiHostForbidden => {
                f.write_str("raw non-ASCII hostnames are forbidden; IDN origins must be specified in ASCII punycode (RFC 3490/3492 xn--...)")
            }
            Self::UnbracketedIpv6Forbidden => {
                f.write_str("unbracketed IPv6 address is forbidden; IPv6 addresses must be enclosed in brackets (e.g. '[::1]')")
            }
            Self::NumericIpv4AliasForbidden => {
                f.write_str("numeric IPv4 aliases and hex/octal IP formats are forbidden; only canonical dotted-decimal IPv4 is permitted")
            }
        }
    }
}

impl std::error::Error for EndpointError {}

// --- Internal helpers ---

fn parse_authority(authority: &str) -> Result<(&str, Option<u16>), EndpointError> {
    if let Some(stripped) = authority.strip_prefix('[') {
        // IPv6 authority: [addr]:port or [addr]
        let close_bracket = stripped.find(']').ok_or(EndpointError::InvalidHost)?;
        let ipv6_host = &authority[..close_bracket + 2];
        let after_bracket = &stripped[close_bracket + 1..];
        let port = if let Some(colon_port) = after_bracket.strip_prefix(':') {
            Some(parse_strict_port(colon_port)?)
        } else if after_bracket.is_empty() {
            None
        } else {
            return Err(EndpointError::InvalidHost);
        };
        Ok((ipv6_host, port))
    } else {
        // Not bracketed IPv6. Check for colons.
        let colon_count = authority.chars().filter(|&c| c == ':').count();
        if colon_count > 1 {
            // Multiple colons without brackets: unbracketed IPv6 literal
            Err(EndpointError::UnbracketedIpv6Forbidden)
        } else if colon_count == 1 {
            let colon_idx = authority.find(':').unwrap();
            let host = &authority[..colon_idx];
            let port_str = &authority[colon_idx + 1..];
            let port = parse_strict_port(port_str)?;
            Ok((host, Some(port)))
        } else {
            Ok((authority, None))
        }
    }
}

fn parse_strict_port(port_str: &str) -> Result<u16, EndpointError> {
    if port_str.is_empty() {
        return Err(EndpointError::InvalidPort);
    }
    // Reject leading '+' or '0' (prevents +443, 0443, 0)
    if port_str.starts_with('+') || port_str.starts_with('0') {
        return Err(EndpointError::InvalidPort);
    }
    if !port_str.chars().all(|c| c.is_ascii_digit()) {
        return Err(EndpointError::InvalidPort);
    }
    let port = u16::from_str(port_str).map_err(|_| EndpointError::InvalidPort)?;
    if port == 0 {
        return Err(EndpointError::InvalidPort);
    }
    Ok(port)
}

fn canonicalize_host(raw_host: &str) -> Result<(String, bool), EndpointError> {
    if raw_host.is_empty() {
        return Err(EndpointError::EmptyHost);
    }

    // Strict ASCII requirement: reject raw non-ASCII unicode
    if !raw_host.is_ascii() {
        return Err(EndpointError::NonAsciiHostForbidden);
    }

    // Try parsing as bracketed IPv6
    if raw_host.starts_with('[') && raw_host.ends_with(']') {
        let inner = &raw_host[1..raw_host.len() - 1];
        if let Ok(ipv6) = Ipv6Addr::from_str(inner) {
            let is_loopback = ipv6.is_loopback();
            return Ok((format!("[{ipv6}]"), is_loopback));
        } else {
            return Err(EndpointError::InvalidHost);
        }
    }

    // If host contains brackets elsewhere, reject
    if raw_host.contains('[') || raw_host.contains(']') {
        return Err(EndpointError::InvalidHost);
    }

    // Check if host is IPv4 dotted-decimal or numeric alias
    if looks_like_numeric_or_ip(raw_host) {
        return canonicalize_ipv4(raw_host);
    }

    // Hostname canonicalization
    let is_loopback = raw_host.eq_ignore_ascii_case("localhost");

    // Strip single trailing dot if present (DNS root zone, e.g. api.typesafe.ai. -> api.typesafe.ai)
    let host_to_split =
        if raw_host.ends_with('.') && raw_host.len() > 1 && !raw_host.ends_with("..") {
            &raw_host[..raw_host.len() - 1]
        } else {
            raw_host
        };

    let labels: Vec<&str> = host_to_split.split('.').collect();
    if labels.is_empty() {
        return Err(EndpointError::EmptyHost);
    }

    // Single-label host must be "localhost"
    if labels.len() == 1 && !is_loopback {
        return Err(EndpointError::InvalidHost);
    }

    // Top-level domain (last label) cannot be all numeric
    if let Some(tld) = labels.last()
        && tld.chars().all(|c| c.is_ascii_digit())
    {
        return Err(EndpointError::NumericIpv4AliasForbidden);
    }

    let mut canonical_labels = Vec::with_capacity(labels.len());
    for label in labels {
        if label.is_empty() {
            return Err(EndpointError::InvalidHost);
        }
        let lower = label.to_ascii_lowercase();
        validate_ascii_label(&lower)?;
        canonical_labels.push(lower);
    }

    let canonical_hostname = canonical_labels.join(".");
    Ok((canonical_hostname, is_loopback))
}

fn looks_like_numeric_or_ip(host: &str) -> bool {
    // If it starts with 0x/0X, or has all numeric labels, or contains only digits and dots
    if host.starts_with("0x") || host.starts_with("0X") {
        return true;
    }
    let parts: Vec<&str> = host.split('.').collect();
    // If every label is all digits or starts with 0x
    parts.iter().all(|p| {
        !p.is_empty()
            && (p.chars().all(|c| c.is_ascii_digit()) || p.starts_with("0x") || p.starts_with("0X"))
    })
}

fn canonicalize_ipv4(host: &str) -> Result<(String, bool), EndpointError> {
    // Hex, octal, or non-4-part numeric IP representations are forbidden
    if host.contains("0x") || host.contains("0X") {
        return Err(EndpointError::NumericIpv4AliasForbidden);
    }

    let parts: Vec<&str> = host.split('.').collect();
    if parts.len() != 4 {
        return Err(EndpointError::NumericIpv4AliasForbidden);
    }

    let mut octets = [0u8; 4];
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() || part.len() > 3 || !part.chars().all(|c| c.is_ascii_digit()) {
            return Err(EndpointError::NumericIpv4AliasForbidden);
        }
        // Disallow leading zeros (octal ambiguity, e.g. 01, 0177, 00)
        if part.len() > 1 && part.starts_with('0') {
            return Err(EndpointError::NumericIpv4AliasForbidden);
        }
        let val = u32::from_str(part).map_err(|_| EndpointError::NumericIpv4AliasForbidden)?;
        if val > 255 {
            return Err(EndpointError::InvalidHost);
        }
        octets[i] = val as u8;
    }

    let ipv4 = Ipv4Addr::new(octets[0], octets[1], octets[2], octets[3]);
    let is_loopback = ipv4.is_loopback();
    Ok((ipv4.to_string(), is_loopback))
}

fn validate_ascii_label(label: &str) -> Result<(), EndpointError> {
    if label.is_empty() || label.len() > 63 {
        return Err(EndpointError::InvalidHost);
    }
    if label.starts_with('-') || label.ends_with('-') {
        return Err(EndpointError::InvalidHost);
    }
    for c in label.chars() {
        if !c.is_ascii_alphanumeric() && c != '-' {
            return Err(EndpointError::InvalidHost);
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
