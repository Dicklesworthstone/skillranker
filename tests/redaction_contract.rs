//! Synthetic scanner regressions; see THIRD_PARTY_NOTICES.md for provenance.
use skillranker::privacy::redaction::{
    EXCERPT_JOIN, MAX_INSPECTED_PAYLOAD_BYTES, MAX_REDACTED_FIELD_BYTES, REDACTION_MARKER,
    RedactionError, Redactor,
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
        // A quoted value keeps its quotes (sr-wx8t).
        (
            format!("api_key = \"{}\"", "s".repeat(100)),
            format!("api_key = \"{REDACTION_MARKER}\""),
        ),
        (
            "password='秘密 café phrase'".into(),
            format!("password='{REDACTION_MARKER}'"),
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
fn prefixed_key_names_and_provider_keys_are_private() {
    // `_` is a word character: a `\b` before the keyword skipped all of these.
    for (input, expected) in [
        ("OPENAI_API_KEY=abc123def", "OPENAI_API_KEY=[REDACTED]"),
        ("TYPESAFE_API_KEY=abc123def", "TYPESAFE_API_KEY=[REDACTED]"),
        (
            "export GITHUB_TOKEN=abc123def",
            "export GITHUB_TOKEN=[REDACTED]",
        ),
        ("DB_PASSWORD=hunter2", "DB_PASSWORD=[REDACTED]"),
        ("PGPASSWORD=hunter2", "PGPASSWORD=[REDACTED]"),
        ("client_secret: s3cr3t", "client_secret: [REDACTED]"),
        (
            r#"{"access_token":"ya29abc"}"#,
            r#"{"access_token":"[REDACTED]"}"#,
        ),
        (
            "key sk-ant-api03-abcdefghijklmnopqrstuvwx here",
            "key [REDACTED] here",
        ),
        (
            "key sk-proj-abcdefghijklmnopqrstuvwx here",
            "key [REDACTED] here",
        ),
        (
            "stripe sk_live_abcdefghijklmnop1234 here",
            "stripe [REDACTED] here",
        ),
        ("Authorization: Basic dXNlcjpwYXNz", "[REDACTED]"),
        ("postgres://user:p@ss@db/x", "postgres://[REDACTED]@db/x"),
    ] {
        let result = Redactor::default().redact_field(input).unwrap();
        assert_eq!(result.as_str(), expected, "{input}");
    }
    // Honest twins: counts and prose that only resemble keys stay readable.
    for text in [
        "max_tokens: 5",
        "input_tokens=120",
        "the secretary: Jane",
        "a basic introduction to caching",
        "tokenizer=bpe",
    ] {
        let result = Redactor::default().redact_field(text).unwrap();
        assert_eq!(result.as_str(), text);
    }
    // The payload scan treats a prefixed key's value as private too.
    assert!(matches!(
        Redactor::default().inspect_payload(br#"{"access_token":"ya29abc"}"#),
        Err(RedactionError::SecretsDetected { .. })
    ));
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
    assert_eq!(result.as_str(), "api_key='[REDACTED]'");
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
    // The visible join counts toward the limit: head `é`, join, tail `z`.
    let result = scanner.redact_field_excerpt("éabc界xyz", 5).unwrap();
    assert_eq!(result.as_str(), format!("é{EXCERPT_JOIN}z"));
    assert_eq!(result.omitted_scalars(), 6);
}

/// Whether `text` holds a piece of a redaction marker. The inputs below are
/// lowercase apart from their markers, so any capital left after removing
/// whole markers is a fragment.
fn has_marker_fragment(text: &str) -> bool {
    text.replace(REDACTION_MARKER, "")
        .chars()
        .any(|c| c.is_ascii_uppercase())
}

#[test]
fn no_cut_leaves_a_fragment_of_a_redaction_marker() {
    // A fragment such as `password=[RED` no longer reads as a redaction, and
    // the final payload scan takes it for a secret: the whole request fails.
    // Also: a cut must not strand `password=` before its marker, nor glue text
    // onto one (`[REDACTED]…`); the scan reads either as a secret value.
    let scanner = Redactor::default();
    let clean = |text: &str| {
        !has_marker_fragment(text)
            && scanner
                .inspect_payload(serde_json::json!({ "text": text }).to_string().as_bytes())
                .is_ok()
    };
    for raw in [
        "check password=supersecretvalue1234 then token=abcdefghij0123456789 end",
        r#"args {"api_key": "abcdefghijklmnopqrstuvwxyz0123", "note": "ok"} done"#,
    ] {
        truncations_stay_clean(&scanner, raw, &clean);
    }
}

fn truncations_stay_clean(scanner: &Redactor, raw: &str, clean: &dyn Fn(&str) -> bool) {
    let redacted = scanner.redact_field(raw).unwrap();
    assert!(redacted.redaction_count() > 0, "{}", redacted.as_str());
    let length = redacted.as_str().chars().count();
    for budget in 0..=length {
        let excerpt = scanner.redact_field_excerpt(raw, budget).unwrap();
        assert!(clean(excerpt.as_str()), "{budget}: {}", excerpt.as_str());
        assert!(excerpt.as_str().chars().count() <= budget);
        // The omission count is what was removed; the join is not source text.
        let join = if excerpt.as_str().contains(EXCERPT_JOIN) {
            EXCERPT_JOIN.chars().count()
        } else {
            0
        };
        assert_eq!(
            excerpt.omitted_scalars(),
            length - (excerpt.as_str().chars().count() - join),
            "{budget}: {}",
            excerpt.as_str()
        );
        let truncated = skillranker::context::head_tail_truncate(redacted.as_str(), budget);
        assert!(clean(&truncated), "{budget}: {truncated}");
        assert!(
            truncated.chars().count() <= budget.max(1),
            "{budget}: {truncated}"
        );
        if budget > 0 && budget < length {
            assert!(
                truncated.contains('…') || truncated.contains("chars omitted"),
                "a cut must stay visible: {budget}: {truncated}"
            );
        }
    }
}

#[test]
fn a_quoted_secret_keeps_its_quotes_so_redacted_text_passes_the_payload_scan() {
    // A live shadow refusal (sr-wx8t). Replacing a quoted value together with
    // its quotes turned `f(password="x")` into `f(password=[REDACTED])`, and
    // the scan read `[REDACTED])` as an unquoted secret. The whole request
    // was refused as SecretsDetected.
    let scanner = Redactor::default();
    for (input, secret, expected) in [
        (
            r#"f(password="hunter2")"#,
            "hunter2",
            r#"f(password="[REDACTED]")"#,
        ),
        (
            r#"`"api_key": "ok"`."#,
            "\"ok\"",
            r#"`"api_key": "[REDACTED]"`."#,
        ),
        (
            "call(token='xyz789').then()",
            "xyz789",
            "call(token='[REDACTED]').then()",
        ),
        (
            r#"{"secret": "abc"}, next"#,
            "abc",
            r#"{"secret": "[REDACTED]"}, next"#,
        ),
    ] {
        let first = scanner.redact_field(input).unwrap();
        assert_eq!(first.as_str(), expected, "{input}");
        assert!(!first.as_str().contains(secret), "{input}");
        let again = scanner.redact_field(first.as_str()).unwrap();
        assert_eq!(
            again.as_str(),
            first.as_str(),
            "redaction is idempotent: {input}"
        );
        assert_eq!(again.redaction_count(), 0, "{input}");
        let payload = serde_json::json!({ "text": first.as_str() }).to_string();
        assert!(
            scanner.inspect_payload(payload.as_bytes()).is_ok(),
            "redacted text must pass the scan: {}",
            first.as_str()
        );
    }
    // An unterminated quoted value runs to the end of the field and is still
    // replaced whole.
    assert_eq!(
        scanner.redact_field("password='abc").unwrap().as_str(),
        "password=[REDACTED]"
    );
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
