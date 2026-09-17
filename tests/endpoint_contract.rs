//! Integration tests for TypeSafe endpoint canonicalization, origin-scoped
//! credential routing, redirect prohibition, and proxy variable safety.
//!
//! Satisfies contract boundary `p1_canonical_origin` (sr-roadmap-l1i.2.4)
//! mapped in `tests/contract_matrix.toml`.

use skillranker::config::{ConfigSources, ResolvedConfig};
use skillranker::jev::{
    CanonicalOrigin, CredentialRoutingError, EndpointConfig, EndpointError,
    OriginScopedCredential, ProxyPolicy, RedirectError, RedirectPolicy, Scheme,
    AMBIENT_PROXY_VARS, DEFAULT_TYPESAFE_ENDPOINT, SKILLRANKER_USER_AGENT, SYSTEMONE_PATH,
};
use skillranker::privacy::ApiCredential;
use std::ffi::OsString;

/// Synthetic canary secret token used to verify that no diagnostics, error messages,
/// or formatting strings ever leak credentials.
const CANARY_SECRET: &str = "canary-sk-secret-token-xyz9876543210";

fn canary_credential() -> ApiCredential {
    let sources = ConfigSources {
        environment: vec![(
            OsString::from("TYPESAFE_API_KEY"),
            OsString::from(CANARY_SECRET),
        )],
        ..Default::default()
    };
    let config = ResolvedConfig::resolve(sources, 1).expect("valid config with canary key");
    config
        .credential()
        .expect("credential present from environment")
        .clone()
}

fn assert_canary_not_leaked(text: &str) {
    assert!(
        !text.contains(CANARY_SECRET),
        "security violation: secret token leaked in diagnostic: {text}"
    );
}

// -----------------------------------------------------------------------------
// 1. Endpoint Canonicalization Policy
// -----------------------------------------------------------------------------

#[test]
fn endpoint_canonicalization() {
    // Default production endpoint
    let prod = CanonicalOrigin::production();
    assert_eq!(prod.as_str(), DEFAULT_TYPESAFE_ENDPOINT);
    assert_eq!(prod.origin_key(), "https://api.typesafe.ai");
    assert_eq!(prod.scheme(), Scheme::Https);
    assert_eq!(prod.host(), "api.typesafe.ai");
    assert_eq!(prod.port(), None);
    assert!(prod.is_secure());
    assert!(!prod.is_loopback());

    // Equivalent spellings must normalize to identical canonical representations
    let variants = [
        "https://api.typesafe.ai",
        "https://api.typesafe.ai/",
        "https://api.typesafe.ai:443",
        "https://api.typesafe.ai:443/",
        "HTTPS://API.TypeSafe.AI",
        "HTTPS://API.TypeSafe.AI/",
        "HTTPS://API.TypeSafe.AI:443/",
        "https://api.typesafe.ai.",
        "https://api.typesafe.ai./",
        "https://api.typesafe.ai.:443/",
    ];

    for variant in &variants {
        let parsed = CanonicalOrigin::parse(variant)
            .unwrap_or_else(|e| panic!("failed to parse variant '{variant}': {e}"));
        assert_eq!(
            parsed, prod,
            "variant '{variant}' did not normalize to production canonical origin"
        );
        assert_eq!(
            parsed.as_str(),
            prod.as_str(),
            "variant '{variant}' as_str mismatch"
        );
        assert_eq!(
            parsed.origin_key(),
            prod.origin_key(),
            "variant '{variant}' origin_key mismatch"
        );
    }

    // Explicit non-default port is preserved in canonical string
    let with_port = CanonicalOrigin::parse("https://api.typesafe.ai:8443/").unwrap();
    assert_eq!(with_port.as_str(), "https://api.typesafe.ai:8443");
    assert_eq!(with_port.port(), Some(8443));
    assert_ne!(with_port, prod);

    // IDN Punycode normalization
    let idn = CanonicalOrigin::parse("https://bücher.example.com/").unwrap();
    assert_eq!(idn.as_str(), "https://xn--bcher-kva.example.com");
    assert_eq!(idn.host(), "xn--bcher-kva.example.com");

    // Loopback IPv4
    let loopback_ipv4 = CanonicalOrigin::parse("http://127.0.0.1:8080/").unwrap();
    assert_eq!(loopback_ipv4.as_str(), "http://127.0.0.1:8080");
    assert_eq!(loopback_ipv4.scheme(), Scheme::Http);
    assert_eq!(loopback_ipv4.port(), Some(8080));
    assert!(!loopback_ipv4.is_secure());
    assert!(loopback_ipv4.is_loopback());

    // Loopback IPv4 default port 80 stripped
    let loopback_default_port = CanonicalOrigin::parse("http://127.0.0.1:80/").unwrap();
    assert_eq!(loopback_default_port.as_str(), "http://127.0.0.1");
    assert_eq!(loopback_default_port.port(), None);

    // Loopback IPv6 (bracketed)
    let loopback_ipv6 = CanonicalOrigin::parse("http://[::1]:8000/").unwrap();
    assert_eq!(loopback_ipv6.as_str(), "http://[::1]:8000");
    assert!(loopback_ipv6.is_loopback());

    // Loopback IPv6 (unbracketed without port)
    let loopback_unbracketed = CanonicalOrigin::parse("http://::1/").unwrap();
    assert_eq!(loopback_unbracketed.as_str(), "http://[::1]");
    assert!(loopback_unbracketed.is_loopback());

    // Loopback localhost
    let localhost = CanonicalOrigin::parse("http://localhost:3000/").unwrap();
    assert_eq!(localhost.as_str(), "http://localhost:3000");
    assert!(localhost.is_loopback());

    // User agent is non-empty and contains crate name
    assert!(SKILLRANKER_USER_AGENT.starts_with("skillranker/"));
}

