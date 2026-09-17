//! Synthetic scanner regressions; see THIRD_PARTY_NOTICES.md for provenance.
use skillranker::privacy::redaction::{
    MAX_INSPECTED_PAYLOAD_BYTES, MAX_REDACTED_FIELD_BYTES, REDACTION_MARKER, RedactionError,
    Redactor,
};

#[test]
fn incomplete_quoted_values_and_numeric_credentials_are_private() {
    let scanner = Redactor::default();
    for quote in ['\'', '"'] {
        let input = format!("password={quote}abc\\");
        assert_eq!(
            scanner.redact_field(&input).unwrap().as_str(),
            "password=[REDACTED]"
        );
    }
    for payload in [
        br#"{"password":1234}"#.as_slice(),
        br#"{"credential":[true,1234]}"#,
    ] {
        assert!(matches!(
            scanner.inspect_payload(payload),
            Err(RedactionError::SecretsDetected { .. })
        ));
    }
    scanner
        .inspect_payload(br#"{"count":1234,"enabled":true,"password":null}"#)
        .unwrap();
}

#[test]
fn ordinary_text_and_hashes_remain_usable() {
    let text = "Investigate café tests; revision a1b2c3d4e5f60718293a4b5c6d7e8f90.";
    let result = Redactor::default().redact_field(text).unwrap();
    assert_eq!(result.as_str(), text);
    assert_eq!(result.redaction_count(), 0);
}

#[test]
fn secret_families_remove_complete_values_preserving_neighbors() {
    let github = format!("ghp_{}", "a".repeat(36));
    let cases = [
        (
            "AKIAIOSFODNN7EXAMPLE".to_owned(),
            REDACTION_MARKER.to_owned(),
        ),
        (github, REDACTION_MARKER.to_owned()),
        (
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.Signature123".into(),
            REDACTION_MARKER.into(),
        ),
        (
            "Bearer abcdefghijklmnopqrstuvwxyz".into(),
            REDACTION_MARKER.into(),
        ),
        (
            "postgres://admin:hunter2@localhost/db".into(),
            format!("postgres://{REDACTION_MARKER}@localhost/db"),
        ),
        (
            format!("xoxb-{}-{}abcdef", "0123456789", "0123456789"),
            REDACTION_MARKER.into(),
        ),
        (
            format!("api_key = \"{}\"", "s".repeat(100)),
            format!("api_key = {REDACTION_MARKER}"),
        ),
        (
            "password='秘密 café phrase'".into(),
            format!("password={REDACTION_MARKER}"),
        ),
        (
            "credential=秘密値".into(),
            format!("credential={REDACTION_MARKER}"),
        ),
    ];
    for (input, expected) in cases {
        let result = Redactor::default()
            .redact_field(&format!("before {input} after"))
            .unwrap();
        assert_eq!(result.as_str(), format!("before {expected} after"));
        assert_eq!(result.redaction_count(), 1);
    }
}

#[test]
fn adjacent_aws_identifiers_are_both_removed() {
    let result = Redactor::default()
        .redact_field("AKIAIOSFODNN7EXAMPLE,ASIAIOSFODNN7EXAMPLE")
        .unwrap();
    assert_eq!(result.as_str(), "[REDACTED],[REDACTED]");
    assert_eq!(result.redaction_count(), 2);
}

#[test]
fn complete_and_unterminated_private_blocks_are_removed() {
    for label in [
        "RSA PRIVATE KEY",
        "OPENSSH PRIVATE KEY",
        "PGP PRIVATE KEY BLOCK",
    ] {
        let input =
            format!("before -----BEGIN {label}-----\nprivate body\n-----END {label}----- after");
        assert_eq!(
            Redactor::default().redact_field(&input).unwrap().as_str(),
            "before [REDACTED] after"
        );
        let partial = format!("before -----BEGIN {label}-----\nprivate body");
        assert_eq!(
            Redactor::default().redact_field(&partial).unwrap().as_str(),
            "before [REDACTED]"
        );
    }
}

#[test]
fn overlapping_assignment_and_token_count_once() {
    let input = format!("api_key='ghp_{}'", "b".repeat(36));
    let result = Redactor::default().redact_field(&input).unwrap();
    assert_eq!(result.as_str(), "api_key=[REDACTED]");
    assert_eq!(result.redaction_count(), 1);
}

#[test]
fn entropy_overlap_extends_beyond_known_token() {
    let token = format!(
        "ghp_{}_-Z0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz",
        "b".repeat(36)
    );
    let result = Redactor::with_entropy(true).redact_field(&token).unwrap();
    assert_eq!(result.as_str(), REDACTION_MARKER);
    assert_eq!(result.redaction_count(), 1);
}

#[test]
fn redaction_before_truncation() {
    let token = format!("ghp_{}", "q".repeat(36));
    let input = format!("é{token}界");
    let scanner = Redactor::default();
    for budget in 0..=13 {
        let result = scanner.redact_field_excerpt(&input, budget).unwrap();
        assert!(!result.as_str().contains('q'));
        assert_eq!(result.redaction_count(), 1);
        assert!(result.as_str().chars().count() <= budget);
    }
    let result = scanner.redact_field_excerpt("éabc界xyz", 5).unwrap();
    assert_eq!(result.omitted_scalars(), 3);
}

#[test]
fn repeated_scans_are_stable_and_debug_hides_private_prose() {
    let scanner = Redactor::default();
    let first = scanner
        .redact_field("private prose password='secret value'")
        .unwrap();
    let second = scanner.redact_field(first.as_str()).unwrap();
    assert_eq!(first.as_str(), second.as_str());
    assert_eq!(second.redaction_count(), 0);
    assert!(!format!("{first:?}").contains("private prose"));
}

#[test]
fn final_payload_checks_all_nested_fields_and_decodes_escapes() {
    let scanner = Redactor::default();
    for field in [
        "latest_user_request",
        "recent_messages",
        "tool_name",
        "arguments",
        "result",
        "description",
        "body_excerpt",
        "tags",
    ] {
        let secret = format!("ghp_{}", "z".repeat(36));
        let payload = serde_json::json!({"state": {field: [secret]}});
        assert!(matches!(
            scanner.inspect_payload(&serde_json::to_vec(&payload).unwrap()),
            Err(RedactionError::SecretsDetected { .. })
        ));
        let clean = serde_json::json!({"state": {field: [scanner.redact_field(&secret).unwrap().as_str()]}});
        scanner
            .inspect_payload(&serde_json::to_vec(&clean).unwrap())
            .unwrap();
    }
    let encoded = format!(r#"{{"description":"\u0067hp_{}"}}"#, "z".repeat(36));
    assert!(matches!(
        scanner.inspect_payload(encoded.as_bytes()),
        Err(RedactionError::SecretsDetected { .. })
    ));
    let key = format!(r#"{{"ghp_{}":null}}"#, "z".repeat(36));
    assert!(matches!(
        scanner.inspect_payload(key.as_bytes()),
        Err(RedactionError::SecretsDetected { .. })
    ));
    assert!(matches!(
        scanner.inspect_payload(br#"{"password":"short"}"#),
        Err(RedactionError::SecretsDetected { .. })
    ));
}

#[test]
fn payload_limits_duplicates_and_errors_are_safe() {
    let scanner = Redactor::default();
    for payload in [b"{private".as_slice(), br#"{"a":1,"a":2}"#, b"{} {}"] {
        let error = scanner.inspect_payload(payload).unwrap_err();
        assert_eq!(error, RedactionError::InvalidPayload);
        assert!(!format!("{error} {error:?}").contains("private"));
    }
    assert_eq!(
        scanner.inspect_payload(&vec![b' '; MAX_INSPECTED_PAYLOAD_BYTES + 1]),
        Err(RedactionError::PayloadTooLong)
    );
    assert_eq!(
        scanner
            .redact_field(&"x".repeat(MAX_REDACTED_FIELD_BYTES + 1))
            .unwrap_err(),
        RedactionError::FieldTooLong
    );
    let deep = format!("{}0{}", "[".repeat(66), "]".repeat(66));
    assert_eq!(
        scanner.inspect_payload(deep.as_bytes()),
        Err(RedactionError::InvalidPayload)
    );
    scanner
        .inspect_payload(br#"{"ordinary":["safe text",1,true,null]}"#)
        .unwrap();
}
