use skillranker::roster::{FrontmatterError, UsageKind, parse_skill_metadata};

const ALIASES: [(&str, &str, &str); 6] = [
    (
        "disable-model-invocation",
        "disable_model_invocation",
        "true",
    ),
    ("user-invocable", "user_invocable", "false"),
    ("usage", "usage_kind", "reference"),
    ("aliases", "alias", "[alternate]"),
    ("tags", "tag", "[testing]"),
    ("phases", "phase", "[planning]"),
];

#[test]
fn synonymous_keys_are_duplicate_definitions_in_either_order() {
    for (canonical, alias, value) in ALIASES {
        for (first, second) in [(canonical, alias), (alias, canonical)] {
            for second in [second.to_owned(), second.to_ascii_uppercase()] {
                let doc = format!("---\n{first}: {value}\n{second}: {value}\n---\n# Skill\n");
                assert_eq!(
                    parse_skill_metadata(doc.as_bytes()).unwrap_err(),
                    FrontmatterError::DuplicateKey,
                    "duplicate semantic field: {first}/{second}"
                );
            }
        }
    }
}

#[test]
fn conflicting_restrictions_cannot_be_overwritten_by_aliases() {
    for (canonical, alias) in [
        ("disable-model-invocation", "disable_model_invocation"),
        ("user-invocable", "user_invocable"),
    ] {
        for (first, second) in [(canonical, alias), (alias, canonical)] {
            for (a, b) in [("true", "false"), ("false", "true")] {
                let doc = format!("---\n{first}: {a}\n{second}: {b}\n---\n# Skill\n");
                assert_eq!(
                    parse_skill_metadata(doc.as_bytes()).unwrap_err(),
                    FrontmatterError::DuplicateKey
                );
            }
        }
    }
}

#[test]
fn each_supported_spelling_retains_its_meaning() {
    for (canonical, alias, value) in ALIASES {
        for key in [canonical, alias] {
            let doc = format!("---\n{key}: {value}\n---\n# Skill\n");
            let parsed = parse_skill_metadata(doc.as_bytes()).unwrap();
            match canonical {
                "disable-model-invocation" => assert!(!parsed.agent_invocable),
                "user-invocable" => assert!(!parsed.user_invocable),
                "usage" => assert_eq!(parsed.usage_kind, UsageKind::Reference),
                "aliases" => assert_eq!(parsed.aliases, ["alternate"]),
                "tags" => assert_eq!(parsed.tags[0].as_str(), "testing"),
                "phases" => assert_eq!(parsed.phases[0].as_str(), "planning"),
                _ => unreachable!(),
            }
        }
    }
}

#[test]
fn unknown_keys_are_not_canonicalized_as_known_aliases() {
    let parsed = parse_skill_metadata(
        b"---\ncustom-key: one\ncustom_key: two\nuser-invocable: false\n---\n# Skill\n",
    )
    .unwrap();
    assert!(!parsed.user_invocable);
    let err =
        parse_skill_metadata(b"---\ncustom: private-canary\nCUSTOM: other\n---\n").unwrap_err();
    assert_eq!(err, FrontmatterError::DuplicateKey);
    assert!(!format!("{err:?} {err}").contains("private-canary"));
}

#[test]
fn dynamic_markers_preserve_reference_reuse_safety_and_ascii_digit_rules() {
    for (body, dynamic) in [
        ("Ordinary reusable guidance", false),
        ("$", false),
        ("$x $é1 $９ $١", false),
        ("ARGUMENTS CLAUDE_ command", false),
        ("$0", true),
        ("$9", true),
        ("$$5", true),
        ("$123", true),
        ("Use $ARGUMENTS here", true),
        ("Use ${CLAUDE_SESSION_ID} here", true),
        ("Read !`command` here", true),
        ("Unicode λ🙂 before $7 and a trailing $", true),
    ] {
        let doc = format!("---\ndescription: Reference\nusage: reference\n---\n{body}");
        let parsed = parse_skill_metadata(doc.as_bytes()).unwrap();
        assert_eq!(parsed.dynamic_content, dynamic, "body: {body}");
        assert_eq!(parsed.usage_kind, UsageKind::Reference);
    }
}

#[test]
fn dynamic_markers_at_the_full_file_boundary_are_not_lost_to_excerpting() {
    use skillranker::roster::MAX_SKILL_FILE_BYTES;
    let header = "---\ndescription: Reference\nusage: reference\n---\n";
    for (tail, dynamic) in [
        ("$8", true),
        ("$ARGUMENTS", true),
        ("$９", false),
        ("$", false),
    ] {
        let doc = format!(
            "{header}{}{tail}",
            "x".repeat(MAX_SKILL_FILE_BYTES - header.len() - tail.len())
        );
        assert_eq!(doc.len(), MAX_SKILL_FILE_BYTES);
        let parsed = parse_skill_metadata(doc.as_bytes()).unwrap();
        assert_eq!(parsed.dynamic_content, dynamic, "tail: {tail}");
    }
}