// -----------------------------------------------------------------------------
// 2. Target URL and Path Joining
// -----------------------------------------------------------------------------

#[test]
fn target_url_and_path_joining() {
    let origin = CanonicalOrigin::production();
    let target = origin.join_systemone();
    assert_eq!(target.as_str(), "https://api.typesafe.ai/v1/systemone");
    assert_eq!(target.origin(), &origin);
    assert_eq!(format!("{target}"), "https://api.typesafe.ai/v1/systemone");
    assert_eq!(SYSTEMONE_PATH, "/v1/systemone");

    // EndpointConfig default is production with joined systemone
    let cfg = EndpointConfig::default();
    assert_eq!(cfg.origin(), &origin);
    assert_eq!(cfg.target_url(), &target);

    // Duplicate path joins rejected
    let duplicate_paths = [
        "https://api.typesafe.ai/v1/systemone",
        "https://api.typesafe.ai/v1/systemone/",
        "https://api.typesafe.ai/v1",
        "https://api.typesafe.ai/v1/",
    ];
    for dup in &duplicate_paths {
        let err = CanonicalOrigin::parse(dup).unwrap_err();
        assert!(
            matches!(err, EndpointError::DuplicatePathJoin { .. }),
            "expected DuplicatePathJoin for '{dup}', got: {err:?}"
        );
    }

    // Non-root paths rejected
    let non_root = [
        "https://api.typesafe.ai/api",
        "https://api.typesafe.ai/custom/path",
        "https://api.typesafe.ai/eval",
    ];
    for nr in &non_root {
        let err = CanonicalOrigin::parse(nr).unwrap_err();
        assert!(
            matches!(err, EndpointError::NonRootPathForbidden { .. }),
            "expected NonRootPathForbidden for '{nr}', got: {err:?}"
        );
    }
}

// -----------------------------------------------------------------------------
// 3. Userinfo, Query and Fragment Rejection
// -----------------------------------------------------------------------------

#[test]
fn userinfo_query_and_fragment_rejection_without_token_leak() {
    // Userinfo with canary password
    let userinfo_input = format!("https://user:{}@api.typesafe.ai/", CANARY_SECRET);
    let err = CanonicalOrigin::parse(&userinfo_input).unwrap_err();
    assert_eq!(err, EndpointError::UserinfoForbidden);
    assert_canary_not_leaked(&format!("{err}"));
    assert_canary_not_leaked(&format!("{err:?}"));

    // Query string with canary token
    let query_input = format!("https://api.typesafe.ai/?auth={}", CANARY_SECRET);
    let err = CanonicalOrigin::parse(&query_input).unwrap_err();
    assert_eq!(err, EndpointError::QueryForbidden);
    assert_canary_not_leaked(&format!("{err}"));
    assert_canary_not_leaked(&format!("{err:?}"));

    // Fragment with canary token
    let frag_input = format!("https://api.typesafe.ai/#{}", CANARY_SECRET);
    let err = CanonicalOrigin::parse(&frag_input).unwrap_err();
    assert_eq!(err, EndpointError::FragmentForbidden);
    assert_canary_not_leaked(&format!("{err}"));
    assert_canary_not_leaked(&format!("{err:?}"));

    // Empty input and input too long
    assert_eq!(CanonicalOrigin::parse(""), Err(EndpointError::EmptyInput));
    let huge = format!("https://api.typesafe.ai/{}", "x".repeat(3000));
    assert!(matches!(
        CanonicalOrigin::parse(&huge),
        Err(EndpointError::InputTooLong(_))
    ));
}

// -----------------------------------------------------------------------------
// 4. Scheme Enforcement and Insecure HTTP Restrictions
// -----------------------------------------------------------------------------

#[test]
fn scheme_enforcement_and_loopback_restriction() {
    // Insecure HTTP to non-loopback host is forbidden
    let err = CanonicalOrigin::parse("http://api.typesafe.ai").unwrap_err();
    assert!(
        matches!(err, EndpointError::InsecureScheme { .. }),
        "expected InsecureScheme, got: {err:?}"
    );

    // Unsupported schemes
    let bad_schemes = [
        "ftp://api.typesafe.ai",
        "ws://api.typesafe.ai",
        "wss://api.typesafe.ai",
        "file:///path",
    ];
    for bs in &bad_schemes {
        let err = CanonicalOrigin::parse(bs).unwrap_err();
        assert!(
            matches!(err, EndpointError::UnsupportedScheme(_)),
            "expected UnsupportedScheme for '{bs}', got: {err:?}"
        );
    }

    // Missing scheme
    let err = CanonicalOrigin::parse("api.typesafe.ai").unwrap_err();
    assert_eq!(err, EndpointError::MissingScheme);
}

// -----------------------------------------------------------------------------
// 5. Origin-Scoped Credential Routing and Isolation
// -----------------------------------------------------------------------------

#[test]
fn origin_scoped_credential_routing_and_isolation() {
    let credential = canary_credential();
    let prod_origin = CanonicalOrigin::production();
    let loopback_origin = CanonicalOrigin::parse("http://127.0.0.1:8080").unwrap();
    let other_origin = CanonicalOrigin::parse("https://other.typesafe.ai").unwrap();

    // 1. Binding credentials to unencrypted HTTP is strictly forbidden
    let insecure_err = OriginScopedCredential::bind(credential.clone(), &loopback_origin)
        .err()
        .expect("insecure HTTP binding must fail");
    assert!(
        matches!(
            insecure_err,
            CredentialRoutingError::InsecureHttpForbidden { .. }
        ),
        "expected InsecureHttpForbidden, got: {insecure_err:?}"
    );
    assert_canary_not_leaked(&format!("{insecure_err}"));

    // 2. Binding to canonical HTTPS origin succeeds
    let bound = OriginScopedCredential::bind(credential, &prod_origin)
        .expect("HTTPS origin binding must succeed");
    assert_eq!(bound.origin(), &prod_origin);

    // 3. Emitting header for the matching origin succeeds
    let auth_header = bound
        .authorization_header_for(&prod_origin)
        .expect("authorization header emission for exact origin must succeed");
    assert_eq!(auth_header, format!("Bearer {CANARY_SECRET}"));

    // 4. Request to another origin is strictly refused (OriginMismatch)
    let mismatch_err = bound
        .authorization_header_for(&other_origin)
        .err()
        .expect("mismatched origin must fail");
    assert!(
        matches!(
            mismatch_err,
            CredentialRoutingError::OriginMismatch { .. }
        ),
        "expected OriginMismatch, got: {mismatch_err:?}"
    );
    assert_canary_not_leaked(&format!("{mismatch_err}"));

    // 5. Debug and Display of OriginScopedCredential redact credentials
    let debug_str = format!("{bound:?}");
    let display_str = format!("{bound}");
    assert_canary_not_leaked(&debug_str);
    assert_canary_not_leaked(&display_str);
    assert!(debug_str.contains("<redacted>"));
    assert!(display_str.contains("<redacted>"));
}

// -----------------------------------------------------------------------------
// 6. Strict Redirect Prohibition and Attacker Redirection
// -----------------------------------------------------------------------------

#[test]
fn redirect_policy_prohibits_all_3xx_and_sanitizes_destinations() {
    // 3xx status codes must all fail
    let redirect_codes = [300, 301, 302, 303, 304, 307, 308];
    for &status in &redirect_codes {
        let err = RedirectPolicy::validate_response_status(status, None).unwrap_err();
        assert!(
            matches!(err, RedirectError::RedirectForbidden { status_code, .. } if status_code == status),
            "expected RedirectForbidden for {status}, got: {err:?}"
        );
    }

    // Attacker redirect location with credentials and secret parameters
    let attacker_loc = format!(
        "https://user:{}@attacker.evil.com/steal?token={}",
        CANARY_SECRET, CANARY_SECRET
    );
    let err = RedirectPolicy::validate_response_status(302, Some(&attacker_loc)).unwrap_err();
    let err_str = format!("{err}");
    let err_debug = format!("{err:?}");

    // Credentials and queries must be scrubbed in error output
    assert_canary_not_leaked(&err_str);
    assert_canary_not_leaked(&err_debug);
    assert!(err_str.contains("[REDACTED]@attacker.evil.com"));
    assert!(err_str.contains("[QUERY-REDACTED]"));

    // Successful non-redirect status codes
    assert!(RedirectPolicy::validate_response_status(200, None).is_ok());
    assert!(RedirectPolicy::validate_response_status(400, None).is_ok());
    assert!(RedirectPolicy::validate_response_status(401, None).is_ok());
    assert!(RedirectPolicy::validate_response_status(429, None).is_ok());
    assert!(RedirectPolicy::validate_response_status(500, None).is_ok());
}

// -----------------------------------------------------------------------------
// 7. Ambient Proxy Variables Inspection and Scrubbing
// -----------------------------------------------------------------------------

#[test]
fn ambient_proxy_variables_detected_and_sanitized() {
    // Verify all 8 ambient proxy variables are tracked
    assert_eq!(AMBIENT_PROXY_VARS.len(), 8);
    for &var_name in &[
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
        "NO_PROXY",
        "no_proxy",
    ] {
        assert!(
            AMBIENT_PROXY_VARS.contains(&var_name),
            "missing proxy variable {var_name}"
        );
    }

    // Proxy URL with basic auth credentials and query string
    let proxy_val = format!(
        "http://proxyuser:{}@proxy.internal.corp:8080/?token={}",
        CANARY_SECRET, CANARY_SECRET
    );

    let simulated_env = [
        ("PATH", "/usr/bin:/bin"),
        ("HTTPS_PROXY", proxy_val.as_str()),
        ("EMPTY_PROXY", ""),
        ("no_proxy", "localhost,127.0.0.1"),
    ];

    let detected = ProxyPolicy::inspect_ambient_environment(simulated_env);
    assert_eq!(detected.len(), 2);

    for (name, sanitized) in &detected {
        assert_canary_not_leaked(sanitized);
        if *name == "HTTPS_PROXY" {
            assert!(sanitized.contains("[REDACTED]@proxy.internal.corp:8080"));
            assert!(sanitized.contains("[QUERY-REDACTED]"));
        }
    }
}

// -----------------------------------------------------------------------------
// 8. Configuration Override Integration
// -----------------------------------------------------------------------------

#[test]
fn configuration_override_integration() {
    let sources = ConfigSources {
        environment: vec![(
            OsString::from("TYPESAFE_ENDPOINT"),
            OsString::from("https://staging.typesafe.ai:8443/"),
        )],
        ..Default::default()
    };
    let config = ResolvedConfig::resolve(sources, 1).expect("resolved config");
    let ep_override = config
        .effective()
        .endpoint()
        .expect("endpoint override present");

    let canonical = CanonicalOrigin::from_override(ep_override).expect("valid override");
    assert_eq!(canonical.as_str(), "https://staging.typesafe.ai:8443");
    assert_eq!(canonical.port(), Some(8443));

    let ep_config = EndpointConfig::from_override(ep_override).expect("valid config override");
    assert_eq!(
        ep_config.target_url().as_str(),
        "https://staging.typesafe.ai:8443/v1/systemone"
    );
}
